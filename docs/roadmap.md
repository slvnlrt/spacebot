# MVP Roadmap

Tracking progress toward a working Spacebot that can hold a conversation, delegate work, manage memory, and connect to at least one messaging platform.

For each piece: reference IronClaw, OpenClaw, Nanobot, and Rig for inspiration, but make design decisions that align with Spacebot's architecture. Don't copy patterns that assume a monolithic session model.

---

## Current State

**What exists and compiles:**
- Project structure — all modules declared, module root pattern (`src/memory.rs` not `mod.rs`)
- Error hierarchy — thiserror domain enums (`ConfigError`, `DbError`, `LlmError`, `MemoryError`, `AgentError`, `SecretsError`) wrapped by top-level `Error` with `#[from]`
- Config loading — env-based with compaction/channel defaults, data dir setup
- Database connections — SQLite (sqlx) + LanceDB + redb. SQLite migrations for all tables (memories, associations, conversations, heartbeats). Migration runner in `db.rs`.
- LLM — `SpacebotModel` implements Rig's `CompletionModel` trait (completion, make, stream stub). Routes through `LlmManager` via direct HTTP to Anthropic and OpenAI. Handles tool definitions in requests and tool calls in responses.
- Memory — types (`Memory`, `Association`, `MemoryType`, `RelationType`), SQLite store (full CRUD + associations), LanceDB embedding storage + vector search (cosine) + FTS (Tantivy), fastembed (all-MiniLM-L6-v2, 384 dims), hybrid search (vector + FTS + graph traversal + RRF fusion), `MemorySearch` bundles store + lance + embedder. Maintenance (decay/prune stubs).
- Agent structs — Channel, Branch, Worker, Compactor, Cortex. Core LLM calls within agents are simulated — the surrounding infrastructure is real.
- StatusBlock — event-driven updates from `ProcessEvent`, renders to context string
- SpacebotHook — implements `PromptHook<M>` with tool call/result event emission, leak detection regexes (`LazyLock`)
- CortexHook — implements `PromptHook<M>` for system observation
- Messaging — `Messaging` trait with RPITIT + `MessagingDyn` companion + blanket impl. `MessagingManager` with adapter registry. Discord/Telegram/Webhook adapters are empty stubs.
- Tools — 11 tools implement Rig's `Tool` trait. Structs hold dependencies (Arc<MemorySearch>, event channels, etc.). `definition()` with JSON schemas, `call(&self, args)` with `Self::Error`. Legacy wrapper functions preserved for backward compat.
- Custom `ToolServerHandle` deleted — `AgentDeps.tool_server` uses `rig::tool::server::ToolServerHandle` directly
- Core types in `lib.rs` — `InboundMessage`, `OutboundResponse`, `StatusUpdate`, `ProcessEvent` (with `agent_id` on all variants), `AgentDeps`, `Agent`, `AgentId`, `ProcessId`, `ProcessType`, `ChannelId`, `WorkerId`, `BranchId`
- `main.rs` — CLI (clap), tracing, config/DB/LLM/memory init, event loop, graceful shutdown. Tool server creation deferred to Phase 4.

**What's missing:**
- No identity files (SOUL.md, IDENTITY.md, USER.md)
- Agent LLM calls are simulated (placeholder `tokio::time::sleep` instead of real `agent.prompt()`)
- ToolServer not yet created — tools implement `Tool` but aren't registered on a server yet (happens in Phase 4)
- Streaming not implemented (SpacebotModel.stream() returns error)
- Secrets and settings stores are empty stubs

**Known issues:**
- `embedding.rs` `embed_one()` async path creates a new fastembed model per call instead of sharing via Arc (the sync `embed_one_blocking()` works correctly and is what `hybrid_search` uses)
- Arrow version mismatch in Cargo.toml: `arrow = "54"` vs `arrow-array`/`arrow-schema` at `"57.3.0"` — should align or drop the `arrow` meta-crate
- `lance.rs` casts `_distance`/`_score` columns as `Float64Type` — LanceDB may return `Float32`, risking a runtime panic on cast
- `SpacebotHook` missing `agent_id` field — `ProcessEvent` variants now require `agent_id: AgentId` but the hook only has `process_id` and `process_type`. Needs `agent_id` added to the hook struct and its constructors.
- `MemoryRecallTool::call()` relevance score mapping is wrong — after `curate_results()` reorders/filters, indexing `search_results[idx]` by curated index doesn't map to the correct original result. Should look up score by memory ID.
- `MemorySaveTool` dropped `channel_id` — `save_fact()` takes `channel_id` but `MemorySaveArgs` has no such field, so `memory.with_channel_id()` is never called. Silently drops the association.
- `ReplyTool` takes `Arc<InboundMessage>` at construction time, but Rig's `ToolServer` registers tools once and shares across calls. The reply tool would need per-message reconstruction, conflicting with ToolServer's model. Needs rethinking in Phase 4.
- `definition()` on all tools hand-writes JSON schemas instead of using the `JsonSchema` derive on `Args` types. The derives are unused dead weight. Either use `schemars::schema_for!()` to generate from the derive, or drop the derive (hand-written schemas have richer descriptions but create a maintenance burden where field changes require updating two places).

---

## ~~Phase 1: Migrations and LanceDB~~ Done

- [x] SQLite migrations for all tables (memories, associations, conversations, heartbeats)
- [x] Inline DDL removed from `memory/store.rs`, `conversation/history.rs`, `heartbeat/store.rs`
- [x] `memory/lance.rs` — LanceDB table with Arrow schema, embedding insert, vector search (cosine), FTS (Tantivy), index creation
- [x] Embedding generation wired into memory save flow (`memory_save.rs` generates + stores)
- [x] Vector + FTS results connected into hybrid search via `MemorySearch` struct
- [x] `MemorySearch` bundles `MemoryStore` + `EmbeddingTable` + `EmbeddingModel`, replaces `memory_store` in `AgentDeps`

---

## ~~Phase 2: Wire Tools to Rig~~ Done

- [x] Reshape tools as structs with dependency fields (MemorySaveTool, MemoryRecallTool hold `Arc<MemorySearch>`; BranchTool, CancelTool, RouteTool, SpawnWorkerTool hold channel_id + event_tx; SetStatusTool holds worker_id + event_tx; ReplyTool holds `Arc<InboundMessage>`; ShellTool, FileTool, ExecTool are stateless)
- [x] Implement Rig's `Tool` trait on all 11 tools (`const NAME`, `Args`, `Output`, `definition()`, `call()`)
- [x] Delete the custom `ToolServerHandle` wrapper — `AgentDeps.tool_server` is now `rig::tool::server::ToolServerHandle`
- [x] Implement `PromptHook<M>` on `SpacebotHook` — tool call/result event emission, leak detection
- [x] Implement `PromptHook<M>` on `CortexHook` — observation logging
- [x] Added `Clone`/`Debug` impls for `MemorySearch`, `EmbeddingTable`, `MemoryStore`
- [x] Legacy wrapper functions preserved for backward compatibility
- [ ] Create shared ToolServer for channel/branch tools (deferred to Phase 4 — needs real agent construction)
- [ ] Create per-worker ToolServer factory for task tools (deferred to Phase 4)

**Fixups needed before Phase 4:**
- `SpacebotHook` needs `agent_id: AgentId` field — `ProcessEvent` variants now require it but the hook doesn't have it
- `MemoryRecallTool::call()` — relevance score uses curated index to look up `search_results`, but curation reorders/filters so the index mapping is wrong
- `MemorySaveTool` — missing `channel_id` field on `MemorySaveArgs`, so `memory.with_channel_id()` is never called
- `ReplyTool` — constructed per-message (`Arc<InboundMessage>`), but ToolServer registers tools once. Needs rethinking for actual channel wiring.
- `definition()` hand-writes JSON schemas while `Args` types derive `JsonSchema` (unused). Pick one approach.

---

## ~~Phase 3: System Prompts and Identity~~ Done

- [x] `prompts/` directory with all 5 prompt files (CHANNEL.md, BRANCH.md, WORKER.md, COMPACTOR.md, CORTEX.md)
- [x] `identity/files.rs` — `Prompts` struct, `load_all_prompts()`, per-type loaders
- [x] `conversation/context.rs` — `build_channel_context()` (prompt + status + identity memories + high-importance memories), `build_branch_context()`, `build_worker_context()`
- [x] `conversation/history.rs` — `HistoryStore` with save_turn, load_recent, compaction summaries

---

## Phase 4: Model Routing + The Channel (MVP Core)

Implement model routing so each process type uses the right model, then wire the channel as the first real agent.

- [ ] Implement `RoutingConfig` — process-type defaults, task-type overrides, fallback chains (see `docs/routing.md`)
- [ ] Add `resolve_for_process(process_type, task_type)` to `LlmManager`
- [ ] Implement fallback logic in `SpacebotModel` — retry with next model in chain on 429/502/503/504
- [ ] Rate limit tracking — deprioritize 429'd models for configurable cooldown
- [ ] Wire `AgentBuilder::new(model).preamble(&prompt).hook(spacebot_hook).tool_server_handle(tools).default_max_turns(5).build()`
- [ ] Replace placeholder message handling with `agent.prompt(&message).with_history(&mut history).max_turns(5).await`
- [ ] Wire status block injection — prepend rendered status to each prompt call
- [ ] Connect conversation history persistence (HistoryStore already implemented) to channel message flow
- [ ] Fire-and-forget DB writes for message persistence (`tokio::spawn`, don't block the response)
- [ ] Test: send a message to a channel, get a real LLM response back

**Reference:** `docs/routing.md` for the full routing design. Rig's `agent.prompt().with_history(&mut history).max_turns(5)` is the core call. The channel never blocks on branches, workers, or compaction.

---

## Phase 5: Branches and Workers

Replace simulated branch/worker execution with real agent calls.

- [ ] Branch: wire `agent.prompt(&task).with_history(&mut branch_history).max_turns(10).await`
- [ ] Branch result injection — insert conclusion into channel history as a distinct message
- [ ] Branch concurrency limit enforcement (already scaffolded, needs testing)
- [ ] Worker: resolve model via `resolve_for_process(Worker, Some(task_type))`, wire `agent.prompt(&task).max_turns(50).await` with task-specific tools
- [ ] Interactive worker follow-ups — repeated `.prompt()` calls with accumulated history
- [ ] Worker status reporting via set_status tool → StatusBlock updates
- [ ] Handle stale branch results and worker timeout via Rig's `MaxTurnsError` / `PromptCancelled`

**Reference:** No existing codebase has context forking. Branch is `channel_history.clone()` run independently. Workers get fresh history + task description. Rig returns chat history in error types for recovery.

---

## Phase 6: Compactor

Wire the compaction workers to do real summarization.

- [ ] Implement compaction worker — summarize old turns + extract memories via LLM
- [ ] Emergency truncation — drop oldest turns without LLM, keep N recent
- [ ] Pre-compaction archiving — write raw transcript to conversation_archives table
- [ ] Non-blocking swap — replace old turns with summary while channel continues

**Reference:** IronClaw's tiered compaction (80/85/95 thresholds, already implemented). The novel part is the non-blocking swap.

---

## Phase 7: Webhook Messaging Adapter

Get a real end-to-end messaging path working.

- [ ] Implement WebhookAdapter (axum) — POST endpoint, InboundMessage production, response routing
- [ ] Implement MessagingManager.start() — spawn adapters, merge inbound streams via `select_all`
- [ ] Implement outbound routing — responses flow from channel → manager → correct adapter
- [ ] Optional sync mode (`"wait": true` blocks until agent responds)
- [ ] Wire the full path: HTTP POST → InboundMessage → Channel → response → OutboundResponse → HTTP response
- [ ] Test: curl a message in, get a response back

**Reference:** IronClaw's Channel trait and ChannelManager with `futures::stream::select_all()`. The Messaging trait and MessagingDyn companion are already implemented.

---

## Phase 8: End-to-End Integration

Wire everything together into a running system.

- [ ] main.rs orchestration — init config, DB, LLM, memory, tools, messaging, start event loop
- [ ] Event routing — ProcessEvent fan-in from all agents, dispatch to appropriate handlers
- [ ] Channel lifecycle — create on first message, persist across restarts, resume from DB
- [ ] Test the full loop: message in → channel → branch → worker → memory save → response out
- [ ] Graceful shutdown — broadcast signal, drain in-flight work, close DB connections

---

## Post-MVP

Not blocking the first working version, but next in line.

- **Streaming** — implement `SpacebotModel.stream()` with SSE parsing, wire through messaging adapters with block coalescing (see `docs/messaging.md`)
- **Cortex** — system-level observer, memory consolidation, decay management. No reference codebase for this.
- **Heartbeats** — scheduled tasks with fresh channels. Circuit breaker (3 failures → disable).
- **Telegram adapter** — real messaging platform integration.
- **Discord adapter** — thread-based conversations map naturally to channels.
- **Secrets store** — AES-256-GCM encrypted credentials in redb.
- **Settings store** — redb key-value with env > DB > default resolution.
- **Memory graph traversal during recall** — walk typed edges (Updates, Contradicts, CausedBy) during search.
- **Multi-channel identity coherence** — same soul across conversations, cortex consolidates across channels.
