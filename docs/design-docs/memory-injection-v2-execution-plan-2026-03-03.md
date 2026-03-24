# Memory Injection V2 — Execution Plan and Findings

Date: 2026-03-03
Branch: `merge/upstream-main-2026-02-24`
Scope: finish Memory Injection V2 in Spacebot with upstream sync, cleanup, visualization, UX, logging, docs, and final review.

## Objectives

1. Validate and harden persistence of injected history blocks.
2. Validate and harden re-injection delay behavior (turn-based timeout).
3. Remove ambient/pinned injection features entirely.
4. Implement timeline visualization for memory injection events.
5. Improve memory injection settings UI/UX.
6. Consolidate and normalize logging for memory injection flow.
7. Produce dedicated design docs under `docs/design-docs`.
8. Run final review + gates.

## Current Repository Findings

### Branch divergence

- `upstream/main...HEAD` = behind 89 / ahead 23.
- Merge from upstream is required before significant changes.

### Working tree state (before execution)

- Modified: `.devcontainer/devcontainer.json`
- Modified: `interface/package-lock.json`
- Modified: `interface/src/routes/AgentConfig.tsx`
- Modified: `src/agent/channel.rs`
- Untracked include memory-v2 plan files and migrations for injection visualization.

### Key backend code locations

- Injection prefix and block detection:
  - `src/agent/channel.rs` (`INJECTION_BLOCK_PREFIX`, `is_injection_block`, `prune_old_injection_blocks`)
- Per-channel reinjection/semantic state:
  - `src/agent/channel.rs` (`ChannelInjectionState`, `should_reinject`, `record_injection`, `prune_semantic_buffer`)
- Injection pipeline:
  - `src/agent/channel.rs` (`compute_memory_injection`)
- Injection persistence in runtime history:
  - `src/agent/channel.rs` (`run_agent_turn`, injection done in shared history guard before clone)
- Turn rollback behavior:
  - `src/agent/channel.rs` (`apply_history_after_turn`)
- Existing tests for reinjection and block pruning:
  - `src/agent/channel.rs` tests near file end.

### Key config and API locations

- Runtime memory injection config struct and defaults:
  - `src/config.rs` (`MemoryInjectionConfig`)
- Config resolution and per-agent override merge:
  - `src/config.rs` (default and agent merge paths)
- Global settings API read/write:
  - `src/api/settings.rs`
- Agent config API read/write (per-agent overrides):
  - `src/api/config.rs`

### Key frontend locations

- Global settings UI:
  - `interface/src/routes/Settings.tsx`
- Per-agent settings UI:
  - `interface/src/routes/AgentConfig.tsx`
- Client types:
  - `interface/src/api/client.ts`
- Channel timeline rendering:
  - `interface/src/routes/ChannelDetail.tsx`
- Live event integration:
  - `interface/src/hooks/useChannelLiveState.ts`

### Visualization groundwork status

- Migration files already exist:
  - `migrations/20260225000001_memory_injection_events.sql`
  - `migrations/20260225000002_memory_injection_historical.sql`
- Current backend does not yet persist/read memory injection timeline events.
- Timeline currently includes only message/branch/worker rows.

## Confirmed Gotchas

1. Re-injection timeout is in turns, not wall-clock time.
   - Controlled by `context_window_depth`.
2. `ambient/pinned` remains deeply wired (config, API, UI, runtime retrieval logic).
   - Must be removed consistently end-to-end.
3. Mismatch risk in defaults between runtime config and API fallback values.
   - Must be normalized in one pass.
4. Merge risk is high due to upstream velocity and local modifications in target files.
5. Visualization data must stay UI-only.
   - Must never be fed back to model context/compactor/cortex.

## Implementation Plan

### Phase 0 — Upstream sync and safety

1. Save temporary WIP state safely.
2. Merge `upstream/main`.
3. Reapply local WIP, resolve conflicts.
4. Confirm baseline compiles before new changes.

### Phase 1 — Persistence and reinjection hardening

1. Verify existing behavior in `run_agent_turn` and rollback path.
2. Add/adjust targeted tests:
   - block persistence across `PromptCancelled`
   - `max_injected_blocks_in_history` behavior, including `0`
   - reinjection delay boundaries for `context_window_depth`
3. Normalize setting wiring and labels related to turn-based reinjection.

### Phase 2 — Remove ambient/pinned features

1. Remove runtime fields and logic:
   - `pinned_types`, `ambient_enabled`, `pinned_limit`, `pinned_sort`
2. Remove config parse/resolve handling and validations for removed fields.
3. Remove API fields in global + per-agent config endpoints.
4. Remove UI controls and client typing for removed fields.
5. Ensure backward compatibility for older config files by ignoring legacy keys safely.

### Phase 3 — Implement timeline visualization

1. Backend event and persistence:
   - process event for memory injection
   - fire-and-forget persistence into `memory_injection_events`
2. Timeline query/model:
   - add timeline item variant for memory injection
   - include in `load_channel_timeline` union query
3. API event forwarding and SSE payload wiring.
4. Frontend:
   - add event + timeline item types
   - handle `memory_injected` in live hook
   - render compact/collapsible timeline entry in channel detail
5. Add explicit safety comments that this table/data is UI-only.

### Phase 4 — Settings UX and logging consolidation

1. Improve memory injection section wording and field grouping.
2. Make turn-based semantics explicit in labels/help text.
3. Standardize logging events and levels for injection lifecycle.
4. Ensure logs include useful stable fields (`channel_id`, counts, elapsed, filters).
5. Debug matching logic for irrelevant injected memories (no strict-mode workaround):
  - trace per-candidate provenance and decision path end-to-end
  - verify cosine input consistency (query embedding vs memory embedding source)
  - verify threshold application by `SourceSignal` branch (`FtsOnly`, `Both`, default)
  - verify post-filter ordering/budget step does not surface weak candidates

### Phase 5 — Design docs and final review

1. Add dedicated design docs in `docs/design-docs`:
   - architecture + data flow
   - settings semantics
   - safety boundaries (no model feedback loop)
2. Run final review with regression checklist.
3. Run required gates:
   - `just preflight`
   - `just gate-pr`

## Validation Matrix

- Rust compile: `cargo build`
- Rust tests: targeted + `cargo test --lib`
- Interface build: `cd interface && bun run build`
- Runtime smoke: channel flow with injected context and timeline events
- Settings smoke: global/per-agent changes reflected at runtime

## Non-goals

- Reworking unrelated memory subsystems.
- Changing historical migration files.
- Introducing additional memory injection retrieval modes beyond contextual flow.

## Session Progress (2026-03-03)

### Completed work

1. Upstream merge was completed and stabilized.
2. `src/agent/channel.rs` was reset to upstream structure and memory-injection behavior was re-ported safely.
3. Memory injection history-block persistence and pruning were reconnected in the upstream flow.
4. Ambient/pinned memory injection was removed from runtime path, API surfaces, frontend settings, and config resolution.
5. Timeline visualization for memory injection was implemented end-to-end:
  - new timeline event persistence table migration created
  - timeline union query + item variant added
  - SSE event forwarding added
  - frontend live-state + channel timeline rendering added
6. Settings/AgentConfig UI was cleaned accordingly (ambient/pinned controls removed).
7. Additional decision logging was added in memory-injection filtering to improve debugging signal.
8. Build-blocking non-exhaustive match regressions were fixed (`cortex_chat`, `api/system`).

### Commits created in this session

- `efba0c7` — Merge upstream/main and rebase memory injection settings/runtime
- `b1d7f2c` — Add memory injection timeline persistence, SSE, and UI
- `a5c5430` — Remove ambient/pinned memory injection config fields
- `ee10253` — Add detailed memory injection decision logging
- `f4878b8` — Handle memory injection event variants and fix settings JSX

### Important local-only state (not committed on purpose)

- This document and related planning docs under `memory-v2-plan/` are kept local-only per request.
- Stash entries were preserved; markdown/planning files were restored locally from stash.
- `interface/package-lock.json` remains locally modified and intentionally uncommitted.

### Open issue to debug next

- Irrelevant memories can still be injected at lower thresholds.
- This is treated as a matching-logic defect in one stage of the pipeline (not a strict-mode feature request).
- Next step is to trace candidate provenance and decision path end-to-end and isolate the exact failing step.
