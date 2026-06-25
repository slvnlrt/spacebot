# Microsoft Teams Adapter — Roadmap (v1.1 / v2 / v3)

> Design document for the work deferred out of the Teams adapter **v1** (single bot, text + @mention). It is grounded in (a) the Microsoft Bot Framework / Teams platform wire format (cited to Microsoft Learn) and (b) the actual spacebot code seams (cited `file:line`, verified against the `feat/teams-channel` branch). It is a design, not an implementation plan — each version below should get its own `writing-plans` plan before execution.

## Where v1 stands

v1 (shipped on `feat/teams-channel`) implements: own axum inbound server (`POST /api/messages` + `/health`), Azure JWT validation (RS256, `iss`/`aud`/`exp` required), `Activity` → `InboundMessage::Text`, `TeamsPermissions` enforcement, outbound `respond`/`broadcast` of **text** with an SSRF-guarded serviceUrl, registration mirroring Slack, `target.rs` routing, @mention detection, `extract_platform_meta`. **One bot per port** (named instances are parsed but deliberately not started — `src/main.rs:3612` warns).

## Cross-cutting facts (verified against code — these shape every version below)

1. **The capability vocabulary already exists end-to-end.** `MessageContent` (`src/lib.rs:586-609`: `Text` / `Media{attachments}` / `Interaction{action_id,block_id,values,label,message_ts}`) and `OutboundResponse` (`src/lib.rs:698-762`: Text/ThreadReply/File/Reaction/RemoveReaction/Ephemeral/RichMessage/ScheduledMessage/`StreamStart`/`StreamChunk`/`StreamEnd`/Status). v2 adds *adapter-side normalization onto these*, not new variants.

2. **⚠ `MessageContent::Interaction` is flattened to text for the agent.** It is NOT a structured event downstream: `src/agent/channel.rs:1485` (never batched), `:1688-1690` and `:2077` render it via `Display` (`(message.content.to_string(), Vec::new())`, comment: "so the LLM sees plain text"). `channel_dispatch.rs`/`worker.rs` have zero `Interaction` references. **Implication:** an Adaptive Card button click delivered as `Interaction` reaches the agent as a plain user turn like `[interaction: approve → Approve]`. This is fine for "let the agent react to a click in conversation" (v2), but it is **not** a reliable gate for a security-sensitive approval (v3) — see v3.

3. **The `PendingApproval` gate is API/dashboard-only and has no identity check.** `TaskStatus::PendingApproval` (`src/tasks/store.rs:19`); tasks are *created into* it; the only forward transition **to an executable state** is `PendingApproval → Ready` (`store.rs:662-680`) — note `can_transition` also allows the universal `→ Backlog` (any status) and identity no-ops (`current == next`), so a pending task can also be parked to `Backlog`. After `→ Ready` the cortex can claim it (`Ready` only, `cortex.rs:3905`). `POST /tasks/{n}/approve` (`src/api/tasks.rs:424`) sets `approved_by` from the request body **with no validation** (`approved_by: Option<String>`, free-form, even null). `NotificationKind::TaskApproval` (`notifications.rs:21`) is emitted to the **dashboard inbox + SSE only** (`api/tasks.rs:162`, `tools/task_create.rs:163`) — **no messaging-adapter path exists**. **Implication:** v3 must *add* both an outbound chat path and approver-identity validation; it cannot merely "hook in".

4. **The manager/runtime-key infra already supports named instances.** `MessagingManager` keys adapters by `name()` in a `HashMap` and runs each `start()` in an independent Tokio task with its own retry (`manager.rs:70-101`); `register_and_start` hot-swaps by name (`manager.rs:108`). `binding_runtime_adapter_key(platform, Some(name))` (`config/types.rs:1956`) and `apply_runtime_adapter_to_conversation_id` (`messaging/traits.rs:358`) are ready. The only thing missing for multi-bot is the *inbound listener* model (one-per-adapter today → must become shared) and main.rs starting the instances.

---

# v1.1 — Multi-bot + config hot-reload

**Goal:** support several Teams bots in one process (the limitation documented in v1), and bring Teams to parity with other adapters' live config reload.

## Problem

Each `TeamsAdapter::start()` binds its **own** `TcpListener` on `teams_config.port` (the bind is at `src/messaging/teams.rs:920`, inside `start()`); `main.rs` would pass the same `port` to every instance → the 2nd+ bind fails and the manager retries ~12× then gives up (`manager.rs` retry task). Teams is also absent from `spawn_file_watcher` (`src/config/watcher.rs:26-41`), so permission edits only apply on a full re-init, not the lightweight watcher path.

## Design: one shared listener, demux by JWT `aud`

Multiple Azure Bot registrations **can share one messaging endpoint URL** — confirmed standard. Each inbound Activity carries a Connector-signed JWT whose **`aud` claim = the target bot's App ID** ([Bot Connector authentication](https://learn.microsoft.com/en-us/azure/bot-service/rest-api/bot-framework-rest-connector-authentication?view=azure-bot-service-4.0)). The JWKS/OpenID metadata is a **single shared document** for all Bot Framework bots (`login.botframework.com`), so one key cache validates every bot's signature.

Refactor from "N adapters, N listeners" to **one shared inbound server + a registry of bot instances**:

```
                POST /api/messages   Authorization: Bearer <JWT aud=<BotAppId>>
                          │
            ┌─────────────▼──────────────┐
            │  Shared axum listener      │   (one port, one public route, one TLS cert)
            │  + shared JwksCache        │
            └─────────────┬──────────────┘
   secure order:          │
   1 decode header (kid)  │   2 verify RS256 sig (shared JWKS)
   3 iss == api.botframework.com
   4 aud ∈ {our registered App IDs}   ← reject 403 if not ours
   5 exp/nbf (+5min skew)   6 serviceUrl claim == Activity.serviceUrl
            ┌─────────────┴──────────────┐
            │  route to instance[aud]    │
            └──────┬───────────────┬──────┘
          instance "teams"   instance "teams:hr"
          TokenProvider A     TokenProvider B     ← per-bot app_id/secret
```

**Security invariant (must hold):** signature verification (step 2) precedes the `aud`-membership check (step 4). Reading `aud` to *select* a bot is fine; *accepting* a request based on `aud` before the signature is verified is a critical flaw — an attacker could set `aud` to one of our App IDs with a forged signature. Microsoft documents this explicitly. The shared JWKS lets us always verify the signature without first knowing which bot is targeted (key chosen by `kid`).

Diagram **step 6** (`serviceUrl` JWT claim == `Activity.serviceUrl`) is a **net-new addition**, not parity: v1 does NOT validate a `serviceUrl` claim (its only serviceUrl control is the URL-pattern SSRF guard `is_allowed_service_url`). Connector tokens do not always carry a `serviceUrl` claim, so treat step 6 as an optional defense-in-depth check (skip if the claim is absent), layered on top of the existing SSRF guard — not a replacement for it.

**Outbound is per-bot.** Each instance has its own `TeamsTokenProvider` (its own `app_id`/`client_secret`); the bot replying must use the token provider for *that* instance (cross-bot tokens are rejected 401 by the Connector).

### Code shape

- New `TeamsListener` (or `TeamsHub`) owning: the axum server, the shared `JwksCache`, a `HashMap<AppId, TeamsInstance>` where `TeamsInstance` holds `{ runtime_key, permissions, token_provider, service_urls, sidecar_path, inbound_tx }`. The handler validates, looks up the instance by `aud`, applies that instance's permissions, and dispatches on that instance's stream.
- **Split the JWT validator (required API change).** Today `validate_token_with_key` (`teams.rs:439-467`) bakes the audience check into `decode()` via `set_audience(&[expected_aud])` + `validate_aud=true` — incompatible with demux, because you don't know `expected_aud` until *after* decoding, and a single expected aud would reject all-but-one bot. Refactor into: (i) verify signature + `iss` + `exp`/`nbf` (no aud), then (ii) a separate `aud ∈ {our registered App IDs}` membership check, then route. **Assume `aud` is a single App-ID string and reject array-valued `aud` for routing** (Connector tokens carry a single string; an array makes "route to instance[aud]" ambiguous). Two registered bots cannot share an `aud` (App IDs are unique per registration), so that ambiguity is a non-issue.
- **Move the SSRF guard + serviceUrl sidecar *into* per-instance dispatch.** In v1 these live in `handle_messages` with a single `runtime_key`/`service_urls`/`sidecar_path` (`teams.rs` ~`:1195-1206`, guard `is_allowed_service_url` `:718`). In the shared listener, serviceUrl capture/persistence and the outbound guard must use the *resolved instance's* `runtime_key`/`service_urls`/`sidecar_path` (keyed via `apply_runtime_adapter_to_conversation_id(&instance.runtime_key, …)`), or serviceUrls cross-contaminate between bots. Re-run the v1 inbound security review after the refactor.
- `TeamsAdapter` becomes a thin per-instance object whose `start()` registers with the shared listener instead of binding its own socket. Only the FIRST instance (or a dedicated owner) binds the port; the rest attach to the registry. (Alternative: a single non-`Messaging` hub that the manager sees as N logical adapters via N `InboundStream`s fed from the shared server.)
- `TeamsInstanceConfig` gains nothing new for the shared-listener path (no per-instance port). Validation: all instances share `teams_config.port`.

### Alternative considered: per-instance ports

Give each `TeamsInstanceConfig` its own `port`. Simpler code (no demux), but requires N public HTTPS routes / N Azure messaging-endpoint URLs and N reverse-proxy entries. Rejected as the default: operationally heavier and scales poorly. (Could be offered as an escape hatch, but the shared-listener is the recommended production pattern.)

### Watcher hot-reload (parity)

Mirror the Mattermost arm in `src/config/watcher.rs` (closest model — fallible `new()`):
1. Import `TeamsPermissions` (`watcher.rs:4-7`).
2. Add `teams_permissions: Option<Arc<ArcSwap<TeamsPermissions>>>` param (after `signal_permissions`, `:35`).
3. Permission `.store()` swap block after Signal (`:260`): `TeamsPermissions::from_config(cfg, &config.bindings)` → `perms.store(...)`.
4. Capture `teams_permissions` in the hot-start closure (`:267-273`).
5. Hot-start block after Mattermost (`:687`): default + named-instance loop via `binding_runtime_adapter_key("teams", …)` + `register_and_start`.
6. Forward `teams_permissions` at the two main.rs call sites (`:1838-1853`, `:1856-1871`) — handle already exists at `:3572-3573`.
   (Note: named-instance permission hot-update is a known TODO across all adapters — `watcher.rs:308-310` etc. — so v1.1 inherits that limitation unless we also fix it.)

### Touchpoints
`src/messaging/teams.rs` (listener/hub refactor), `src/main.rs` (start instances via `binding_runtime_adapter_key`, drop the v1 single-instance `warn!` at `:3612`), `src/config/watcher.rs` (6 mirror steps), `src/config/types.rs` (validation: instances share the port).

### Risks
- **Single point of failure:** one listener for all bots — a panic in the shared handler silences every bot. Mitigate: isolate per-instance work in Tokio tasks; a bad instance config is excluded from the registry, not fatal.
- **Emulator/dev issuer:** the (archived but usable) Bot Framework Emulator and single-tenant bots use a different `iss`/OpenID endpoint; the shared validator must key the issuer/openid-config per instance config if dev mode is supported (see "Local testing" below).
- Refactoring the listener risks regressing the v1 single-bot security chain — re-run the full inbound security review.

### Tests
Unit: the `aud`-routing decision (token aud ∈ {ours} → correct instance; aud ∉ {ours} → 403; forged-sig → 401 before routing). Integration (see Local testing): two instances on one port, a signed token per `aud` lands on the right stream.

---

# v2 — Richer capabilities (additive, onto existing vocabulary)

**Goal:** beyond text — attachments, cards, card-button interactions, thread replies, typing, streaming. Each maps onto an existing `MessageContent`/`OutboundResponse` variant; **no new agent wiring** (cross-cutting fact #1/#2).

## Capability → variant mapping (verified shapes)

| Capability | Wire shape (cited) | Maps to | Difficulty |
|---|---|---|---|
| Inbound image/file | `message` w/ `attachments[]`; inline image `contentUrl` needs the bot's bearer to fetch; file uses `application/vnd.microsoft.teams.file.download.info` (`content.downloadUrl`), **personal scope only** ([files](https://learn.microsoft.com/en-us/microsoftteams/platform/bots/how-to/bots-filesv4)) | `MessageContent::Media{attachments}` | Medium |
| Outbound Adaptive/Hero card | `message` w/ `attachments[].contentType = application/vnd.microsoft.card.adaptive` (or `.hero`) ([rich cards](https://learn.microsoft.com/en-us/azure/bot-service/rest-api/bot-framework-rest-connector-add-rich-cards?view=azure-bot-service-4.0)) | `OutboundResponse::RichMessage` (`cards` → Adaptive Card; **no Slack-`blocks` equivalent** — `slack.rs` uses `blocks`, `discord.rs` uses `cards`, so Teams follows the Discord side) | Easy–Medium |
| **Card button click** | `Action.Submit` → inbound **`message`** w/ `value:{…}` (merged button `data` + inputs); `text` empty ([card actions](https://learn.microsoft.com/en-us/microsoftteams/platform/task-modules-and-cards/cards/cards-actions)) | `MessageContent::Interaction{action_id, values, …}` (same rail as Slack `block_actions`) | Easy (wire) — see caveat |
| Thread reply | channel `conversation.id = "19:…@thread.tacv2;messageid=<root>"`; reply = POST to that id w/ `replyToId`; new thread = POST without `;messageid=` ([channel convos](https://learn.microsoft.com/en-us/microsoftteams/platform/bots/how-to/conversations/channel-and-group-conversations)) | `OutboundResponse::ThreadReply` | Medium |
| Typing | outbound `type:"typing"` Activity (no text); expires ~3s, resend periodically | `OutboundResponse::Status(Thinking)` / `send_status` | Easy |
| Streaming | `type:"typing"` Activities w/ `entities[].type="streaminfo"` (`streamId`/`streamType`/`streamSequence`), final `type:"message"`; **personal chat only**, cumulative text, ≤1 req/s, 2-min cap ([streaming](https://learn.microsoft.com/en-us/microsoftteams/platform/bots/streaming-ux)) | `OutboundResponse::StreamStart/StreamChunk/StreamEnd` | Hard |
| Reaction (inbound) | `type:"messageReaction"` w/ `reactionsAdded/Removed` ([reactions](https://learn.microsoft.com/en-us/microsoftteams/platform/bots/how-to/conversations/subscribe-to-conversation-events)) | **no inbound variant exists** — gap (could map to `Interaction` or be dropped) | Easy (if dropped) |
| Reaction (outbound by bot) | Teams SDK reactions endpoint (not core BF) | `OutboundResponse::Reaction` (Discord supports; Slack-style) | Medium |
| `invoke` (task modules, msg extensions, `Action.Execute`) | `type:"invoke"`, **synchronous HTTP-body response within 5s** ([dialogs](https://learn.microsoft.com/en-us/microsoftteams/platform/task-modules-and-cards/task-modules/task-modules-bots)) | none — needs new vocabulary + a sync-response path our adapter doesn't have | Hard — **out of scope** (except the v3 approval use of `Action.Submit`, which is async) |

## Caveat that shapes v2 — interactions are text to the agent

Because `Interaction` is flattened to its `Display` string before the LLM (fact #2), a card-button click in v2 lets the agent *react in conversation* ("you clicked Approve") but the agent cannot structurally branch on it. That is acceptable for conversational buttons (quick replies, menu choices) where the agent's text understanding suffices. Anything that must *deterministically* gate logic (an approval) must be handled at the adapter/task layer, not via the agent — that is exactly v3.

`Action.Execute` (the `invoke`-based universal action that can refresh a card in place) requires a **synchronous 5-second** HTTP-body response, which our fire-and-forget `respond` model doesn't provide. v2 should use **`Action.Submit`** (async `message` with `value`) and treat `Action.Execute`/task-modules/messaging-extensions as out of scope.

## Phasing
- **v2a (easy wins):** outbound Adaptive/Hero cards (`RichMessage.cards`), typing (`Status`), inbound `Media` attachments (personal scope). Each is an additive `respond`/`activity_to_inbound` arm + a test.
- **v2b:** thread replies (`ThreadReply` + the `;messageid=` conversation-id handling), card-button `Action.Submit` → `Interaction`, outbound reactions.
- **v2c (hard):** streaming (the `streaminfo` protocol — personal chat only; map `StreamStart/Chunk/End`, send cumulative text, throttle to 1/s, handle the 2-min cap).

### Touchpoints
`src/messaging/teams.rs` only (additive `respond` match arms + `activity_to_inbound` arms for attachments/`messageReaction`/card `value`), plus the `Activity` serde structs (add `attachments`, `value`, `entities`, `reactionsAdded/Removed`). The capability table's "maps to" column is the contract; nothing in `agent/` changes.

### Risks
- Inbound file bytes need the bot's bearer (inline images) or anonymous `downloadUrl` (files) — and files are personal-scope only; channel files need Microsoft Graph (out of scope). Document the scope limits.
- Streaming is genuinely hard (cumulative text, throttle, personal-only) and low ROI for an agent that posts whole messages — consider deferring indefinitely.
- `RichMessage` from the agent carries Slack `blocks` AND Discord `cards`; Teams should consume `cards` and fall back to `text`, ignoring `blocks` (mirror how each adapter ignores the other's payload — `slack.rs:1025`, `discord.rs`).

---

# v3 — Human-in-the-loop approval via Adaptive Cards

**Goal:** when an agent action is gated on human approval, deliver an **Approve/Reject Adaptive Card** to a Teams conversation; a click resolves the gate. This is the feature Hermes Agent shipped (Allow Once / Session / Always / Deny card; "clicking a button resolves the approval inline and replaces the card" — [hermes docs](https://hermes-agent.nousresearch.com/docs/user-guide/messaging/teams)).

## Why this is a separate feature, not "v2 buttons"

The existing approval primitive is the **task** gate (`PendingApproval → Ready`), and a card click must *deterministically* flip it — but `Interaction` reaches the agent as text (fact #2), which can't reliably gate a task. So the approval card must be handled at the **adapter + task-API layer**, bypassing the agent:

```
agent needs approval ──► task created PendingApproval ──► TaskApproval notification
                                                              │  (NEW: messaging route)
                                                              ▼
                          Teams adapter sends an Adaptive Card (Approve/Reject) to the bound conversation
                                                              │
                user clicks Approve ──► Action.Submit ──► inbound message w/ value{action,task_id}
                                                              │
            adapter intercepts (reserved action namespace), does NOT route to the agent:
              1 authz: value.task approver ∈ allowed?  (from.aadObjectId)   ← NEW capability
              2 idempotency: task still PendingApproval?  (atomic)
              3 call POST /tasks/{n}/approve (PendingApproval → Ready)
              4 updateActivity: replace card with "Approved by X at T"
                                                              │
                          cortex claims the now-Ready task ──► action proceeds
```

## Verified gaps this feature must close (not just hook into — fact #3)

1. **No outbound messaging path for `TaskApproval`.** Today it goes only to the dashboard inbox/SSE (`api/tasks.rs:162`, `tools/task_create.rs:163`). v3 must add a route: on `TaskApproval` emission (or a new hook), if a Teams binding is configured for approvals, send the card. Decide where this lives (a notification → messaging bridge, or a direct call in the approval-emitting sites).
2. **No approver identity validation.** `approved_by` is free-form/nullable (`task_update.rs:69`, no RBAC — `api/tasks.rs:436`). v3 must validate the clicker (`activity.from.aadObjectId`) against an allowlist of approvers (per task / per binding) **before** calling `/approve`, and record the real approver identity in `approved_by`. Teams does NOT enforce button-level access — anyone in the chat can click ([Teams approvals guide](https://laurakokkarinen.com/the-ultimate-guide-to-microsoft-teams-based-approvals/)).
3. **Adapter must intercept approval submits**, not deliver them as `Interaction` to the agent. Reserve an `action_id`/`value` namespace (e.g. `value.kind == "task_approval"`) the inbound handler recognises before normalization.

## Card mechanics (verified)

- **Use `Action.Submit`** (not `Action.Execute`): async, no 5-second sync-response constraint, works with our fire-and-forget model. The click arrives as a `message` Activity with `value` = merged button `data` ([card actions](https://learn.microsoft.com/en-us/microsoftteams/platform/task-modules-and-cards/cards/cards-actions)). Embed correlation data: `value:{kind:"task_approval", task_id, decision:"approve"|"reject", expires_at}`.
- **Update the card after the decision:** `PUT {serviceUrl}/v3/conversations/{conversationId}/activities/{activityId}` with the result card ("Approved by X at T", no buttons) ([connector REST](https://learn.microsoft.com/en-us/azure/bot-service/rest-api/bot-framework-rest-connector-add-rich-cards?view=azure-bot-service-4.0)). The `activityId` is the original card's id (stored when sent; also arrives as `replyToId` on the click). Add an `update_activity` outbound method (also reusable by v2 streaming).
- **Identity:** `activity.from.aadObjectId` is the stable approver id; `from.id` is the Teams MRI. Validate against the allowlist.
- **Idempotency (the adapter must self-detect — the API will NOT reject a double-click):** the atomic `COALESCE(?, approved_by)` (`store.rs:544`) makes the *first* writer win on the recorded approver, which is good. BUT `can_transition` allows identity transitions (`current == next`, `store.rs:663`), so a second `/approve` while the task is already `Ready` returns **200, not an error** — you cannot rely on a 4xx to detect the duplicate. The adapter must therefore check state itself: read the task's status (or use `update_with_status_transition`'s `previous_status`, `store.rs:379-411`) and only show "Approved by X" / proceed when *this* call performed the `PendingApproval → Ready` transition; a later click finding the task already non-`PendingApproval` shows "already resolved" without re-triggering anything.
- **Expiry:** embed `expires_at`; reject late clicks with an "expired" card (no built-in expiry in Teams).

## Touchpoints
`src/messaging/teams.rs` (send approval card; intercept `task_approval` submits before normalization; `update_activity`; approver allowlist check), a notification→messaging bridge (new — where `TaskApproval` is emitted: `api/tasks.rs`, `tools/task_create.rs`), `src/tasks`/`src/api/tasks.rs` (optional: an approver-allowlist field + identity recording), config (which Teams binding/conversation receives approval cards; the approver allowlist).

## Risks / security
- **Authorization is the crux** — without the allowlist check, anyone in the chat approves agent actions. This is the single most important control. Treat it like the inbound JWT gate.
- Persist the pending-card ↔ task mapping (activityId, task_id, conversation, approvers) durably (survives restart) — reuse the sidecar pattern or the DB.
- The card lives in chat forever if not updated; always `updateActivity` on resolution and on expiry.
- Cross-cutting with v1.1: in a multi-bot setup, the approval card must be sent by (and the click validated against) the correct instance's token.

## Tests
Unit: the approval-submit interception decision (recognise `kind:"task_approval"`; authz allow/deny by `aadObjectId`; idempotent second click; expired). Integration: a signed `Action.Submit` POST → `/tasks/{n}/approve` called once → task goes `Ready` → card updated.

---

# Cross-cutting: testing without a public HTTPS endpoint

All three versions are buildable and unit-testable with no public endpoint. To exercise the *running* inbound path (and to let a developer drive the bot by hand), two complementary approaches — both deferred but enabling:
- **In-process integration test:** spin up the adapter's axum server on an ephemeral port, mint a JWT with a test-injected signing key (the validator already has an injectable `validate_token_with_key`), POST a real `Activity`, assert it lands on the `InboundStream` with permission enforcement + 401 on a bad token. No external tooling. This is the biggest current verification gap (the running server has only unit coverage).
- **Dev-mode for a local client:** the (archived but functional) Bot Framework Emulator or the maintained **Microsoft 365 Agents Toolkit Test Tool** talks to `http://localhost:<port>/api/messages`. With auth left empty it sends no Bearer → our gate 401s, so a clearly-dangerous, localhost-only, off-by-default `dev_skip_auth` (or an emulator-issuer acceptance path) is needed to use it. The **Bot Connector REST protocol our adapter speaks is unchanged** under the Agents SDK rebrand — only the dev tooling moved.

---

# Recommended sequencing

1. **v1.1 multi-bot + watcher** — unblocks real multi-bot deployments and closes the documented v1 limitation; medium effort, well-defined, mostly within `teams.rs` + `watcher.rs` + `main.rs`. Do the watcher parity even if multi-bot is deferred (it's small and closes a real gap).
2. **v2a** (cards out, typing, inbound attachments) — high value, low risk, `teams.rs`-only additive arms.
3. **v3 approval cards** — high value but the largest blast radius (new outbound notification path + approver RBAC + adapter interception); needs its own plan and a security review on par with the inbound JWT gate.
4. **v2b/v2c** (threads, button-interactions, streaming) — opportunistic; streaming is low-ROI and may stay deferred.

Each version → its own `writing-plans` plan, Opus-reviewed, executed subagent-driven, with the same security discipline as v1 (adversarial review of every auth/identity path).
