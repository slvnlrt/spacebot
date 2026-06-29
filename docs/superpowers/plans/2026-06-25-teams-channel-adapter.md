# Microsoft Teams Channel Adapter — v1 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

## Plan review — applied corrections (2026-06-25, Opus-reviewed, verified against code)

These supersede anything below that conflicts:

- **C4 — permissions convention.** Every binding-integrated adapter (Slack `slack.rs:83`, Mattermost, Twitch, Discord, Telegram, Signal) takes an `Arc<ArcSwap<{Platform}Permissions>>` built from bindings; only `webhook.rs:79` uses a bare `auth_token`. A Teams **channel** must route via bindings like Slack → **v1 implements `TeamsPermissions`** (`config/permissions.rs`, mirroring `SlackPermissions`), the adapter takes it, and registration genuinely mirrors the Slack block. The earlier "ad-hoc `allowed_users` on the adapter" idea is dropped — use the bindings/`dm_allowed_users` path like every other channel.
- **W3 — NOT "no core agent-loop changes".** `src/agent/channel.rs` gates by `message.source`: `compute_listen_mode_invocation` (~`:3889`) returns `invoked_by_mention=false` for unknown sources → **without a `teams` arm, @mention in a channel won't trigger the bot.** v1 **must** add `teams` to the mention-detection source list (Task 10). Slash-commands (`channel.rs:~1223`) and quiet-mode fallback (`~:3975`) stay v2. The Architecture/checklist "no core changes" wording is corrected to "minimal additive: mention-source list".
- **C1/C2 — target.rs needs three teams touchpoints**, not one: the prefix list in `parse_delivery_target` (`target.rs:34`), the `parse_named_instance_target` allow-list (`~:595`), and a `"teams"` arm in `resolve_broadcast_target`'s `match adapter` (else `_ => None` at `~:188` makes proactive sends to tracked Teams channels silently fail). Reconcile the two proactive paths: `resolve_broadcast_target` yields the target string; the **serviceUrl sidecar** yields the POST base URL — both are needed.
- **W2 — `extract_platform_meta`** (`conversation/channels.rs:~339` `_ => {}`) needs a `teams` arm to persist `serviceUrl`/`conversationType`, else channel metadata is dropped.
- **W1 — config hot-reload (`watcher.rs`) deferred to v1.1.** Consequence: Teams won't pick up config changes until restart (boot-time registration works). Stated explicitly, not silent.
- **MS Bot Framework specifics** (JWT issuer URLs, token scope, Activity schema) remain "implementer verify at build time".

**Goal:** Add a Microsoft **Teams** messaging adapter (bidirectional channel + DM) to spacebot, following the existing adapter pattern, scoped to the **wiring + text** path.

**Architecture:** Implement the `Messaging` trait for a `TeamsAdapter` backed by the **Bot Framework** (Azure Bot Service). Inbound: the adapter binds its **own axum server** (the `webhook.rs` convention) exposing `POST /api/messages`; it validates the Azure-signed JWT, parses the Bot Framework `Activity`, normalizes it to an `InboundMessage`, and emits it on the adapter's `InboundStream`. Outbound: `respond()`/`broadcast()` acquire an Azure AD app token (client-credentials, cached) and `POST` an `Activity` to the Bot Connector REST API at the conversation's `serviceUrl` (captured from inbound and persisted for proactive sends). Registered in `MessagingManager` by runtime key like every other adapter, with one **minimal additive** core touch: adding `teams` to the channel mention-detection source list so @mentions trigger the bot (see corrections C4/W3).

**Tech Stack:** Rust, `axum` (inbound, like `webhook.rs`), `reqwest` (outbound REST, like other adapters), `serde`/`serde_json` (Activity schema), `jsonwebtoken` (validate inbound Azure JWT against the Bot Framework JWKS), `azure_identity` (client-credentials token with caching/refresh), `tokio`.

## Global Constraints

- **Branch:** `feat/teams-channel` (off `main`; do NOT start on `main`).
- **Default build stays green:** `just gate-pr` ALL GREEN; the adapter compiles unconditionally (like `slack`/`discord` — messaging adapters are not feature-gated). New crates (`jsonwebtoken`, `azure_identity`) are added to `Cargo.toml`; keep them lean (rustls, no openssl).
- **Heavy cargo wrapped:** `systemd-run --scope -p MemoryMax=40G -p MemorySwapMax=0 …`.
- **Follow conventions, do not invent:** inbound HTTP = own axum server on a configured port (mirror `src/messaging/webhook.rs`); config shape = mirror `SlackConfig` incl. named instances; target format = `target.rs` per-platform pattern.
- **Map to the EXISTING capability vocabulary — do not add new `MessageContent`/`OutboundResponse` variants in v1** (see table below).
- Add files to git **explicitly**. Frequent commits, one per task. No commit trailers (clean for a potential upstream PR).
- **Secrets:** Teams credentials (client secret) go through the same config/secrets path as other adapters; never log them.

## Capability mapping (Teams → existing vocabulary)

| Teams event/capability | Existing variant | v1? |
|---|---|---|
| Inbound text message (DM, @mention in channel) | `MessageContent::Text` | ✅ v1 |
| Inbound attachments | `MessageContent::Media { attachments }` | v2 (adapter normalization only) |
| Inbound Adaptive Card `Action.Submit` / button click | `MessageContent::Interaction { action_id, values, … }` (same rail as Slack `block_actions`) | v2 |
| Outbound text reply | `OutboundResponse::Text` | ✅ v1 |
| Outbound thread reply | `OutboundResponse::ThreadReply` | v2 |
| Outbound file / rich / reaction | `OutboundResponse::File` / `RichMessage` / `Reaction` | v2 |
| Typing indicator | `OutboundResponse::Status` / `send_status` | v2 |
| Streaming (edit-in-place) | `StreamStart`/`StreamChunk`/`StreamEnd` | v2 |
| **Human-approval Adaptive Card (gate an agent action)** | builds on the existing `TaskStatus::PendingApproval` + `task_update(approved_by)` gate; card = delivery/resolution surface | **v3 / separate feature** (touches the task-approval layer, not just the adapter) |
| Teams-exotic `invoke` (task modules, message extensions, modals) | none — would need new vocabulary + agent wiring | **out of scope** |

v1 implements only the ✅ rows. v2/v3 are explicit follow-ons; each non-v1 row is "add a match arm + adapter-side normalization onto the existing variant", except the last two which are separate cross-cutting features.

## File structure

- **Create** `src/messaging/teams.rs` — the `TeamsAdapter` (`Messaging` impl), Activity structs, JWT validation, token provider, inbound axum server, outbound Bot Connector client.
- **Create** `docs/design-docs/teams-setup.md` — operator setup (Azure AD app, Azure Bot resource + Teams channel, app manifest `.zip`, the public-HTTPS reverse-proxy requirement).
- **Modify** `src/messaging.rs` (or `messaging/mod.rs`) — `pub mod teams;`.
- **Modify** `src/config/types.rs` — `MessagingConfig.teams`, `TeamsConfig`, `TeamsInstanceConfig`, `Debug` redaction, `SystemSecrets`, add `"teams"` to `is_named_adapter_platform()`.
- **Modify** `src/config/toml_schema.rs` — `TomlTeamsConfig`/`TomlTeamsInstanceConfig` in `TomlMessagingConfig`.
- **Modify** `src/config/load.rs` — resolve Teams creds (env/TOML), validate, return `Some(TeamsConfig)`/`None`.
- **Modify** `src/messaging/target.rs` — `teams:` parse/normalize arm.
- **Modify** `src/main.rs` — register default + named Teams instances at startup (mirror Slack block).
- **Cargo.toml** — add `jsonwebtoken`, `azure_identity` (rustls).
- **Modify** `src/config/permissions.rs` — `TeamsPermissions` (v1, Task 10).
- **Modify** `src/agent/channel.rs`, `src/conversation/channels.rs` — mention-source arm + `extract_platform_meta` arm (Task 11).
- *(Deferred to v1.1, not v1-blocking: `config/watcher.rs` hot-reload only.)*

---

## Task 1: Config types + TOML + loading

**Files:** `src/config/types.rs`, `src/config/toml_schema.rs`, `src/config/load.rs`
**Interfaces produced:** `TeamsConfig { enabled, app_id, client_secret, tenant_id, port, bind, instances: Vec<TeamsInstanceConfig>, <permission/binding fields mirroring SlackConfig — read by TeamsPermissions in Task 10> }`, `TeamsInstanceConfig { name, enabled, app_id, client_secret, tenant_id, <same permission fields as SlackInstanceConfig> }`; `MessagingConfig.teams: Option<TeamsConfig>`. **Do NOT add a raw `allowed_users` field — permissions flow through `TeamsPermissions`/bindings (C4); mirror exactly what `SlackConfig`/`SlackInstanceConfig` carry.**

- [ ] **Step 1:** Add `TeamsConfig`/`TeamsInstanceConfig` to `types.rs` mirroring `SlackConfig`/`SlackInstanceConfig` (fields above; `port` default 3979 to avoid clashing with webhook's default; `bind` default `"0.0.0.0"` since it sits behind a reverse proxy). Implement `Debug` redacting `client_secret`, and `SystemSecrets` listing the secret field(s). Add `pub teams: Option<TeamsConfig>` to `MessagingConfig` and its `Default`.
- [ ] **Step 2:** Add `"teams"` to `is_named_adapter_platform()`.
- [ ] **Step 3:** Add `TomlTeamsConfig`/`TomlTeamsInstanceConfig` to `toml_schema.rs` (`TomlMessagingConfig.teams`).
- [ ] **Step 4:** In `load.rs`, resolve creds from env (`TEAMS_APP_ID`/`TEAMS_CLIENT_SECRET`/`TEAMS_TENANT_ID`) or TOML, validate (disable instance if creds missing, warn), return `Some(TeamsConfig)`/`None`. Mirror the Slack/Mattermost loader.
- [ ] **Step 5:** Test — a `#[test]` parsing a TOML snippet (default + one named instance) into `TeamsConfig`; assert fields + that a missing secret disables the instance. Run `cargo test --lib config::`. Commit.

## Task 2: Azure AD token provider (client-credentials, cached)

**Files:** `src/messaging/teams.rs` (new, partial), `Cargo.toml`
**Interface:** `struct TeamsTokenProvider { … }` with `async fn bearer(&self) -> Result<String>` returning a cached/refreshed Bot Connector token (scope `https://api.botframework.com/.default`).

- [ ] **Step 1:** Add `azure_identity` (rustls) to `Cargo.toml`. Implement `TeamsTokenProvider` using `ClientSecretCredential` (tenant, app_id, client_secret) wrapped for auto-refresh; OR hand-roll: `POST https://login.microsoftonline.com/{tenant}/oauth2/v2.0/token` (`grant_type=client_credentials`, `scope=https://api.botframework.com/.default`) with `reqwest`, caching the token with a 5-minute early-refresh leeway behind a `tokio::sync::Mutex<Option<CachedToken>>` so concurrent callers coalesce on a single refresh (the Hermes pattern). **Implementer: verify the exact `azure_identity` API; if it adds friction, hand-roll the POST — the endpoint + params above are stable.**
- [ ] **Step 2:** Test the pure cache-decision helper: `fn needs_refresh(expiry, now, leeway) -> bool` (exponential nothing — just the leeway boundary). Assert refresh fires at `expiry - leeway`. Commit.

## Task 3: Inbound JWT validation

**Files:** `src/messaging/teams.rs`, `Cargo.toml`
**Interface:** `async fn validate_inbound_jwt(auth_header: &str, app_id: &str, jwks: &JwksCache) -> Result<()>`.

- [ ] **Step 1:** Add `jsonwebtoken` to `Cargo.toml`. Implement JWKS fetch+cache from the Bot Framework OpenID config (`https://login.botframework.com/v1/.well-known/openidconfiguration` → `jwks_uri` → keys), refreshed ≥ daily.
- [ ] **Step 2:** Implement validation: RS256 signature against the matching `kid`, `iss == https://api.botframework.com`, `aud == app_id`, not expired. Reject (HTTP 401/403) otherwise. **Implementer: confirm the current issuer/openid-config URLs from Microsoft docs (Bot Connector authentication) at implementation time.**
- [ ] **Step 3:** Test the claim-checking logic with a locally-signed token (generate an RS256 keypair in the test, build a token with correct/incorrect `iss`/`aud`/`exp`, assert accept/reject). This tests the validator independent of the live JWKS. Run; commit.

## Task 4: Bot Framework `Activity` ↔ `InboundMessage`

**Files:** `src/messaging/teams.rs`
**Interface:** `fn activity_to_inbound(activity: &Activity, adapter_key: &str) -> Option<InboundMessage>` (None for non-message activities in v1).

- [ ] **Step 1:** Define `serde` structs for the inbound `Activity` subset we need: `type`, `id`, `text`, `from {id, name}`, `conversation {id, conversationType}`, `serviceUrl`, `recipient`, `channelData`, `replyToId`. (Only `type == "message"` is handled in v1.)
- [ ] **Step 2:** Implement `activity_to_inbound`: strip `<at>…</at>` bot-mention tags from `text`; map `conversationType` (`personal`→DM, `channel`/`groupChat`→channel) into the `conversation_id` format `teams:{conversation.id}` (or `teams:{instance}:{conversation.id}` for named instances, via `apply_runtime_adapter_to_conversation_id`); set `content = MessageContent::Text(stripped)`, `source = "teams"`, `adapter = Some(runtime_key)`, `sender_id`, `timestamp`, and stash `serviceUrl` + `replyToId` in `metadata`.
- [ ] **Step 3:** Test with 2-3 captured Activity JSON samples (personal message, channel @mention) → assert the produced `InboundMessage` (conversation_id, stripped text, metadata). Run; commit.

## Task 5: Adapter skeleton + inbound server (`start`)

**Files:** `src/messaging/teams.rs`
**Interface:** `impl Messaging for TeamsAdapter` — `name()`, `start() -> InboundStream`, `health_check()`, `shutdown()`. (`respond`/`broadcast` stubbed to `Ok(())` until Task 6.)

- [ ] **Step 1:** Define `TeamsAdapter { runtime_key, app_id, tenant_id, token: TeamsTokenProvider, jwks: JwksCache, port, bind, service_urls: Arc<…>, inbound_tx, permissions: Arc<ArcSwap<TeamsPermissions>> }`. `new(...)` from config + permissions (Task 10 supplies `TeamsPermissions`; if Task 10 runs after this, stub the field as `Arc<ArcSwap<TeamsPermissions>>` and wire enforcement in Task 10). No raw `allowed_users`.
- [ ] **Step 2:** Implement `start()` mirroring `webhook.rs`: build an axum `Router` with `POST /api/messages` and `GET /health`; bind `TcpListener` on `bind:port`; `tokio::spawn(axum::serve(...))`; return the `InboundStream` (a `tokio::mpsc` → `ReceiverStream`).
- [ ] **Step 3:** The `/api/messages` handler: read the `Authorization` header → `validate_inbound_jwt` (401 on failure) → deserialize `Activity` → capture `serviceUrl` into the `service_urls` map keyed by `conversation_id` → `activity_to_inbound` → apply allowlist (silently drop unauthorized senders) → send on `inbound_tx`. Respond `200 OK` promptly.
- [ ] **Step 4:** `health_check()` returns `Ok(())` if the token provider can mint a token. `name()` returns the runtime key.
- [ ] **Step 5:** Test the handler wiring at the pure level where possible (allowlist decision; serviceUrl capture). Full HTTP round-trip is exercised manually (needs a signed request). Run `cargo test --lib messaging::teams`; commit.

## Task 6: Outbound text (`respond`) + proactive (`broadcast`)

**Files:** `src/messaging/teams.rs`
**Interface:** `respond(&self, message, OutboundResponse::Text)`, `broadcast(&self, target, response)`.

- [ ] **Step 1:** `respond` for `OutboundResponse::Text`: resolve `serviceUrl` (from `message.metadata`, fallback to the `service_urls` map by conversation), get a bearer token, `POST {serviceUrl}/v3/conversations/{conversationId}/activities` with an `Activity { type: "message", text, replyToId? }`. Use `mark_classified_broadcast` on API errors so permission/not-found are permanent. **Convention for unsupported `respond` variants = the `webhook.rs` one: map to text if possible, else `return Ok(())` (silent no-op) — never an error.** (`broadcast` is the opposite: it returns a permanent error via `ensure_supported_broadcast_response` — Step 2.) Verify against `webhook.rs:~197` at implementation time.
- [ ] **Step 2:** `broadcast(target, response)`: parse `target` (`teams:{conversation_id}`), look up `serviceUrl` from the persisted `service_urls` sidecar, gate on `ensure_supported_broadcast_response("teams", &response, is_supported)` where `is_supported` allows only `Text` in v1, then POST as in Step 1.
- [ ] **Step 2b:** Persist `service_urls` to a sidecar file under the agent/instance data dir (Hermes pattern) so proactive sends survive restarts. Load on `new()`.
- [ ] **Step 3:** Test: `is_supported` predicate (Text true, others false); target parsing. Run; commit.

## Task 7: Target normalization (THREE touchpoints — see C1/C2)

**Files:** `src/messaging/target.rs`
- [ ] **Step 1:** Add `raw.starts_with("teams:")` to the named-instance prefix check in `parse_delivery_target` (`target.rs:34`) so `teams:{instance}:{conversation_id}` routes to `parse_named_instance_target` instead of the generic 2-segment `split_once`.
- [ ] **Step 2:** Add `teams` to the `parse_named_instance_target` allow-list/`match adapter` (`~:595`) so it accepts teams targets (mirror the `slack`/`discord` arms, incl. the named `["teams", _, channel_id]` shape).
- [ ] **Step 3:** Add a `"teams"` arm to `resolve_broadcast_target`'s `match channel.platform` (`~:56-188`, whose fall-through is `_ => return None`) returning the conversation id from `channel` metadata — else proactive/broadcast sends to a tracked Teams channel silently no-op.
- [ ] **Step 4:** Add the `"teams"` arm to `normalize_target`.
- [ ] **Caveat:** MS conversation ids can themselves contain `:`/`;` — do not assume exactly 2-3 segments; split on the FIRST colon(s) for the platform/instance prefix and keep the remainder verbatim. The serviceUrl (POST base) is NOT in the target — it comes from the sidecar (Task 6).
- [ ] **Step 5:** Test: default + named target round-trip, a conversation id containing a colon, and empty rejection. Run `cargo test --lib messaging::target`; commit.

## Task 8: Registration at startup + module wiring

**Files:** `src/messaging.rs` (mod decl), `src/main.rs`
- [ ] **Step 1:** `pub mod teams;`.
- [ ] **Step 2:** In `main.rs`, after the Mattermost block, add the Teams block: if `config.messaging.teams` enabled, build the default adapter (if creds present) + one per named instance, register each with `new_messaging_manager.register(adapter)` (runtime key `teams` / `teams:{name}`). Mirror the Slack registration block exactly.
- [ ] **Step 3:** `cargo build` (default) clean; `just gate-pr` green. Commit.

## Task 9: Setup documentation

**Files:** `docs/design-docs/teams-setup.md`
- [ ] Document: create Azure AD app (App ID + client secret + tenant); create Azure Bot resource, set messaging endpoint to `https://<public-host>/api/messages`, add the Teams channel; build the Teams app manifest `.zip` (manifest v1.29: `bots` with `botId` + scopes `personal`/`team`/`groupchat`, icons); sideload/admin-approve; **the public-HTTPS reverse-proxy requirement** (Caddy/nginx + TLS, or cloudflared/ngrok in dev) pointing at the adapter's `bind:port`; the config TOML example. Commit.

## Task 10: `TeamsPermissions` + bindings integration (C4)

**Files:** `src/config/permissions.rs`, `src/messaging/teams.rs`, `src/main.rs`
- [ ] **Step 1:** Add `TeamsPermissions` to `permissions.rs` mirroring `SlackPermissions` (`from_config`/`from_instance_config`, `channel_bindings`, `dm_allowed_users`), hot-reloadable via `Arc<ArcSwap<TeamsPermissions>>`.
- [ ] **Step 2:** `TeamsAdapter` takes `Arc<ArcSwap<TeamsPermissions>>` (not an ad-hoc allowlist). The inbound handler enforces channel bindings + `dm_allowed_users` the way `mattermost.rs:~1146` does (drop unauthorized silently).
- [ ] **Step 3:** Update Task 8's registration so it genuinely mirrors the Slack block: build `teams_permissions` via `from_config`/`from_instance_config` and pass into each adapter (this is what makes "mirror Slack exactly" true). Remove the standalone `allowed_users` field from Tasks 1/5.
- [ ] **Step 4:** Test the permission decision (allowed/denied DM, channel binding match). Run `cargo test --lib`; commit.

## Task 11: Mention detection + channel metadata (W3/W2 — minimal core touch)

**Files:** `src/agent/channel.rs`, `src/conversation/channels.rs`
- [ ] **Step 1:** Add `teams` to the `message.source` mention-detection list in `compute_listen_mode_invocation` (`channel.rs:~3889`) so a channel @mention sets `invoked_by_mention=true` (the Teams adapter strips `<at>` tags and sets a mention marker in `metadata` — Task 4 — that this reads). Without this, the bot ignores @mentions in Teams channels.
- [ ] **Step 2:** Add a `teams` arm to `extract_platform_meta` (`channels.rs:~339`) persisting `serviceUrl` + `conversationType` so channel routing/metadata isn't dropped.
- [ ] **Step 3:** Leave slash-commands (`channel.rs:~1223`) and quiet-mode fallback (`~:3975`) as **v2** (documented). Verify DM always-respond + channel @mention both reach the agent (manual, needs a signed request).
- [ ] **Step 4:** `just gate-pr` green. Commit.

## Final whole-branch review

Dispatch the final review (most capable model) over `git diff main..HEAD`. Required checks:
- Inbound JWT validation cannot be bypassed (no path reaches dispatch without a valid Azure-signed token); allowlist enforced.
- No secrets logged; `Debug` redaction holds.
- `serviceUrl` is always read from the inbound activity / sidecar, never hardcoded.
- Only `Text` is claimed as supported (v1); other variants fail/inert per the adapter convention, not silently mis-sent.
- Default build green; conventions matched (own axum server like `webhook.rs`; config like Slack; manager registration like Slack).
- No new `MessageContent`/`OutboundResponse` variants. The only core touch is additive: `teams` added to the mention-detection source list (Task 11) — verify it's the minimal set and changes no existing platform's behaviour.
- Bindings/permissions integration uses `TeamsPermissions` (not an ad-hoc allowlist); `dm_allowed_users` + channel bindings are honoured (parity with Slack/Mattermost).
- All three `target.rs` touchpoints present (C1/C2); proactive send resolves serviceUrl from the sidecar.

## Out of scope (explicit)

- v2 capabilities (attachments, interactions, threads, reactions, cards, streaming, typing, `fetch_history` via Graph) — each is an additive match arm / adapter-side normalization onto the existing vocabulary; separate follow-on tasks.
- **Human-approval Adaptive Cards** — a separate feature on top of the existing `TaskStatus::PendingApproval` + `task_update(approved_by)` gate (card renders a pending approval; the click resolves it). Touches the task-approval layer, not just this adapter.
- Teams-exotic `invoke` (task modules, message extensions, modals) — would need new vocabulary + agent wiring.
- Config hot-reload (`watcher.rs`) — **v1.1** (boot-time registration works; Teams won't pick up config changes until restart — stated, not silent). `TeamsPermissions` itself is **v1** (Task 10); only its hot-reload wiring is deferred.
