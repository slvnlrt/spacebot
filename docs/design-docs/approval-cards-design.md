# Human-in-the-Loop Approval via Interactive Cards — Design

**Status:** Design (not yet planned/built). Supersedes and generalizes the Teams-specific "v3" sketch in `teams-roadmap.md`.

**Vision:** When an agent action is gated on human approval, deliver an **Approve / Reject card** to the chat channel the task came from; a click **deterministically** resolves the gate, validated against an approver allowlist, and the card is updated to show the outcome. This is designed **channel-agnostic** from the start — Teams is the first concrete adapter, but the gate, the provenance, the click-interception, the resolver, and the RBAC are shared infrastructure that any adapter (Slack, Discord, …) plugs into with a thin capability surface.

**Guiding principle (per product direction):** reuse the infra that already exists, but **build new infrastructure where it is justified** rather than contorting the design to avoid it. Below, each piece is explicitly tagged **REUSE** or **NEW**, with the justification for every NEW piece.

---

## 1. What already exists (REUSE) vs what's missing

Verified against the code on `feat/teams-channel`:

**REUSE — the approval gate (generic, not channel-specific).**
- `TaskStatus::PendingApproval` (`src/tasks/store.rs:19`); the only forward-to-executable transition is `PendingApproval → Ready` (`can_transition`, `store.rs:662-673`); `→ Backlog` (park) is also allowed.
- `POST /tasks/{n}/approve` → `approve_task` (`src/api/tasks.rs`) calls `store.update(..., status: Ready, approved_by)`; the store uses `update_with_status_transition` (`store.rs:379`) which can report the **previous** status, and persists `approved_by` via `COALESCE(?, approved_by)` (`store.rs:544` — first writer wins).
- After `→ Ready`, the cortex claims the task and the action proceeds (existing flow).

**REUSE — the interactive-card primitive.**
- Outbound: `OutboundResponse::RichMessage { interactive_elements, cards, … }` (`src/lib.rs:700-762`) with `Button { label, custom_id, style, url }` / `InteractiveElements` (`lib.rs:975-1015`). Teams renders these as Adaptive Card `Action.Submit`/`OpenUrl` (v2b); Slack uses Block Kit; Discord uses components.
- Inbound: a click arrives as `MessageContent::Interaction { action_id, block_id, values, label, message_ts }` (`lib.rs`), produced by Slack (`slack.rs` block_actions), Discord (components), and Teams (v2b `value` → Interaction). `message_ts` carries the **id of the card-bearing message** — the handle needed to update it later.

**REUSE — the notification as a trigger signal.**
- `NotificationKind::TaskApproval` is emitted whenever a task is `PendingApproval` (`api/tasks.rs:156-172` `maybe_emit_approval_notification`, `tools/task_create.rs:161-173`). Today it goes **only** to the dashboard inbox + SSE.

**MISSING (the gaps this feature must close):**
1. **No task → conversation provenance.** The `Task` struct (`store.rs:106-132`) and `CreateTaskInput` (`store.rs:134-146`) have **no** `conversation_id`/`channel`/`adapter` field — only `owner_agent_id`/`assigned_agent_id`. The generic `metadata: Value` exists but is unused for this. The `TaskApproval` notification likewise carries `agent_id` + task number but **no conversation linkage**. → We cannot currently route a card back to the originating chat.
2. **No outbound messaging path for approvals.** Nothing in `src/messaging/` references approval/`PendingApproval`/`approve_task` (verified: zero matches). The notification never reaches an adapter.
3. **No deterministic interception of an approval click.** `MessageContent::Interaction` is **flattened to its `Display` string** for the agent at `channel.rs:2077` (and `:1688`), and `:1485` makes it never coalesce — i.e. a click reaches the LLM as a plain text turn like `[interaction: approve → Approve]`. There is **no** shared point that recognizes an approval click and acts on it deterministically.
4. **No approver identity validation.** `approve_task` sets `approved_by` from the request body with **no RBAC** (free-form `Option<String>`). Teams/Slack/Discord do not enforce button-level access — **anyone in the chat can click**.
5. **No general message-update capability.** The `Messaging` trait (`traits.rs:199-252`) has `respond`/`send_status`/`broadcast`/`fetch_history`/`health_check` but **no `update_message`**. Slack/Discord can edit (they do, but only on the internal streaming path via `active_messages`); Teams has none implemented (though Bot Framework *does* support `PUT …/activities/{id}` — see §5). → "replace the card with 'Approved by X'" has no home.

---

## 2. Architecture (channel-agnostic)

```
 agent needs approval
        │  (task created PendingApproval — REUSE)
        ▼
 [A] task provenance: conversation_id + adapter + approver allowlist
     captured into task.metadata at creation              ── NEW (small)
        │
        ▼  on PendingApproval (notification hook or direct emit)
 [B] approval dispatcher: read provenance, build Approve/Reject card,
     send it proactively to the originating conversation   ── NEW
        │  (card carries reserved data: kind=approval, task, decision, expires_at)
        ▼
 user clicks  ──►  adapter normalizes to MessageContent::Interaction (REUSE, v2b/Slack/Discord)
        │
        ▼  inbound dispatch (shared, BEFORE the agent — main.rs)
 [C] approval interceptor: recognize reserved interaction namespace,
     route to the resolver instead of the agent            ── NEW (shared, not per-adapter)
        │
        ▼
 [D] approval resolver:
       1. authz: clicker's sender_id ∈ approver allowlist?  ── NEW (RBAC)  ◄── the crux
       2. idempotency: did THIS call perform PendingApproval→Ready?  (REUSE previous_status)
       3. POST /tasks/{n}/approve (or reject→Backlog), record real approver
       4. update the card → "Approved by X at T" (no buttons)
        │
        ▼
 [E] card update capability: Messaging::update_message(ref, …)
     with a follow-up-message fallback                     ── NEW (trait method + per-adapter)
        │
        ▼
 cortex claims the now-Ready task ──► action proceeds (REUSE)
```

**Why the interceptor [C] is shared, not per-adapter:** an approval click is *inbound*, and every adapter already converts it to the same `MessageContent::Interaction`. Recognizing `action_id`-namespaced approvals in **one** place (the inbound consumer in `main.rs`, before `message_tx.send` at ~`main.rs:2491`) means every current and future adapter gets approval handling for free, with no per-adapter branching. Putting it in each adapter's `respond()` (as a naive reading might suggest) would duplicate security-critical logic N times and miss the point of the abstraction.

---

## 3. The new components

### [A] Task provenance — NEW (small, REUSE the existing `metadata` field)
At task creation from a channel, the `task_create` tool **has** the channel context (it runs inside an agent turn bound to a conversation). Capture it into the existing `Task.metadata: Value`:
```jsonc
metadata: { "approval": {
  "conversation_id": "teams:19:...@thread.tacv2",
  "adapter": "teams",            // runtime_key, for routing the proactive send
  "approvers": ["29:...","..."], // approver allowlist (platform sender_ids); optional → falls back to binding/global
  "requested_by": "<agent_id>"
}}
```
No schema migration (metadata is already a JSON column). The only code change is `task_create.rs` threading the conversation/adapter through `CreateTaskInput.metadata`. **Open question O1:** approver allowlist source — per-task (as above), per-binding (config), or a global `approvers` list? Recommend: per-binding config default, overridable per-task. Never empty-means-all for approvals (fail-closed, unlike DMs).

### [B] Approval dispatcher (notification → messaging bridge) — NEW
A component that, when a task enters `PendingApproval`, reads `metadata.approval`, builds a `RichMessage` with two buttons, and sends it **proactively** to `conversation_id` on `adapter`.
- **Trigger:** hook the existing `maybe_emit_approval_notification` emission sites (`api/tasks.rs`, `tools/task_create.rs`) — emit an internal "approval requested" event alongside the dashboard notification, OR have the dispatcher subscribe to task events. Recommend a small `ApprovalDispatcher` that the API state owns, called right where the notification is emitted (single chokepoint).
- **Send path:** `MessagingManager::broadcast(conversation_id, RichMessage{...})`. **Dependency:** `broadcast` must accept `RichMessage` with buttons. Today Teams `broadcast` is Text-only (v1); Slack/Discord broadcast support varies. → small per-adapter extension (reuse the v2b card/button rendering already in Teams' `respond`).
- **Button data (reserved namespace):** `custom_id = "approval:{task_number}:approve"` / `":reject"`. Embed `expires_at`. The card stores its own message id when sent (needed for the update) — captured from the click's `message_ts` so no extra state is strictly required.

### [C] Approval interceptor (shared inbound) — NEW
In the inbound consumer (`main.rs` ~2465-2491), before forwarding to the channel:
```rust
if let MessageContent::Interaction { action_id, .. } = &message.content
    && let Some(req) = parse_approval_action(action_id) {   // "approval:{n}:{approve|reject}"
    approval_resolver.handle(req, &message).await;          // [D] — does NOT forward to the agent
    continue;
}
```
This is the only routing change. Everything non-approval flows unchanged. (Placing it here also means the click never coalesces or hits the LLM — correct, since the click must be deterministic, not interpreted.)

### [D] Approval resolver — NEW (the security crux)
```
1. parse task_number + decision + expires_at from the interaction.
2. authz: is message.sender_id ∈ approvers(task)?   ← RBAC. Deny → update card "not authorized" (or ignore), do NOT touch the gate.
3. expiry: now > expires_at → update card "expired", stop.
4. idempotency: call the store's update_with_status_transition IN-PROCESS and inspect
   its returned `previous_status`; only act if previous_status == PendingApproval (this
   call performed the transition). A later click finds Ready/Backlog → "already resolved
   by Y", no re-trigger.
5. record the REAL approver identity (sender_id) as approved_by — not a client-supplied value.
6. update the card (→ [E]) to "Approved/Rejected by <approver> at <T>".
```
Reject = transition `PendingApproval → Backlog` (park; `can_transition` allows `→ Backlog` from any state). Approve = `→ Ready`.

**⚠ C1 (Opus review) — do NOT call `POST /tasks/{n}/approve`.** That endpoint's handler (`approve_task`, `api/tasks.rs`) goes through `store.update`, which **discards** `previous_status` (`store.rs:372-377`: `.map(|result| result.task)`). The idempotency check in step 4 *requires* `previous_status`, which is only surfaced by `store.update_with_status_transition` (`store.rs:379-417`). So the resolver must call `update_with_status_transition` **directly in-process** (the inbound consumer already owns `api_state` → the task store), not the HTTP endpoint. `update_with_status_transition` opens `BEGIN IMMEDIATE` and re-reads status inside the transaction, so two simultaneous clicks serialize: the first sees `previous_status == PendingApproval`, the second sees `Ready` — making "did THIS call perform the transition" decidable atomically per row, and `COALESCE(?, approved_by)` (`store.rs:544`) records the winning approver. (Alternatively, extend `approve_task` to surface `previous_status` — but in-process is simpler and avoids a round-trip.)

### [E] Card-update capability — NEW (trait method + per-adapter + fallback)
Add to the `Messaging` trait:
```rust
/// Replace a previously-sent message (e.g. an approval card) in place.
/// Default: post a follow-up message — correct for adapters that cannot edit.
fn update_message(
    &self,
    target: &str,               // conversation
    message_ref: &str,          // the card's platform id (from Interaction.message_ts)
    response: OutboundResponse,
) -> impl Future<Output = Result<()>> + Send {
    async move { /* default: self.broadcast(target, response) — a follow-up */ }
}
```
Per-adapter:
- **Teams:** `PUT {serviceUrl}/v3/conversations/{conv}/activities/{activityId}` (Bot Framework **does** support activity update — the v2b note that "Teams messages are immutable" is incorrect; it's just unimplemented). Reuses the v2a `send_activity`/`post_activity` seam with PUT.
- **Slack:** `chat.update` (already used for streaming — `slack.rs:1092`).
- **Discord:** `edit_message` (already used for streaming — `discord.rs:330`).
- Adapters with no edit → the default follow-up message ("Approved by X").

**Justification for NEW [E]:** a card that lives forever in chat with live buttons after the decision is a correctness + security problem (stale buttons re-clickable). Updating it is essential, and no general update path exists. Building one capability (with a safe fallback) is cleaner than special-casing per call site.

---

## 4. Security model (the heart of the feature)

- **Authorization is THE control.** Without the [D].2 allowlist check, anyone in the chat approves agent actions. Treat it like the inbound JWT gate: fail-closed, validated server-side against `sender_id`, never trusting button data. Teams does not enforce button-level access; neither do Slack/Discord.
- **Identity granularity.** Today adapters capture only `sender_id` (platform user id: Teams MRI `29:…`, Slack `U…`, Discord snowflake). No email/AAD. → v3 RBAC matches platform `sender_id`s. **Enhancement (O2):** capture richer identity (Teams `from.aadObjectId`, which is stable and org-meaningful) so approver lists can be expressed as org identities, not opaque MRIs. This is a small per-adapter addition (Teams: read `activity.from.aadObjectId` into metadata).
- **The reserved namespace is not a gate.** `custom_id = approval:{n}:approve` is attacker-forgeable by any chat participant; it only *routes*. The allowlist is the gate.
- **Idempotency / no double-action** via `previous_status` (see [D].4) — do not rely on a 4xx from `/approve` (there isn't one for a re-approve).
- **Expiry** prevents stale cards from resolving long-dead requests.
- **Provenance trust:** the conversation a card is sent to comes from the task metadata captured at creation (server-side), not from anything the clicker controls.

---

## 5. Per-channel feasibility

| Capability | Teams | Slack | Discord |
|---|---|---|---|
| Render Approve/Reject buttons | ✅ Adaptive Card actions (v2b) | ✅ Block Kit | ✅ components |
| Inbound click → `Interaction` | ✅ (v2b) | ✅ | ✅ |
| Proactive send (broadcast) of a card | ⚠ broadcast is Text-only today → extend | ✅ already does `RichMessage{blocks}` (`slack.rs:1159`) | ✅ already does `RichMessage{cards,interactive_elements}` (`discord.rs:432`) |
| Update the card in place | ⚠ `PUT activities/{id}` — implement (Bot FW supports it) | ✅ `chat.update` (exists, `slack.rs:1106`) | ✅ `edit_message` (exists, `discord.rs:340`) |
| Stable approver identity | `from.id` (MRI); `aadObjectId` available (O2) | `U…` user id | snowflake |

Every channel can do the full loop; the per-adapter work is small (extend broadcast to cards; implement update). Teams ships first because v2b already built its button round-trip.

---

## 6. Open design decisions

- **O1 — approver allowlist source:** per-task metadata vs per-binding config vs global. Recommend per-binding default + per-task override; fail-closed (no implicit allow-all for approvals).
- **O2 — identity richness:** match raw `sender_id` (ship-fast) vs add `aadObjectId`/email capture (org-meaningful allowlists). Recommend ship with `sender_id`, add richer identity as a fast-follow.
- **O3 — trigger wiring:** call the dispatcher inline at the notification-emission chokepoint vs a task-event subscription. Recommend inline (one place, simplest, no event-bus dependency).
- **O4 — reject semantics:** `→ Backlog` (park, re-approvable) vs a terminal rejected state (none exists today). Recommend Backlog for v1 (no schema change); a real `Rejected` state is a separate enhancement.
- **O5 — multi-instance (v1.1):** in a multi-bot deployment the card must be sent by, and the click validated against, the **correct** adapter instance (the `adapter` runtime_key in provenance handles routing; resolver uses it).
- **O6 — what if provenance is absent** (task created outside a channel, e.g. dashboard/cortex): no card is sent; falls back to the existing dashboard-only approval. The bridge simply no-ops when `metadata.approval.conversation_id` is missing.

---

## 7. Phasing

1. **Core (channel-agnostic):** [A] provenance + [C] interceptor + [D] resolver + RBAC + idempotency, with the dashboard still as the fallback. Testable without any adapter (unit-test parse_approval_action, the resolver's authz/idempotency/expiry decisions).
2. **Teams first:** [B] dispatcher card send (extend Teams broadcast to cards) + [E] Teams `update_message` (`PUT activity`). End-to-end via cloudflared.
3. **Slack/Discord:** implement their `update_message` (mostly exists) + verify card broadcast. They inherit [A]/[C]/[D] for free — the payoff of the agnostic core.
4. **Enhancements:** O2 richer identity; O4 a real Rejected state.

## 8. Risks
- **Authorization bugs = agent actions approved by anyone.** Single most important control; review on par with the inbound JWT gate; adversarial tests (forged namespace, non-approver click, double-click, expired).
- **Provenance staleness / wrong conversation** — the card could leak a task title to the wrong chat. Capture provenance carefully; the title/body in the card is visible to everyone in that conversation (consider redaction for sensitive tasks — O7).
- **Refactoring `broadcast` to carry cards** touches the security-reviewed outbound path on each adapter — re-review.
- **Interceptor placement** must be before coalescing/agent dispatch and must not swallow non-approval interactions (which still flow to the agent as today).

---

## 8b. Opus design-review revisions (2026-06-27) — must be honored by the plan

Verdict: **Sound-with-changes.** Both self-corrections upheld (Teams `PUT activity` is real; the shared inbound interceptor is the right seam). Beyond C1 (folded into [D]) and the §5 broadcast correction (Slack/Discord already render cards → card-broadcast is **Teams-only** work, making phase 3 nearly free):

- **Interceptor placement (refines [C]).** `resolve_agent_for_message` drops messages via `require_mention` with a `continue` at `main.rs:~2211-2218`, *before* the `message_tx.send` point. An approval click carries no @-mention, so the interceptor must run **above** mention-gating (right after `conversation_id` is known), or clicks in mention-gated channels are silently swallowed.
- **Identity trust is not uniform (refines §4).** Only Teams validates inbound via JWT (`teams.rs:1576`); Slack/Discord `sender_id` is trusted from the platform's signed webhook envelope, not an independently-verifiable per-user token. State the per-adapter trust basis; do not claim JWT-grade parity off Teams. Promote `aadObjectId` capture (O2) from "fast-follow" — approver lists keyed on opaque `29:…` MRIs are brittle.
- **`broadcast` default is a silent no-op (I1).** `Messaging::broadcast` defaults to `async { Ok(()) }` (`traits.rs:225`), so an adapter that doesn't override it swallows the card and reports success. The dispatcher must treat the broadcast result as load-bearing, log/alert on failure, and **always** keep the dashboard notification as the fallback. Adapters that cannot broadcast a card should return an error, not `Ok`.
- **Card id for non-click updates (I3).** The click round-trips the card id (Slack `message_ts`, Discord `component.message.id`, Teams `reply_to_id`/`id`), so click-driven updates need no send-time state — but **expiry sweeps, dashboard-resolution-while-card-live, and cancellation have no click and thus no id**. Either (a) capture the card id at send time (requires `broadcast`/`send_activity` to return the platform id — today they return `()`), or (b) scope expiry to "reject the click when it eventually arrives" rather than proactively updating the un-clicked card. Decide in the plan.
- **Reject is non-terminal (refines O4 → §8 risk).** `can_transition` allows `Backlog → Ready` and `Done → Ready` (`store.rs:677-678`), so `→ Backlog` reject does **not** permanently block the action — a later `/approve` or re-park→approve can still execute it. A real `Rejected` terminal state is the only way to make rejection final (deferred). Call this out as a security risk, not just a phasing note.
- **Duplicated trigger (I4).** `maybe_emit_approval_notification` is called from `create_task` (`api/tasks.rs:295`) and `update_task` (`api/tasks.rs:350`), and the emission is *also* duplicated inline in `tools/task_create.rs:161-173`. `PendingApproval` is only entered via those paths (`send_agent_message.rs:244` creates **Ready** tasks, bypassing approval by design). The dispatcher hook must cover all emission sites — consider consolidating them first (DRY) so there's one chokepoint.
- **Teams serviceUrl precondition (O5/I6).** A Teams proactive send needs the conversation's `serviceUrl`, kept in the per-instance `service_urls` sidecar (`teams.rs:~1590`). Since the task originated from that conversation, the originating instance has it — **but** if `sidecar_path` is `None` (in-memory only) a restart between task-creation and approval loses it, and in a multi-instance deploy a *different* instance may dispatch. Require sidecar persistence as a precondition for the Teams card path; treat "serviceUrl unknown" as the dashboard-fallback case, never a panic.

## 9. Code seam index (verified, for the eventual implementation plan)
- Gate: `src/tasks/store.rs:19` (PendingApproval), `:379` (update_with_status_transition + previous_status), `:544` (COALESCE approved_by), `:662-673` (can_transition). `src/api/tasks.rs` approve_task + `maybe_emit_approval_notification` (`:156-172`).
- Notification: `src/notifications.rs:18-71`; emit sites `tools/task_create.rs:161-173`, `api/tasks.rs:156-172`.
- Task struct (no conv field): `src/tasks/store.rs:106-146`.
- Interaction flatten / dispatch: `src/agent/channel.rs:1485,1688,2071-2078`; inbound consumer `src/main.rs:~2465-2491`.
- Messaging trait (no update method): `src/messaging/traits.rs:199-252`. `OutboundResponse`/`MessageContent`/`Interaction`/`Button`: `src/lib.rs:583-638,700-762,975-1015`.
- Per-adapter identity: `slack.rs` block_actions (sender_id = `U…`), `discord.rs:660` (snowflake), `teams.rs` (`activity.from.id`; `aadObjectId` available).
- Update paths that exist: Slack `chat.update` `slack.rs:1092`; Discord `edit_message` `discord.rs:330`; Teams — none yet (Bot FW `PUT activities/{id}`).
