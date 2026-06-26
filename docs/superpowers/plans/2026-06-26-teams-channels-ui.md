# Teams in the Channels UI (v1.2) — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development. Steps use `- [ ]`.
> **This plan was rewritten after an Opus plan-review found the first draft's model wrong.** The verified facts below supersede intuition.

**Goal:** Make Teams a first-class channel in the admin UI (Settings → Channels): connect a Teams bot from the form, persist its credentials, **and have it actually start** (live, without a restart). That last part forces the v1.1 **watcher parity** work into scope.

## Verified facts (checked against code — do NOT deviate)
- `create_messaging_instance` (`src/api/messaging.rs`) only **writes config + reloads bindings**; it does NOT start adapters (comment at `messaging.rs:2043-2045`). **The file watcher starts them.** So a UI "Connect" that should start Teams live REQUIRES a teams arm in `src/config/watcher.rs` (which today has **zero** teams references).
- Both `create_messaging_instance` (gate at `messaging.rs:1634`) and `delete_messaging_instance` (gate at `messaging.rs:2087`) reject any platform not in a `matches!` allowlist → teams is rejected today.
- Config-write arms are per-platform: default at `messaging.rs:~1714-1845` (mattermost ~1791), named at `~1879-2009` (mattermost ~1956). `delete` clears creds via a per-platform match at `~2146`.
- Status `instances` vec is built by `push_instance_status` calls in the status handler (`messaging.rs:379+`), per-platform reading `m.get("<platform>")`. No teams block → a Teams instance won't render in the UI. `MessagingStatusResponse` (`messaging.rs:30-39`) has fixed fields discord…signal + `instances: Vec<AdapterInstanceStatus>`; the frontend `InstanceCard` renders the `instances` vec.
- `InstanceCredentials` struct (`messaging.rs:58`, `#[derive(utoipa::ToSchema)]`) has `mattermost_base_url`/`mattermost_token` to mirror. The frontend type comes from **auto-generated** `interface/src/api/schema.d.ts` ("Do not make direct changes") via the `justfile` recipe (`cargo run --bin openapi-spec | openapi-typescript`), and CI **diff-checks** it (`justfile:58-60`). So: add fields to the Rust struct, then **regenerate the schema** — never hand-edit the TS type.
- `spawn_file_watcher` (`watcher.rs:26-41`) takes 6 permission params (teams absent); 3 call sites in main.rs (`1838`, `1856`, `2624`). Mattermost watcher arm at `watcher.rs:637-685` = the template (default + named-instance `register_and_start`).
- v1 = **single Teams bot**: `main.rs:3607-3616` deliberately skips named teams instances. `TeamsAdapter::new(...)` is fallible; the v1 registration adds `.with_sidecar_path(instance_dir.join("teams_service_urls.json"))` (`main.rs:3596`). `register_and_start` exists (`manager.rs:108`).

## Decisions (locked)
- **Single-instance only:** reject named Teams instances in the create handler AND hide the "Add Instance" affordance in the UI (only the default "Connect"). Never write dead named config.
- **Extract a shared builder** `build_teams_adapter(runtime_key, app_id, client_secret, tenant_id, port, bind, permissions, instance_dir) -> anyhow::Result<TeamsAdapter>` (sets the sidecar path), called by BOTH `main.rs` registration and the new `watcher.rs` arm — so the construction + sidecar path can't drift (the codebase tolerates per-platform duplication, but we avoid it for teams since the sidecar path is easy to forget).
- Mirror **Mattermost** throughout (closest: self-hosted, fallible `new()`, `from_config`/`from_instance_config`).

## Global Constraints
- Branch `feat/teams-channel`. `cargo build` + `cargo fmt --all -- --check` + the schema diff-check (`justfile` recipe) + `cd interface && bun run build` ALL green.
- **No commit trailers** (verify `git log -1 --format=%B | grep -ciE "Co-Authored-By|Claude-Session|Generated with"` == 0). Add files explicitly. Heavy cargo wrapped in `systemd-run --scope -p MemoryMax=40G -p MemorySwapMax=0`. Disk guard active — run normally.
- `client_secret` is secret: never log it (already `SecretCategory::System`, Debug-redacted). Reuse existing types — do not invent config shapes.

---

## Task 1: Shared `build_teams_adapter` helper + refactor main.rs to use it
**Files:** `src/messaging/teams.rs` (or a small `src/messaging/mod`-level fn), `src/main.rs`
- [ ] **Step 1:** Add `pub fn build_teams_adapter(runtime_key: impl Into<String>, app_id, client_secret, tenant_id, port: u16, bind, permissions: Arc<ArcSwap<TeamsPermissions>>, instance_dir: &Path) -> anyhow::Result<TeamsAdapter>` that calls `TeamsAdapter::new(...)?.with_sidecar_path(instance_dir.join("teams_service_urls.json"))`. (Match the exact construction at `main.rs:3585-3598`.)
- [ ] **Step 2:** Refactor the `main.rs` default-teams registration (`main.rs:3577-3606`) to call this helper (behaviour identical; named instances still skipped with the existing warn). 
- [ ] **Step 3:** `cargo build` clean; `cargo test --lib messaging::teams` still green; `cargo fmt --all -- --check`. Commit (no trailers).

## Task 2: Backend — credentials field + OpenAPI schema regen
**Files:** `src/api/messaging.rs`, `interface/src/api/schema.d.ts` (generated)
- [ ] **Step 1:** Add `teams_app_id`, `teams_client_secret`, `teams_tenant_id` (`Option<String>`) to `InstanceCredentials` (`messaging.rs:58`), mirroring the `mattermost_*` fields (incl. any `#[schema(...)]` attrs).
- [ ] **Step 2:** Regenerate the schema: run the `justfile` generate recipe (`cargo run --bin openapi-spec > /tmp/s.json && cd interface && bunx openapi-typescript /tmp/s.json -o src/api/schema.d.ts`). Confirm `teams_*` appear in `CreateMessagingInstanceRequest` in `schema.d.ts`.
- [ ] **Step 3:** Verify the diff-check passes (`justfile` check recipe → `diff` is empty). `cargo build` clean. Commit `src/api/messaging.rs` + `interface/src/api/schema.d.ts` (no trailers).

## Task 3: Backend — create handler (gate + reject named + config write)
**Files:** `src/api/messaging.rs`
- [ ] **Step 1:** Add `"teams"` to the create allowlist `matches!` (`~messaging.rs:1628-1634`).
- [ ] **Step 2:** **Reject named teams:** in the create handler, if `platform == "teams"` and a non-default instance name is requested, return the structured error (`success:false`, message e.g. "Teams supports a single bot in this version; named instances are not available."). v1 single-listener.
- [ ] **Step 3:** Add the default-instance config-write arm for teams (mirror mattermost `~1791`): require `teams_app_id`/`teams_client_secret`/`teams_tenant_id` (else structured "X is required" error like mattermost), write `[messaging.teams]` `app_id`/`client_secret`/`tenant_id`. Leave `port`/`bind` at `TeamsConfig` defaults (not in the form).
- [ ] **Step 4:** Test via the config-load round-trip pattern (`load.rs:2757/2802` style): write the toml the handler produces, `Config::load_from_path`, assert `TeamsConfig` fields + that a missing secret disables it. (No live Azure needed.) Run `cargo test --lib`; commit (no trailers).

## Task 4: Backend — delete handler + status visibility
**Files:** `src/api/messaging.rs`
- [ ] **Step 1:** Add `"teams"` to the delete allowlist gate (`~2087`).
- [ ] **Step 2:** Add a `"teams"` arm to the delete clear-credentials match (`~2146`): remove `app_id`/`client_secret`/`tenant_id`/`dm_allowed_users` from the `[messaging.teams]` table.
- [ ] **Step 3:** Add a teams block in the status handler (`~379+`) calling `push_instance_status` for the configured default teams instance (mirror how mattermost surfaces — read `m.get("teams")`), so a connected Teams bot renders in the UI `instances` list. (Decide whether to also add a fixed `teams: PlatformStatus` field to `MessagingStatusResponse` — only if the frontend status overview needs it; the `instances` vec is what `InstanceCard` uses, so the `push_instance_status` block is the required part. Note the choice in the report.)
- [ ] **Step 4:** `cargo build` + `cargo fmt --all -- --check` clean. Commit (no trailers).

## Task 5: Watcher hot-start parity (makes "Connect" actually start Teams)
**Files:** `src/config/watcher.rs`, `src/main.rs`
- [ ] **Step 1:** Add `teams_permissions: Option<Arc<ArcSwap<TeamsPermissions>>>` to `spawn_file_watcher` (after `signal_permissions`, `watcher.rs:35`) and import `TeamsPermissions`.
- [ ] **Step 2:** Add the permission `.store()` swap block for teams (mirror the signal/mattermost blocks ~`watcher.rs:245-260`): `TeamsPermissions::from_config(cfg, &config.bindings)`.
- [ ] **Step 3:** Add the teams `register_and_start` arm in the hot-start closure (mirror mattermost `637-685`), **default instance only** (no named loop — v1), using `build_teams_adapter(...)` from Task 1 (so the sidecar path is set). Capture `teams_permissions` in the closure.
- [ ] **Step 4:** Pass `teams_permissions` at all **3** `spawn_file_watcher` call sites in main.rs (`1838`, `1856`, `2624`). The handle already exists (`main.rs:3572-3573`).
- [ ] **Step 5:** `cargo build` + `cargo fmt --all -- --check` clean. Commit (no trailers). (Live hot-reload needs a running instance + endpoint to fully verify; the build + the code mirroring is the gate. Note this.)

## Task 6: Frontend — catalog, credential form, icon (single-instance)
**Files:** `interface/src/components/ChannelSettingCard.tsx`, `interface/src/components/settings/types.ts`, `interface/src/lib/platformIcons.tsx`
- [ ] **Step 1:** Add `"teams"` to BOTH `Platform` unions (`ChannelSettingCard.tsx:35-43` + `settings/types.ts:17-25`); `teams: "Microsoft Teams"` in `PLATFORM_LABELS`; `"teams"` in the `PLATFORMS` catalog array; a `DOC_LINKS.teams`; `teams: faMicrosoft` in `platformIcons.tsx` (`faMicrosoft` from `@fortawesome/free-brands-svg-icons`, already imported).
- [ ] **Step 2:** `AddInstanceCard.handleSave`: add `else if (platform === "teams")` requiring `teams_app_id`/`teams_client_secret`/`teams_tenant_id` (structured error if missing) and setting them trimmed on `credentials`.
- [ ] **Step 3:** Add the `platform === "teams"` credential JSX (mirror mattermost `~1266-1305`): App ID (text), Client Secret (`type=password`), Tenant ID (text), Enter-to-save on the last.
- [ ] **Step 4:** **Single-instance UI:** ensure the catalog "Add" for teams opens the DEFAULT connect form only — do not offer "Add Instance" (named) for teams. (Check how `ChannelsSection`/`PlatformCatalog` decides default-vs-named; gate teams to default. Mirror any platform that's default-only if one exists, else add a small `teams`-specific guard.)
- [ ] **Step 5:** BindingForm: Teams has no guild/workspace/chat_id. Add teams to the Channel IDs group (`~1524`) so bindings can scope to Teams conversation ids, OR leave default routing (agent + dm_allowed_users). Pick the simpler; note it.
- [ ] **Step 6:** `cd interface && bun run build 2>&1 | tail -25` clean (no TS errors). Commit (no trailers).

## Task 7: Whole-feature wire-through verification
- [ ] **Step 1:** Document the end-to-end trace: UI "Connect Teams" → `api.createMessagingInstance({platform:"teams", credentials:{teams_*}})` → Task-3 gate+config-write → file watcher (Task-5) builds via `build_teams_adapter` and `register_and_start` → adapter live; status (Task-4) shows it. Confirm field names align: `teams_app_id`→`TeamsConfig.app_id`, etc.
- [ ] **Step 2:** Full gate: `cargo build`, `cargo fmt --all -- --check`, the schema diff-check, `cd interface && bun run build` — all green. Commit any fmt/schema fixes (no trailers).

## Final whole-branch review (most capable model)
- Create rejects named teams; never logs `client_secret`; writes the exact config shape `load.rs` parses.
- Watcher teams arm + `build_teams_adapter` shared with main.rs (no drift); sidecar path set; default-only.
- Delete clears teams creds + gate; status surfaces teams instances.
- `schema.d.ts` regenerated (diff-check green), not hand-edited; frontend field names ↔ backend creds ↔ `TeamsConfig` align end-to-end.
- Both builds green; no trailers.

## Out of scope (v1.2)
- Multi-bot / named Teams instances + per-instance port (v1.1 multi-bot listener).
- Azure-side setup (app registration, Azure Bot, manifest, reverse proxy) — manual; the form links to `teams-setup.md`.
- Live create→Teams round-trip (needs public HTTPS endpoint).
