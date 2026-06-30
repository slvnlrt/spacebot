# Cross-channel messaging authorization (future reflection)

**Status:** noted, not specced. Raised 2026-06-30 by the operator while validating cross-channel sends in prod.

## The concern

The `send_message_to_another_channel` tool lets an agent send a message into **any active channel it can resolve** (`channel_store.list_active()` → `find_by_name` → `resolve_broadcast_target` → adapter `broadcast`). There is **no authorization gate** on the target channel.

With a **multi-user** deployment — where each end user has their own DM conversation with Spacebot (e.g. a Teams DM per person) — this means:

- User A (or a prompt-injection in A's conversation) can make the agent **post into User B's private DM channel**, since B's channel is in the active list and resolvable by id/name.
- More generally, any conversation can inject messages into any other conversation across adapters (web → Teams DM, Slack → another user's Telegram, etc.).

This is a **privacy / authorization gap**, distinct from admin/operator auth ([[admin-portal-auth-security]]) and from end-user identity / user-scoped memories (the "I7" topic). It only bites once more than one end user shares an instance.

## Current behavior (2026-06-30)

No gating. The tool resolves the target purely by string match against active channels and broadcasts. (Two resolution **bugs** on the Teams path were fixed this day — `find_by_name` case-sensitivity and the serviceUrl routing-key prefix mismatch — which is what surfaced the broader question; the *authorization* question is separate and unaddressed.)

## Axes to think through (before multi-user)

1. **Who may target a channel?** Options, roughly increasing strictness:
   - same-conversation only (no cross-channel) — too strict, kills the feature;
   - same-**user** only (a conversation may only target channels owned by the same end-user identity);
   - same-**agent** only (an agent may target its own channels, not another agent's);
   - per-channel **allowlist / opt-in** (a channel must consent to receive proactive cross-channel messages);
   - **operator policy** (instance config decides the default + per-agent overrides).
2. **User-requested vs agent-proactive.** A user explicitly asking "send X to channel Y" should be gated by whether *that requesting user* has rights to Y — not just whether the agent technically can.
3. **Channel visibility.** Which channels should even be *listed/resolvable* from a given conversation? Hiding other users' channels from `list_active` (scoped listing) is a first, cheap layer of defense-in-depth.
4. **Identity dependency.** A real answer needs an end-user identity model (who owns a channel), which ties into the broader identity/RBAC reflection — same prerequisite as user-scoped memories.

## Why it matters now

Single-operator today (one person), so low immediate risk. But the moment a second human gets a DM with the same Spacebot instance, cross-channel send becomes a way to message them unsolicited (or leak/route content across users). Worth designing before opening the instance to more than one person.
