# Message and Memory Flow

High-level view of how messages flow through Spacebot and where memory enters the picture. Covers the current architecture and the memory injection system.

## 1. Message Lifecycle (Channel Turn)

A channel turn is the full cycle from receiving a user message to producing a response.

```
                         ┌────────────────────────────┐
                         │     Messaging Adapter       │
                         │  (Discord / Telegram / …)   │
                         └─────────────┬──────────────┘
                                       │ InboundMessage
                                       ▼
                         ┌────────────────────────────┐
                         │    Channel Event Loop       │
                         │    (select! on rx + events) │
                         └─────────────┬──────────────┘
                                       │
                          ┌────────────┴──────────────┐
                          │ should_coalesce?           │
                          │  yes → buffer, wait        │
                          │  no  → flush + handle      │
                          └────────────┬──────────────┘
                                       │
                          ┌────────────┴──────────────┐
                          │ System retrigger?          │
                          │  (source == "system")      │
                          │  yes → skip memory inject  │
                          │  no  → compute injection   │
                          └────────────┬──────────────┘
                                       │
                    ┌──────────────────┴──────────────────┐
                    ▼                                      ▼
         ┌────────────────────┐              ┌─────────────────────────┐
         │ Build System Prompt │              │ Memory Injection        │
         │                    │              │ (compute pre-hook)      │
         │ - Identity files   │              │                         │
         │   (SOUL/USER/ID)   │              │ - Hybrid search on msg  │
         │ - Memory bulletin  │              │ - Pinned types (opt-in) │
         │   (cortex, ~60min) │              │ - Dedup (ID + semantic) │
         │ - Skills           │              │ - Budget enforcement    │
         │ - Worker caps      │              │                         │
         │ - Status block     │              │ Returns: Option<String> │
         │ - Conv context     │              └────────────┬────────────┘
         │ - Coalesce hint    │                           │
         └─────────┬──────────┘                           │
                   │                                      │
                   ▼                                      ▼
         ┌──────────────────────────────────────────────────┐
         │                  run_agent_turn                   │
         │                                                  │
         │  1. Register per-turn tools (reply, branch, …)   │
         │  2. Clone history from shared state               │
         │  3. Inject memory context into clone (ephemeral)  │
         │  4. agent.prompt(user_text).with_history(clone)   │
         │  5. LLM agentic loop (tools ↔ responses)         │
         │  6. Write new messages back to shared history     │
         │     (injection NOT persisted)                     │
         │  7. Remove per-turn tools                         │
         └──────────────────────┬───────────────────────────┘
                                │
                    ┌───────────┴───────────┐
                    ▼                       ▼
         ┌──────────────────┐    ┌─────────────────────┐
         │ Compactor Check   │    │ Memory Persistence   │
         │                  │    │ Check                │
         │ token estimate / │    │                      │
         │ context window   │    │ Every N messages,    │
         │                  │    │ spawn a silent       │
         │ >80% → background│    │ branch that reads    │
         │ >85% → aggressive│    │ recent history and   │
         │ >95% → emergency │    │ saves key memories.  │
         └──────────────────┘    └──────────────────────┘
```

## 2. Memory Sources in the System Prompt

The system prompt is assembled fresh every turn. It contains several memory-adjacent sources that exist **independently** of memory injection:

| Source | Origin | Refresh rate | Content |
|--------|--------|-------------|---------|
| **Identity files** | `SOUL.md`, `IDENTITY.md`, `USER.md` on disk | Startup (hot-reloadable) | Core personality, user info |
| **Memory bulletin** | Cortex LLM synthesis | ~60 min | ~500 word briefing of current knowledge |
| **Status block** | Workers/branches via `set_status` | Every turn | Active workers, recent completions |
| **Skills** | Skill definitions on disk | Startup (hot-reloadable) | Available capabilities |
| **Conversation context** | First message metadata | Per-conversation | Server name, channel name, source |

Memory injection complements these — it provides **specific, per-message-relevant memories** that the bulletin and identity files don't cover.

## 3. Memory Injection Pipeline (Detail)

This is the `compute_memory_injection()` pre-hook that runs before each LLM turn.

### V2 Architecture

```
User message text
        │
        ├──────────────────────────────────────────┐
        │                                          │
        ▼                                          ▼
 ┌─────────────────────────┐          ┌─────────────────────────────┐
 │ Pinned Types (opt-in)   │          │ Hybrid Search (always)      │
 │                         │          │                             │
 │ For each in config:     │          │ 1. Embed user message       │
 │   get_by_type(type, n)  │          │ 2. Vector search (LanceDB)  │
 │   sort by recent or     │          │ 3. FTS search (LanceDB)     │
 │   importance             │          │ 4. Graph traversal from     │
 │                         │          │    high-importance seeds     │
 │ Default: [] (disabled)  │          │ 5. RRF fusion               │
 │ Example: [todo, goal]   │          │                             │
 └────────────┬────────────┘          └──────────────┬──────────────┘
              │                                      │
              │  pinned_pool                         │  contextual_pool
              │                                      │
              └──────────────┬───────────────────────┘
                             │
                    ┌────────▼────────┐
                    │  Deduplication   │
                    │                 │
                    │  1. ID in context window     │
                    │     (injected_ids map)       │
                    │  2. ID already in batch      │
                    │     (seen_ids set)           │
                    │  3. Cosine > threshold       │
                    │     (semantic_buffer)        │
                    └────────┬────────┘
                             │
                    ┌────────▼────────┐
                    │ Budget Enforce   │
                    │                 │
                    │ pinned first    │
                    │ (guaranteed)    │
                    │ then contextual │
                    │ until max_total │
                    └────────┬────────┘
                             │
                    ┌────────▼────────────────────┐
                    │ Format                       │
                    │                              │
                    │ [Pinned context]             │
                    │ [Todo] Fix auth token ...    │
                    │ [Goal] Ship v2.0 by Feb ...  │
                    │                              │
                    │ [Relevant to this message]   │
                    │ [Decision] JWT over sessions  │
                    │ [Fact] Auth module in src/... │
                    └──────────────────────────────┘
```

### Where injection happens in code

```
handle_message()
    │
    ├─ compute_memory_injection(&user_text)  ← pre-hook, returns Option<String>
    │
    └─ run_agent_turn(user_text, system_prompt, ..., injected_context)
           │
           ├─ let mut history = shared_history.clone()
           ├─ history.push("[Context from memory]: ...")  ← ephemeral injection
           ├─ agent.prompt(user_text).with_history(&mut history)
           │      ... LLM agentic loop ...
           ├─ shared_history.extend(new_messages_only)   ← injection NOT saved
           └─ return
```

The injection is **ephemeral**: it exists only in the cloned history passed to the LLM. It is never persisted to the conversation database. This means:
- The LLM sees the context as if it "knows" it
- Future compaction summaries capture the *effect* (the LLM's informed response) but not the raw injected memories
- Re-injection is controlled by `ChannelInjectionState` tracking what was injected at which turn

## 4. Memory Write Paths

Memories enter the system through three paths:

```
┌─────────────────────────────────────────────────────────────┐
│                     Memory Write Paths                       │
├─────────────────────────────────────────────────────────────┤
│                                                             │
│  1. Branch-initiated (conversation)                         │
│     Channel → Branch (with memory tools)                    │
│       → memory_recall (search) → memory_save (write)        │
│     Used when: LLM decides to remember something            │
│                                                             │
│  2. Memory persistence branch (automatic)                   │
│     Channel → every N messages → silent branch              │
│       → reads recent history → extracts key facts           │
│       → memory_save                                         │
│     Used when: Automatic background persistence             │
│                                                             │
│  3. Compactor-initiated (during compaction)                  │
│     Compactor → compaction worker                           │
│       → summarizes old messages → extracts memories         │
│       → memory_save                                         │
│     Used when: Context window getting full                  │
│                                                             │
│  4. Cortex-initiated (system-level)                         │
│     Cortex → bulletin generation → memory_save              │
│     Used when: Hourly refresh, consolidation (future)       │
│                                                             │
└─────────────────────────────────────────────────────────────┘
```

## 5. Memory Read Paths

Memories are read through multiple, independent paths:

```
┌──────────────────────────────────────────────────────────────┐
│                     Memory Read Paths                         │
├──────────────────────────────────────────────────────────────┤
│                                                              │
│  A. Memory Injection (pre-hook, every turn)                  │
│     → Hybrid search (vector + FTS + graph + RRF)             │
│     → Pinned types (opt-in SQL queries)                      │
│     → Dedup + budget → inject into ephemeral history         │
│     WHO: Channel (automatic, no LLM decision)                │
│     WHEN: Before every user message turn                     │
│                                                              │
│  B. Memory Bulletin (cortex, periodic)                       │
│     → memory_recall across multiple dimensions               │
│     → LLM synthesis into ~500 word briefing                  │
│     → Cached in RuntimeConfig, read by all channels          │
│     WHO: Cortex (LLM-driven)                                 │
│     WHEN: Every ~60 minutes                                  │
│                                                              │
│  C. Branch Recall (on-demand, LLM-driven)                    │
│     → Branch uses memory_recall tool                         │
│     → Hybrid search → LLM curates results                   │
│     → Returns conclusion to channel                          │
│     WHO: Branch (LLM initiative)                             │
│     WHEN: When the LLM decides to think deeply               │
│                                                              │
│  D. Worker Context (via branch-and-spawn)                    │
│     → Branch recalls memories → enriches task description    │
│     → Worker gets enriched task (no memory tools itself)     │
│     WHO: Branch → Worker handoff                             │
│     WHEN: Complex tasks needing memory context               │
│                                                              │
└──────────────────────────────────────────────────────────────┘
```

### How the read paths complement each other

| Path | Scope | Latency | LLM cost | Contextual? |
|------|-------|---------|----------|-------------|
| **Injection** | Per-message | < 200ms | None | Yes (hybrid search on message) |
| **Bulletin** | Global | ~60 min stale | 1 LLM call/hour | No (periodic snapshot) |
| **Branch Recall** | Deep dive | ~2-5s | 1+ LLM calls | Yes (LLM-curated) |
| **Branch-and-Spawn** | Task enrichment | ~3-8s | 1+ LLM calls | Yes (task-specific) |

Memory injection handles the fast, automatic, per-message path. The bulletin provides global awareness. Branch recall and branch-and-spawn handle deep, LLM-curated retrieval when the agent needs to think carefully. They are complementary, not competing.

## 6. Compaction and Injection Interaction

When the compactor runs, it affects the injection state:

```
Normal operation:
  Turn 1: inject memories A, B, C → record in injected_ids
  Turn 2: inject D, E (A,B,C skipped — still in window)
  Turn 3: inject F (A-E skipped)
  ...

After compaction:
  Compactor summarizes old messages → summary replaces them
  injected_ids is NOT cleared (the essence of injected memories
  lives in the compaction summary — clearing would cause duplicates)
  semantic_buffer entries older than context_window_depth are pruned

After emergency truncation:
  Old messages dropped without LLM summary
  injected_ids is NOT cleared (same reason — re-injection of
  already-discussed memories would confuse the LLM)
```

The key insight from the V1 review (see `memory-v2-plan/05-REVUE_PERFECTIONNISTE.md`): do NOT reset injection state on compaction. The compaction summary captures the *effect* of previously injected memories. Re-injecting the raw memories would create a feeling of déjà-vu for the LLM.

## Reference

- [memory-injection.md](memory-injection.md) — detailed design doc for the injection system (V1 audit, V2 architecture, config, implementation plan)
- `src/agent/channel.rs` — channel event loop, `handle_message`, `compute_memory_injection`, `run_agent_turn`
- `src/agent/compactor.rs` — compaction thresholds, emergency truncation, worker spawning
- `src/memory/search.rs` — `hybrid_search`, RRF, `SearchConfig`
- `src/agent/branch.rs` — branch lifecycle
- `src/agent/worker.rs` — worker lifecycle
- `AGENTS.md` — full architecture overview
