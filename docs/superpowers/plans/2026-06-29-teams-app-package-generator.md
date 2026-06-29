# Teams App-Package Generator Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let an operator download a ready-to-sideload Teams app package (`.zip` with a correct `manifest.json` + Spacebot icons) straight from the Channels settings UI, instead of hand-writing the manifest.

**Architecture:** A backend endpoint `GET /api/messaging/teams/app-package?app_id=<GUID>` builds the manifest (all the known-rejection gotchas baked in), renders the default Spacebot icons from the bundled `ball.png` via the `image` crate (resize to the two Teams-required sizes), zips the three files, and returns them as an `application/zip` download. The Teams app `id` (distinct from the bot App ID) is generated once and persisted in an instance-dir sidecar so re-downloads reuse the same id (re-upload updates the existing Teams app). The frontend adds a "Download Teams app package" link on the Teams channel card plus a link to the setup guide.

**Tech Stack:** Rust (`axum`, `zip`, `uuid`, `image`, `serde_json`), TypeScript/React (interface), fumadocs `.mdx`.

**Lands in:** branch `pr/teams-channels-ui` (upstream PR #608). All referenced files already exist there (the Teams Channels UI). Execute on that branch.

## Global Constraints

- **Manifest correctness (validated 2026-06-26 against real Teams — these reject the package if wrong):** `manifestVersion = "1.17"`; `id` = a plain GUID that is **distinct from** `bots[0].botId`; `bots[0].botId` = the bot **App ID**; **no `packageName` key** (schema 1.17 rejects it); `scopes` lowercase `["personal","team","groupchat"]`; `accentColor` and `validDomains` are required.
- **Icons (Teams requires exact dimensions):** `icon-color.png` = **192×192**, `icon-outline.png` = **32×32** (transparent monochrome). Default = the Spacebot logo (`interface/public/ball.png`, 384×384 RGBA). The doc must explain swapping in a custom icon.
- **The endpoint only needs `app_id`** (the manifest does not use the tenant id or the client secret — never put the secret in the package or URL).
- **`image` is already in `Cargo.lock`** (transitive); adding it as a direct dep resolves to the locked version (no new fetch).
- **Binary endpoints must annotate utoipa with `content_type`** (mirror `src/api/system.rs:backup_export`) so `just check-typegen` stays green. Adding the endpoint changes the OpenAPI spec → **regenerate `schema.d.ts`** (`just typegen`).
- **Gates:** `just gate-pr` must pass (clippy `-D warnings`, fmt, tests, typegen). Heavy cargo wrapped: `systemd-run --scope -p MemoryMax=40G -p MemorySwapMax=0 cargo …`. Trailer-free commits (verify `git log -1 --format=%B | grep -ciE "Co-Authored-By|Claude-Session|Generated with"` == 0). Do NOT push (controller pushes). Real-Teams re-test (sideload the generated package) is a post-merge step, not part of this plan.

---

## File Structure

- **Create** `src/api/teams_package.rs` — the manifest JSON builder, icon rendering (resize `ball.png`), zip assembly, and the persistent-manifest-id sidecar. Pure functions + one small IO helper, all unit-testable. (Keeps `api/messaging.rs` from growing; one clear responsibility.)
- **Modify** `src/api/messaging.rs` — add the `download_teams_app_package` axum handler (thin: parse query → call the builder → return zip). It already owns the Teams messaging endpoints.
- **Modify** `src/api.rs` — declare `mod teams_package;` (the api module tree lives in `src/api.rs`, NOT `src/api/mod.rs`; sibling of `mod messaging;` at line 20). Plain `mod` matches the file's convention; `crate::api::teams_package::…` is still reachable from the `messaging` submodule.
- **Modify** `src/api/server.rs` — register the route (`routes!(messaging::download_teams_app_package)`).
- **Modify** `Cargo.toml` — add `image` as a direct dependency.
- **Modify** `interface/src/api/client.ts` — add `teamsAppPackageUrl(appId)` URL helper.
- **Modify** `interface/src/components/ChannelSettingCard.tsx` — add the download link + setup-guide link in the Teams section.
- **Modify** `interface/src/api/schema.d.ts` — regenerated (not hand-edited).
- **Modify** `docs/content/docs/(messaging)/teams-setup.mdx` — Step 3: the button is the easy path; keep manual manifest + icon-swap instructions.

---

### Task 1: Manifest + icon + zip builder (`src/api/teams_package.rs`)

**Files:**
- Create: `src/api/teams_package.rs`
- Modify: `Cargo.toml` (add `image`), `src/api/mod.rs` (declare module)
- Test: inline `#[cfg(test)] mod tests` in `src/api/teams_package.rs`

**Interfaces:**
- Produces:
  - `pub fn teams_manifest_json(app_id: &str, manifest_id: &str) -> serde_json::Value`
  - `pub fn render_default_icons() -> anyhow::Result<(Vec<u8>, Vec<u8>)>` — `(color_192, outline_32)` PNG bytes from the bundled `ball.png`.
  - `pub fn build_app_package(app_id: &str, manifest_id: &str) -> anyhow::Result<Vec<u8>>` — the `.zip` bytes (`manifest.json` + `icon-color.png` + `icon-outline.png` at the archive root).
  - `pub fn load_or_create_manifest_id(instance_dir: &std::path::Path) -> String` — read `teams_manifest_id.json` or create+persist a new `uuid::Uuid::new_v4()` (atomic tmp+rename). (Used by Task 2's handler.)

- [ ] **Step 1: Add the `image` dependency**

In `Cargo.toml`, under the other deps (near `zip = "2"`):

```toml
# Icon resizing for the generated Teams app package (already in the lockfile transitively).
image = { version = "0.25", default-features = false, features = ["png"] }
```

Run `systemd-run --scope -p MemoryMax=40G -p MemorySwapMax=0 cargo check --lib 2>&1 | tail -5` — expect it to resolve `image` from the existing lock (no version bump). If the locked `image` major differs from `0.25`, set the version to match `grep -A1 'name = "image"' Cargo.lock` rather than forcing an upgrade.

- [ ] **Step 2: Write the failing tests**

Create `src/api/teams_package.rs` with only the tests first:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read as _;

    #[test]
    fn manifest_has_required_fields_and_no_packagename() {
        let m = teams_manifest_json("00000000-0000-0000-0000-000000000001", "ffffffff-0000-0000-0000-000000000002");
        assert_eq!(m["manifestVersion"], "1.17");
        assert_eq!(m["id"], "ffffffff-0000-0000-0000-000000000002");
        // id MUST differ from botId.
        assert_ne!(m["id"], m["bots"][0]["botId"]);
        assert_eq!(m["bots"][0]["botId"], "00000000-0000-0000-0000-000000000001");
        assert_eq!(m["bots"][0]["scopes"][0], "personal");
        assert_eq!(m["bots"][0]["scopes"][1], "team");
        assert_eq!(m["bots"][0]["scopes"][2], "groupchat");
        assert!(m.get("accentColor").is_some());
        assert!(m.get("validDomains").is_some());
        assert!(m.get("packageName").is_none(), "schema 1.17 rejects packageName");
        assert_eq!(m["icons"]["color"], "icon-color.png");
        assert_eq!(m["icons"]["outline"], "icon-outline.png");
    }

    #[test]
    fn icons_render_at_required_dimensions() {
        let (color, outline) = render_default_icons().expect("icons render");
        let c = image::load_from_memory(&color).expect("color png");
        assert_eq!((c.width(), c.height()), (192, 192));
        let o = image::load_from_memory(&outline).expect("outline png");
        assert_eq!((o.width(), o.height()), (32, 32));
    }

    #[test]
    fn package_zip_contains_the_three_root_files() {
        let bytes = build_app_package("00000000-0000-0000-0000-000000000001", "ffffffff-0000-0000-0000-000000000002")
            .expect("package builds");
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).expect("zip opens");
        let names: Vec<String> = (0..zip.len())
            .map(|i| zip.by_index(i).unwrap().name().to_string())
            .collect();
        assert!(names.contains(&"manifest.json".to_string()));
        assert!(names.contains(&"icon-color.png".to_string()));
        assert!(names.contains(&"icon-outline.png".to_string()));
        // manifest.json parses and round-trips the ids.
        let mut mf = zip.by_name("manifest.json").unwrap();
        let mut s = String::new();
        mf.read_to_string(&mut s).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(parsed["bots"][0]["botId"], "00000000-0000-0000-0000-000000000001");
    }

    #[test]
    fn manifest_id_sidecar_is_stable() {
        let dir = std::env::temp_dir().join(format!("teams-pkg-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = load_or_create_manifest_id(&dir);
        let b = load_or_create_manifest_id(&dir);
        assert_eq!(a, b, "second call reuses the persisted id");
        assert_eq!(a.len(), 36, "looks like a GUID");
        std::fs::remove_dir_all(&dir).ok();
    }
}
```

(Reader type confirmed: `zip::ZipArchive::new(Cursor)` in `zip 2.x` — `ZipReader` does not exist. The write side mirrors `src/api/system.rs:387-409` exactly: `ZipWriter::new` + `SimpleFileOptions::default().compression_method(CompressionMethod::Deflated)` + `start_file`/`write_all`/`finish`.)

- [ ] **Step 3: Run the tests to verify they fail**

Run: `systemd-run --scope -p MemoryMax=40G -p MemorySwapMax=0 cargo test --lib api::teams_package 2>&1 | tail -20`
Expected: FAIL — `teams_manifest_json` / `render_default_icons` / `build_app_package` / `load_or_create_manifest_id` not defined.

- [ ] **Step 4: Implement the module**

Prepend to `src/api/teams_package.rs` (above the tests):

```rust
//! Generates a sideloadable Microsoft Teams app package (manifest + icons, zipped)
//! so operators don't have to hand-write the manifest. The manifest gotchas
//! (schema 1.17, distinct app id, lowercase scopes, no packageName, required
//! accentColor/validDomains) are baked in; default icons are the Spacebot logo.

use std::io::{Cursor, Write as _};
use std::path::Path;

use image::imageops::FilterType;
use zip::CompressionMethod;
use zip::write::SimpleFileOptions;

/// The Spacebot logo, bundled at compile time. 384x384 RGBA PNG.
const SPACEBOT_LOGO: &[u8] = include_bytes!("../../interface/public/ball.png");

/// Build the Teams app manifest. `app_id` is the bot's App (client) ID
/// (`bots[].botId`); `manifest_id` is the distinct Teams app id (`id`).
pub fn teams_manifest_json(app_id: &str, manifest_id: &str) -> serde_json::Value {
    serde_json::json!({
        "$schema": "https://developer.microsoft.com/en-us/json-schemas/teams/v1.17/MicrosoftTeams.schema.json",
        "manifestVersion": "1.17",
        "version": "1.0.0",
        "id": manifest_id,
        "developer": {
            "name": "Spacebot",
            "websiteUrl": "https://github.com/spacedriveapp/spacebot",
            "privacyUrl": "https://github.com/spacedriveapp/spacebot",
            "termsOfUseUrl": "https://github.com/spacedriveapp/spacebot"
        },
        "icons": { "color": "icon-color.png", "outline": "icon-outline.png" },
        "name": { "short": "Spacebot", "full": "Spacebot — AI assistant" },
        "description": {
            "short": "AI assistant powered by Spacebot.",
            "full": "Chat with your Spacebot agent in Microsoft Teams."
        },
        "accentColor": "#6264A7",
        "bots": [{
            "botId": app_id,
            "scopes": ["personal", "team", "groupchat"],
            "supportsFiles": false,
            "isNotificationOnly": false
        }],
        "validDomains": []
    })
}
// NOTE: do NOT add a `permissions` key. The manifest validated against real
// Teams (docs/design-docs/teams-setup.md) has no `permissions` field; schema
// 1.17 rejects undefined/extra properties, and `permissions` is deprecated.

/// Render the two Teams icons from the bundled logo: color 192x192 (the logo
/// resized) and outline 32x32 (a white silhouette on transparent, per Teams'
/// outline-icon requirement).
pub fn render_default_icons() -> anyhow::Result<(Vec<u8>, Vec<u8>)> {
    let logo = image::load_from_memory(SPACEBOT_LOGO)?;

    let mut color = Vec::new();
    logo.resize_exact(192, 192, FilterType::Lanczos3)
        .write_to(&mut Cursor::new(&mut color), image::ImageFormat::Png)?;

    // Outline: resize to 32x32, then make every non-transparent pixel white
    // (Teams renders the outline icon monochrome on a transparent background).
    let mut outline_img = logo.resize_exact(32, 32, FilterType::Lanczos3).to_rgba8();
    for px in outline_img.pixels_mut() {
        let alpha = px.0[3];
        px.0 = [255, 255, 255, alpha];
    }
    let mut outline = Vec::new();
    image::DynamicImage::ImageRgba8(outline_img)
        .write_to(&mut Cursor::new(&mut outline), image::ImageFormat::Png)?;

    Ok((color, outline))
}

/// Build the `.zip` package: manifest.json + the two icons at the archive root.
pub fn build_app_package(app_id: &str, manifest_id: &str) -> anyhow::Result<Vec<u8>> {
    let manifest = serde_json::to_vec_pretty(&teams_manifest_json(app_id, manifest_id))?;
    let (color, outline) = render_default_icons()?;

    let mut cursor = Cursor::new(Vec::new());
    let mut zip = zip::ZipWriter::new(&mut cursor);
    let opts = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .unix_permissions(0o644);

    for (name, bytes) in [
        ("manifest.json", manifest.as_slice()),
        ("icon-color.png", color.as_slice()),
        ("icon-outline.png", outline.as_slice()),
    ] {
        zip.start_file(name, opts)?;
        zip.write_all(bytes)?;
    }
    zip.finish()?;
    Ok(cursor.into_inner())
}

/// Read the persisted Teams app `id`, or generate+persist a fresh GUID. Keeping
/// it stable means re-downloading produces the same `id`, so re-uploading the
/// package updates the existing Teams app instead of creating a duplicate.
pub fn load_or_create_manifest_id(instance_dir: &Path) -> String {
    let path = instance_dir.join("teams_manifest_id.json");
    if let Ok(contents) = std::fs::read_to_string(&path)
        && let Ok(v) = serde_json::from_str::<serde_json::Value>(&contents)
        && let Some(id) = v.get("manifest_id").and_then(|x| x.as_str())
        && !id.is_empty()
    {
        return id.to_string();
    }
    let id = uuid::Uuid::new_v4().to_string();
    let tmp = path.with_extension("json.tmp");
    let json = serde_json::json!({ "manifest_id": id }).to_string();
    if std::fs::write(&tmp, &json).is_ok() {
        if let Err(e) = std::fs::rename(&tmp, &path) {
            tracing::warn!(%e, ?path, "teams manifest-id sidecar: rename failed");
        }
    } else {
        tracing::warn!(?path, "teams manifest-id sidecar: write failed");
    }
    id
}
```

Declare the module — in **`src/api.rs`** (the api module tree is a single file, not a `mod.rs` directory), add alongside the other `mod` lines (e.g. after `mod messaging;` at line 20):

```rust
mod teams_package;
```

Plain `mod` (not `pub(super) mod`) matches the file's convention; the sibling `messaging` submodule reaches it as `crate::api::teams_package::…` via the shared `api` parent.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `systemd-run --scope -p MemoryMax=40G -p MemorySwapMax=0 cargo test --lib api::teams_package 2>&1 | tail -20`
Expected: PASS (4 tests). Fix the `zip` reader API in the test if it failed to compile (see Step 2 note).

- [ ] **Step 6: fmt + clippy + commit**

```
cargo fmt --all -- --check
systemd-run --scope -p MemoryMax=40G -p MemorySwapMax=0 cargo clippy --lib 2>&1 | tail -8
```
Expected: clean. Then:
```bash
git add src/api/teams_package.rs src/api/mod.rs Cargo.toml Cargo.lock
git commit -m "feat(api): Teams app-package builder (manifest + icons + zip)"
```

---

### Task 2: The download endpoint (`src/api/messaging.rs` + `server.rs`)

**Files:**
- Modify: `src/api/messaging.rs` (add the handler), `src/api/server.rs` (register the route)
- Test: covered by Task 1's builder tests; the handler is a thin wrapper (no new unit test — see note).

**Interfaces:**
- Consumes: `crate::api::teams_package::{load_or_create_manifest_id, build_app_package}` (Task 1); `state.instance_dir.load()`.
- Produces: `download_teams_app_package` axum handler at `GET /messaging/teams/app-package`.

- [ ] **Step 1: Add the handler to `src/api/messaging.rs`**

Use the binary-zip pattern from `src/api/system.rs:backup_export` (returns `(headers, bytes)`). Add near the other Teams/messaging handlers:

```rust
#[derive(serde::Deserialize)]
pub(super) struct TeamsPackageQuery {
    /// The bot App (client) ID — becomes `bots[].botId` in the manifest.
    app_id: String,
}

#[utoipa::path(
    get,
    path = "/messaging/teams/app-package",
    params(("app_id" = String, Query, description = "Bot App (client) ID")),
    responses(
        (status = 200, description = "Teams app package (zip)", content_type = "application/zip"),
        (status = 400, description = "Missing or invalid app_id"),
        (status = 500, description = "Failed to build package"),
    ),
    tag = "messaging",
)]
pub(super) async fn download_teams_app_package(
    State(state): State<Arc<ApiState>>,
    Query(query): Query<TeamsPackageQuery>,
) -> Result<impl axum::response::IntoResponse, (axum::http::StatusCode, String)> {
    let app_id = query.app_id.trim();
    if app_id.is_empty() {
        return Err((axum::http::StatusCode::BAD_REQUEST, "app_id is required".into()));
    }

    let instance_dir = state.instance_dir.load();
    // `&instance_dir` (a `&Guard<Arc<PathBuf>>`) coerces to `&Path` via deref,
    // matching how messaging.rs already uses the guard (it calls `.join()` on it).
    // Do NOT use `.as_ref()` here — it's ambiguous and won't coerce to `&Path`.
    let manifest_id =
        crate::api::teams_package::load_or_create_manifest_id(&instance_dir);

    let bytes = crate::api::teams_package::build_app_package(app_id, &manifest_id).map_err(|error| {
        tracing::error!(%error, "failed to build teams app package");
        (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "failed to build teams app package".into(),
        )
    })?;

    let headers = [
        (axum::http::header::CONTENT_TYPE, "application/zip"),
        (
            axum::http::header::CONTENT_DISPOSITION,
            "attachment; filename=spacebot-teams-app.zip",
        ),
    ];
    Ok((headers, bytes))
}
```

**Required import:** `messaging.rs` imports only `use axum::extract::State;` (line 4) — `Query` is **NOT** imported. Add `use axum::extract::Query;` to the handler's imports (mandatory, or it won't compile). `State`, `Arc`, `ApiState` are already imported.

- [ ] **Step 2: Register the route in `src/api/server.rs`**

In the messaging routes block (after `toggle_platform`, ~line 212):

```rust
        .routes(routes!(messaging::download_teams_app_package))
```

- [ ] **Step 3: Build + regenerate the OpenAPI typings**

```
systemd-run --scope -p MemoryMax=40G -p MemorySwapMax=0 cargo check --lib 2>&1 | tail -8
just typegen
```
`just typegen` regenerates `interface/src/api/schema.d.ts` from the new endpoint. Then confirm it matches:
```
just check-typegen
```
Expected: no diff (typegen is deterministic). If `just`/`bunx` is unavailable in this environment, run the equivalent from `justfile`: `cargo run --bin openapi-spec > /tmp/s.json && cd interface && bunx openapi-typescript /tmp/s.json -o src/api/schema.d.ts`.

- [ ] **Step 4: Commit**

```bash
git add src/api/messaging.rs src/api/server.rs interface/src/api/schema.d.ts
git commit -m "feat(api): GET /messaging/teams/app-package download endpoint"
```

(No unit test for the handler itself — it is a thin wrapper over the Task-1 builder, which is fully tested. The handler is exercised end-to-end by the post-merge cloudflared re-test.)

---

### Task 3: Frontend download button + URL helper

**Files:**
- Modify: `interface/src/api/client.ts` (URL helper), `interface/src/components/ChannelSettingCard.tsx` (button + doc link)

**Interfaces:**
- Consumes: the `GET /messaging/teams/app-package?app_id=` endpoint (Task 2); `getApiBase()` + the `credentialInputs.teams_app_id` form state.
- Produces: `api.teamsAppPackageUrl(appId: string)`.

- [ ] **Step 1: Add the URL helper in `interface/src/api/client.ts`**

Mirror `attachmentUrl` (≈ line 2166):

```typescript
	teamsAppPackageUrl: (appId: string) => {
		const params = new URLSearchParams({ app_id: appId });
		return `${getApiBase()}/messaging/teams/app-package?${params.toString()}`;
	},
```

- [ ] **Step 2: Add the download link + setup-guide link in `ChannelSettingCard.tsx`**

In the `platform === "teams"` section (after the Tenant ID input, ≈ line 1380), add — using the `<a href download>` pattern from `PortalTimeline.tsx`, enabled only once an App ID is entered:

```tsx
							{(credentialInputs.teams_app_id ?? "").trim() !== "" && (
								<div className="flex flex-wrap items-center gap-3 pt-1">
									<a
										href={api.teamsAppPackageUrl((credentialInputs.teams_app_id ?? "").trim())}
										download="spacebot-teams-app.zip"
										className="inline-flex items-center gap-1.5 rounded-md bg-accent px-3 py-1.5 text-sm font-medium text-white transition-colors hover:bg-accent/90"
									>
										Download Teams app package
									</a>
									<a
										href="https://github.com/spacedriveapp/spacebot/blob/main/docs/content/docs/(messaging)/teams-setup.mdx"
										target="_blank"
										rel="noreferrer"
										className="text-sm text-ink-dull underline hover:text-ink"
									>
										Setup guide
									</a>
								</div>
							)}
							<p className="text-xs text-ink-faint">
								The package uses the default Spacebot icon. To use your own, unzip it, replace
								<code> icon-color.png</code> (192×192) and <code>icon-outline.png</code> (32×32), and re-zip.
							</p>
```

(Confirmed: `api` is imported in `ChannelSettingCard.tsx:5`; `credentialInputs.teams_app_id` is the right field; `bg-accent`/`text-ink-dull`/`text-ink-faint` are real design tokens. The surrounding card uses a `<Button>` component for actions — for visual consistency prefer `<Button asChild><a …></Button>` or match the existing button styling; the `<a>` compiles regardless. **Setup-guide link:** the GitHub `main` URL 404s until #607/#608 merge — acceptable since the UI ships *with* those PRs, but if you prefer a never-dangling link, point it at the in-app docs route instead.)

- [ ] **Step 3: Build the frontend**

Run: `cd interface && bun run build 2>&1 | tail -15`
Expected: builds with no type errors (the `teamsAppPackageUrl` helper + the new JSX compile).

- [ ] **Step 4: Commit**

```bash
git add interface/src/api/client.ts interface/src/components/ChannelSettingCard.tsx
git commit -m "feat(interface): Download Teams app package button on the Teams channel card"
```

---

### Task 4: Documentation — `teams-setup.mdx`

> **Branch note:** this file **exists on the execution branch `pr/teams-channels-ui` (#608)** with a `## Step 3` and the Step-4 `<Tabs>` — it was created in #607 and extended in #608. (It is NOT on `integration`/`feat/teams-channel`; if a reviewer reads those branches it will look absent.) So this task **modifies** the existing Step 3, it does not create the file.

**Files:**
- Modify: `docs/content/docs/(messaging)/teams-setup.mdx`

- [ ] **Step 1: Rewrite Step 3 to lead with the button, keep manual + icon-swap**

Replace the current "Step 3: Build and install the Teams app" body with a `<Tabs>` (mirroring the Step 4 tabs already in the file): a **"Generate from Spacebot"** tab (the easy path) and a **"Manual"** tab (the existing guidance), and add the icon-swap note.

```mdx
## Step 3: Build and install the Teams app

Teams needs an app package (a zip with a `manifest.json` and two icons).

<Tabs items={["Generate from Spacebot", "Manual"]}>
<Tab value="Generate from Spacebot">

1. In **Settings → Channels**, open the Microsoft Teams card and enter your **App ID** (Step 1).
2. Click **Download Teams app package** — Spacebot builds a correct `manifest.json` (with the right schema, a distinct app id, and the Spacebot icon) and downloads `spacebot-teams-app.zip`.
3. Upload it in Teams (**Apps → Manage your apps → Upload an app**).

The package uses the default Spacebot icon. To use your own, unzip it, replace `icon-color.png` (192×192) and `icon-outline.png` (32×32, transparent), and re-zip.

</Tab>
<Tab value="Manual">

Build the package by hand if you prefer. The `manifest.json`:

<Callout type="info">
The manifest `id` (the Teams app id) must be its **own** GUID, distinct from the bot's App ID. `bots[].botId` is the App ID from Step 1. Scopes are lowercase (`personal`, `team`, `groupchat`); `validDomains` and `accentColor` are required; do **not** add a `packageName` key (schema 1.17 rejects it).
</Callout>

```json
{
  "$schema": "https://developer.microsoft.com/en-us/json-schemas/teams/v1.17/MicrosoftTeams.schema.json",
  "manifestVersion": "1.17",
  "version": "1.0.0",
  "id": "GENERATE-A-FRESH-GUID",
  "developer": { "name": "Spacebot", "websiteUrl": "https://example.com", "privacyUrl": "https://example.com", "termsOfUseUrl": "https://example.com" },
  "icons": { "color": "icon-color.png", "outline": "icon-outline.png" },
  "name": { "short": "Spacebot", "full": "Spacebot — AI assistant" },
  "description": { "short": "AI assistant powered by Spacebot.", "full": "Chat with your Spacebot agent in Microsoft Teams." },
  "accentColor": "#6264A7",
  "bots": [{ "botId": "YOUR-APP-ID", "scopes": ["personal", "team", "groupchat"], "supportsFiles": false, "isNotificationOnly": false }],
  "validDomains": []
}
```

Zip `manifest.json` + `icon-color.png` (192×192) + `icon-outline.png` (32×32) at the **root** of the archive.

</Tab>
</Tabs>

Most tenants require admin approval: approve it in the **Teams admin center** under **Manage apps**, where you can also restrict who may install it.
```

- [ ] **Step 2: Commit**

```bash
git add "docs/content/docs/(messaging)/teams-setup.mdx"
git commit -m "docs(teams): document the Download-app-package button + manual/icon-swap path"
```

---

## Final verification (before whole-branch review)

- [ ] `just gate-pr` green (`systemd-run --scope … just gate-pr`) — clippy `-D warnings`, fmt, full tests, **typegen schema diff clean**.
- [ ] `cd interface && bun run build` clean.
- [ ] The four new `teams_package` unit tests pass; the manifest asserts the gotchas (no `packageName`, `id ≠ botId`, scopes lowercase, accentColor/validDomains present).
- [ ] `git log --oneline` shows 4 trailer-free commits; **do not push**.
- [ ] Post-merge (not in this plan): real-Teams re-test via cloudflared — download the package from the UI, sideload it, confirm Teams accepts the manifest and the bot responds.

## Self-Review notes (author)

- **Spec coverage:** button in UI (Task 3) + endpoint/builder (Tasks 1-2) + default Spacebot icon with manual-swap instructions (Tasks 1 + 4) + doc link (Task 3) — all four of the user's asks covered.
- **Persistent app id** (`load_or_create_manifest_id` + sidecar) means re-downloads reuse the same Teams app id, so re-uploads update rather than duplicate. `id ≠ botId` enforced in the builder + asserted in tests.
- **Type consistency:** `teams_manifest_json(app_id, manifest_id)`, `build_app_package(app_id, manifest_id)`, `load_or_create_manifest_id(instance_dir)`, `teamsAppPackageUrl(appId)` are used identically across tasks. The endpoint takes only `app_id` (the manifest doesn't use tenant id; the secret never appears).
- **Opus review (2026-06-29) — applied:** C1 removed the speculative `permissions` manifest field (the validated manifest lacks it → rejection risk); C2 module declared in `src/api.rs` (not a non-existent `src/api/mod.rs`); C3 made the `use axum::extract::Query;` import mandatory; C4 test uses `zip::ZipArchive` (not `ZipReader`); I1 passes `&instance_dir` (not `.as_ref()`); I3 clarified the `.mdx` exists on #608 (modify, not create). Review confirmed CLEAN: `image` 0.25.9 API (`resize_exact`/`write_to`/`ImageFormat::Png`/`to_rgba8`), `ball.png` 384×384, the zip write-side + utoipa binary annotation (`content_type`, mirrors `system.rs:backup_export` — typegen stays green), and the frontend imports/tokens/field name.
- **Residual risk:** adding `image` as a direct dep on an upstream PR — already in the lockfile (transitive), but flag it for maintainers in the PR body. `load_or_create_manifest_id` on a read-only instance dir falls back to a fresh GUID each call (re-download stability breaks in that edge case); acceptable, noted.
