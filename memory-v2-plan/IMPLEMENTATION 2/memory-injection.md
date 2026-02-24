# Memory Injection

Pre-hook system that silently injects relevant memories into the LLM context before each turn. Inspired by MemOS's systematic injection approach. No LLM call in the retrieval path — pure search + dedup.

## History

### Origin (../ETUDE INITIALE/)

The original "Memory V2" plan (see `../ETUDE INITIALE/` for the full archive) identified a core problem: Spacebot's memory recall was entirely LLM-driven. A Branch had to *decide* to call `memory_recall`, which meant the agent was sometimes "amnésique" — it simply forgot to search. The MemOS research (Phase 0 study) showed that **systematic, silent injection** (pre-hook) before every LLM call was far more reliable than optional tool-based recall.

Key insights from the MemOS study:
- The LLM should never "decide" to search. Search happens automatically.
- Results are injected invisibly — the LLM sees context as if it always knew it.
- Deduplication prevents repeated injection of the same memory across turns.
- The injection pipeline must be fast (< 200ms) with zero LLM calls.

### V1 Implementation (current code)

The V1 was implemented in `src/agent/channel.rs` as `compute_memory_injection()`. It followed the original plan closely:

**4 retrieval sources:**

| # | Source | Query | Config setting |
|---|--------|-------|----------------|
| 1 | SQL | `get_by_type(Identity, limit)` | `identity_limit` |
| 2 | SQL | `get_high_importance(threshold, limit)` | `importance_threshold`, `important_limit` |
| 3 | SQL | `get_recent_since(since, limit)` | `recent_threshold_hours`, `recent_limit` |
| 4 | Hybrid | `hybrid_search(user_text, config)` | `vector_search_limit` |

**3-stage deduplication:**
1. ID already injected within context window → skip
2. ID already seen in current batch → skip
3. Cosine similarity > threshold against semantic buffer → skip

**Injection point:** After cloning history, before `agent.prompt()`. Injected as a user message `[Context from memory]: ...`. Not persisted to permanent history.

**State:** `ChannelInjectionState` stored in RAM on the `Channel` struct (not in the shared `ChannelState`). Contains `injected_ids: HashMap<String, usize>` (memory_id → turn) and `semantic_buffer: VecDeque<Vec<f32>>`.

### V1 Config (9 settings)

```toml
[memory_injection]
enabled = true
recent_threshold_hours = 1
identity_limit = 10
important_limit = 10
recent_limit = 10
vector_search_limit = 20
context_window_depth = 50
semantic_threshold = 0.85
importance_threshold = 0.8
```

All settings are hot-reloadable via ArcSwap and editable from the web UI (Settings → Memory Injection).

## V1 Audit — Problems Found

### P1 — Redundant and sequential retrieval

The 3 SQL queries run sequentially (each `await`ed). Worse, `hybrid_search` (source 4) *also* calls `get_high_importance(0.8, 20)` internally for graph seed — so there are potentially two calls to the same SQL query with different parameters. Identity memories (importance=1.0) appear in source 1 *and* source 2 (importance > threshold) *and* potentially source 4 (graph seed). Heavy overlap, wasted work.

### P2 — `vector_search_limit` poorly wired

It feeds `SearchConfig::max_results` but `hybrid_search` primarily uses `max_results_per_source` (default 50) for vector/FTS/graph queries, and the final `.take()` also uses `max_results_per_source`. The UI setting is nearly inoperative.

### P3 — No global budget

No cap on total injected memories. Worst case: `identity_limit(10) + important_limit(10) + recent_limit(10) + hybrid(~50) = 80 memories`. No final relevance sort.

### P4 — No parallelism

Three independent SQL queries run in series. The hybrid search (embedding + vector + FTS + graph traversal) is the heaviest. All blocking the channel turn.

### P5 — Unstructured output

Format is a flat text wall: `[Type] content` per line. No relevance score, no grouping, no priority signal for the LLM.

### P6 — Semantic buffer never resets properly

The `semantic_buffer` only drops entries beyond `MAX_ENTRIES` (100). A memory semantically close to something injected at turn 5 can still block a *different* memory at turn 50, even though the original is long gone from context.

### P7 — 3 of 4 sources are context-blind

Sources 1 (Identity), 2 (Important), 3 (Recent) return the **same results regardless of the user message**. Only source 4 (hybrid) is contextual. A technical question gets the same 10 Identity + 10 Important memories as a "hello."

### P8 — Graph seed threshold hardcoded

`hybrid_search()` uses `get_high_importance(0.8, 20)` with hardcoded 0.8, ignoring the configured `importance_threshold`. If config says 0.7, memories with importance 0.7-0.8 are fetched by source 2 but NOT by the graph seed of source 4.

### P9 — No injection tracing

No logging of *which* memories were injected or *why* (which source brought them). Impossible to debug or measure injection quality.

### P10 — No token budget awareness

Raw text injection with no token counting. Ten 500-token Identity memories = 5000 tokens before the user message even appears.

## Design Discussion — Retrieval Architecture

### Why not per-type configuration?

We considered letting users configure injection limits per memory type (8 types × limit × sort = 16-24 settings). Rejected because:

1. **Users can't tune it.** Is 5 Identity too many? Is 3 Decision enough? It depends on DB size, conversation topic, memory length. Nobody will iterate on this.
2. **Defaults become the architecture.** If 95% of users keep defaults, we've just replaced hardcoded values with hardcoded-but-configurable values. More code, same result.
3. **Still context-blind.** Per-type SQL queries return the same memories regardless of the user message. The contextual signal still comes entirely from hybrid search.

### Why not remove static sources entirely?

We considered making hybrid search the sole retrieval source (Direction 1 in our brainstorm). The hybrid search already does vector + FTS + graph + RRF — if a memory is relevant, it surfaces. If it doesn't surface, injecting it would be noise.

This is correct for most cases but misses one real gap: **the cortex bulletin lag**. The cortex refreshes every ~60 minutes. A Todo created 5 minutes ago won't be in the bulletin. The bulletin also compresses — it summarizes "working on project X" instead of listing 5 specific Todos. Static type injection bridges this gap with raw detail.

However, this gap only matters for **specific types in specific deployment contexts**:
- **Personal assistant / small team**: Active Todos and Goals are valuable ambient context
- **Community Discord bot**: Hundreds of users, no coherent Todos/Goals — static injection is pure noise

### Chosen architecture: Hybrid search primary + opt-in pinned types

**Primary engine:** A single `hybrid_search` call, properly parameterized. This is the main retrieval source. Contextual, semantic, covers importance and recency through scoring.

**Optional ambient awareness:** A configurable list of "pinned types" — memory types that are always injected regardless of the message. Disabled by default. Designed for personal assistant deployments where Todos/Goals provide valuable ongoing context between cortex bulletins.

**Global budget:** Hard cap on total injected memories across all sources, with pinned types getting guaranteed slots and hybrid filling the rest.

## V2 Architecture

### Retrieval pipeline

```
User message arrives
        │
        ▼
┌─ Pinned types (if configured) ─┐      ┌─ Hybrid search ──────────┐
│ For each type in pinned_types:  │      │ embed(user_text)          │
│   get_by_type(type, limit)      │      │ vector + FTS + graph      │
│   sorted by pinned_sort         │      │ RRF fusion                │
│                     (parallel)  │      │ importance/recency boost  │
└─────────────┬───────────────────┘      └────────────┬──────────────┘
              │                                       │
              ▼                                       ▼
         pinned_pool                           contextual_pool
              │                                       │
              └──────────────┬────────────────────────┘
                             ▼
                    ┌─ Deduplication ─┐
                    │ 1. ID in context window → skip     │
                    │ 2. ID seen in batch → skip         │
                    │ 3. Cosine > threshold → skip       │
                    └────────┬───────┘
                             ▼
                    ┌─ Budget enforcement ─┐
                    │ pinned_pool first (guaranteed slots) │
                    │ contextual_pool fills remainder      │
                    │ total ≤ max_total                    │
                    └────────┬─────────┘
                             ▼
                    ┌─ Format & inject ─┐
                    │ [Pinned context]  │
                    │ [Relevant to this message] │
                    └───────────────────┘
```

### Config (v2)

```toml
[memory_injection]
enabled = true

# Contextual search — the main engine (always active when enabled)
search_limit = 20           # max results from hybrid search
contextual_min_score = 0.01 # min hybrid score for contextual candidates
semantic_threshold = 0.85   # cosine dedup threshold
context_window_depth = 50   # turns before re-injection allowed

# Ambient awareness — opt-in, off by default
ambient_enabled = false      # master switch for pinned retrieval
pinned_types = []            # e.g. ["todo", "goal"]
pinned_limit = 3             # per pinned type
pinned_sort = "recent"       # "recent" or "importance"

# Global budget
max_total = 25               # hard cap across all sources
```

### Recommended usage (soft guidance)

`pinned_types` is intentionally flexible, but usage should be conservative:

- **Community bot (Discord server / many users):** keep ambient awareness disabled
    - `pinned_types = []`
    - Rely on contextual hybrid search only
- **Personal assistant / small team:** enable a small ambient set
    - Recommended: `pinned_types = ["todo", "goal"]`
    - Optional: add `"decision"` when long-running projects need stronger continuity

Avoid pinning broad/noisy types (`fact`, `event`, `observation`) unless there is a very specific reason, as they can dilute prompt focus.

This is a **soft recommendation** only. The system does not hard-restrict types; operators keep full control.

Changes from V1:
- **Removed:** `identity_limit`, `important_limit`, `recent_limit`, `recent_threshold_hours`, `importance_threshold` — these were static SQL shortcuts that hybrid search replaces
- **Added:** `contextual_min_score` — explicit relevance floor for contextual hybrid results
- **Added:** `ambient_enabled` — master toggle to disable pinned retrieval at runtime without clearing `pinned_types`
- **Added:** `pinned_types`, `pinned_limit`, `pinned_sort` — opt-in ambient awareness
- **Added:** `max_total` — global budget (V1 had no cap)
- **Renamed:** `vector_search_limit` → `search_limit` (it's the hybrid search budget, not just vector)

### Hybrid search improvements

The `hybrid_search` in `src/memory/search.rs` needs adjustments to work well as the sole contextual source:

1. **Wire `search_limit` to `max_results_per_source`** — the per-source limit that actually controls vector/FTS/graph query sizes
2. **Use configured threshold for graph seed** — replace hardcoded `0.8` in `get_high_importance` call with config value, or use a separate `graph_seed_threshold` 
3. **Score enrichment (future)** — add importance and recency bias to the RRF score so that important/recent memories win tiebreaks:
   ```
   score_final = rrf_score + α × importance + β × recency_decay(age)
   ```
   With small α, β (0.005-0.01) so relevance dominates but importance/recency break ties.

### Deduplication improvements

1. **Semantic buffer aligned to context window** — prune entries older than `context_window_depth` turns, not just by count. This prevents stale-dedup (P6).
2. **Source tagging** — track which source brought each memory (for tracing).

### Format improvements

Structured output with sections:

```
[Pinned context]
[Todo] Fix the auth module token refresh — assigned by Oscar
[Goal] Ship v2.0 by end of February

[Relevant to this message]
[Decision] We chose JWT over session tokens for the API (importance: 0.8)
[Fact] The auth module is in src/auth/ with 3 files (importance: 0.6)
```

This gives the LLM a clear signal: pinned context is always-on background, relevant context is specific to this message.

### Observability

- `tracing::debug!` per injected memory with source tag (pinned vs contextual), score, and type
- `tracing::info!` summary: `"memory injection: 2 pinned + 8 contextual = 10 total, took 45ms"`
- Future: metrics (count, latency, source breakdown)

## Implementation plan

### Step 1 — Config: replace V1 schema with V2

**Files:** `src/config.rs`

Replace `MemoryInjectionConfig` fields:

```rust
// V1 (remove)                          // V2 (add)
enabled: bool,                          enabled: bool,              // keep
recent_threshold_hours: i64,            search_limit: usize,        // renamed from vector_search_limit
identity_limit: i64,                    semantic_threshold: f32,    // keep
important_limit: i64,                   context_window_depth: usize,// keep
recent_limit: i64,                      pinned_types: Vec<String>,  // NEW (default: [])
vector_search_limit: usize,             pinned_limit: i64,          // NEW (default: 3)
context_window_depth: usize,            pinned_sort: String,        // NEW (default: "recent")
semantic_threshold: f32,                max_total: usize,           // NEW (default: 25)
importance_threshold: f32,
```

Update `TomlMemoryInjectionConfig` to match (all fields optional).
Update `Default` impl with new defaults.
Update `resolve` logic (the `.map(|mi| { ... })` block around line 2810).
Update the `reload()` method (line 3341) — `memory_injection` is already stored via ArcSwap.

Validate `pinned_types` values against `MemoryType::ALL` names at parse time (reject unknown types with a warning).
Validate `pinned_sort` is `"recent"` or `"importance"`.

### Step 2 — API: update settings endpoints

**File:** `src/api/settings.rs`

Replace `MemoryInjectionResponse` fields:
```rust
struct MemoryInjectionResponse {
    enabled: bool,
    search_limit: usize,
    semantic_threshold: f32,
    context_window_depth: usize,
    pinned_types: Vec<String>,
    pinned_limit: i64,
    pinned_sort: String,
    max_total: usize,
}
```

Replace `MemoryInjectionUpdate` fields (all `Option`).

Update `get_global_settings`:
- Read new fields from TOML (`search_limit`, `pinned_types`, `pinned_limit`, `pinned_sort`, `max_total`)
- Remove reads for old fields (`identity_limit`, `important_limit`, `recent_limit`, `recent_threshold_hours`, `importance_threshold`)
- `pinned_types` reads as a TOML array of strings

Update `update_global_settings`:
- Write new fields to TOML
- Remove writes for old fields
- `pinned_types` writes as a TOML array

### Step 3 — Hybrid search: fix wiring

**File:** `src/memory/search.rs`

**3a. Wire `search_limit` to `max_results_per_source`.**

Currently `compute_memory_injection` sets `SearchConfig { max_results: vector_search_limit, ..Default::default() }` but `hybrid_search` uses `max_results_per_source` (default 50) for the actual vector/FTS/graph queries. Fix: set `max_results_per_source` from config, not just `max_results`.

**3b. Add `graph_seed_threshold` to `SearchConfig`.**

Currently hardcoded at line ~189: `self.store.get_high_importance(0.8, 20)`. Replace with `config.graph_seed_threshold` (default 0.8). This makes the graph seed configurable without exposing it to the injection config directly.

```rust
pub struct SearchConfig {
    // ... existing fields ...
    /// Importance threshold for graph seed memories. Default: 0.8.
    pub graph_seed_threshold: f32,
    /// Maximum graph seed memories. Default: 20.
    pub graph_seed_limit: i64,
}
```

### Step 4 — Refactor `compute_memory_injection`

**File:** `src/agent/channel.rs`

Replace the 4-query sequential pipeline with:

```rust
async fn compute_memory_injection(&mut self, user_text: &str) -> Option<String> {
    let config = self.deps.runtime_config.memory_injection.load();
    if !config.enabled { return None; }

    let start = std::time::Instant::now();
    let memory_search = self.deps.memory_search();
    let store = memory_search.store();

    // === Phase 1: Parallel retrieval ===

    // Pinned types (SQL, only if configured)
    let pinned_futures = config.pinned_types.iter().map(|type_name| {
        let memory_type = type_name.parse::<MemoryType>(); // validated at config parse
        let sort = &config.pinned_sort;
        let limit = config.pinned_limit;
        async move {
            let Ok(memory_type) = memory_type else { return vec![] };
            store.get_by_type(memory_type, limit).await.unwrap_or_default()
            // TODO: if pinned_sort == "recent", use get_by_type_recent variant
        }
    });

    // Hybrid search (always)
    let search_config = SearchConfig {
        mode: SearchMode::Hybrid,
        max_results: config.search_limit,
        max_results_per_source: config.search_limit,
        ..Default::default()
    };
    let search_future = memory_search.search(user_text, &search_config);

    // Run pinned + hybrid in parallel
    let (pinned_results, search_result) = tokio::join!(
        futures::future::join_all(pinned_futures),
        search_future
    );

    let pinned_pool: Vec<Memory> = pinned_results.into_iter().flatten().collect();
    let contextual_pool: Vec<Memory> = search_result
        .unwrap_or_default()
        .into_iter()
        .map(|r| r.memory)
        .collect();

    // === Phase 2: Dedup ===
    // (same 3-stage dedup as before: context window, batch ID, semantic)

    // === Phase 3: Budget enforcement ===
    // pinned_pool first (guaranteed slots), then contextual, up to max_total

    // === Phase 4: Format ===
    // Two sections: [Pinned context] and [Relevant to this message]

    // === Phase 5: Tracing ===
    let elapsed = start.elapsed();
    tracing::info!(
        pinned_count, contextual_count, total,
        elapsed_ms = elapsed.as_millis(),
        "memory injection complete"
    );
}
```

Key changes from V1:
- 4 sequential queries → 2 parallel phases (pinned SQL + hybrid search via `tokio::join!`)
- No more `get_high_importance` / `get_recent_since` standalone queries (hybrid search covers these)
- Budget enforcement with `max_total` cap
- Source tagging (pinned vs contextual) for format and tracing

### Step 5 — Dedup improvements

**File:** `src/agent/channel.rs` (ChannelInjectionState)

**5a. Align semantic buffer to context window.**

Currently `semantic_buffer` is a `VecDeque<Vec<f32>>` pruned by count (`MAX_ENTRIES = 100`). Change to also track the turn number for each entry and prune entries older than `context_window_depth`. This prevents stale-dedup (P6).

```rust
pub struct ChannelInjectionState {
    pub injected_ids: HashMap<String, usize>,
    /// (embedding, turn_number) — prune entries where turn < current - context_window_depth
    pub semantic_buffer: VecDeque<(Vec<f32>, usize)>,
}
```

Update `is_semantically_duplicate` call to filter out stale entries before comparison.

**5b. Add source tag to injected memories.**

Add an enum for tracing:
```rust
enum InjectionSource { Pinned, Contextual }
```

Track in a lightweight struct alongside the memory during the dedup phase for logging.

### Step 6 — Structured output format

**File:** `src/agent/channel.rs`

Replace the flat `[Type] content` format with two sections:

```
[Pinned context]
[Todo] Fix the auth module token refresh
[Goal] Ship v2.0 by end of February

[Relevant to this message]
[Decision] We chose JWT over session tokens for the API
[Fact] The auth module is in src/auth/ with 3 files
```

If pinned pool is empty, skip the `[Pinned context]` header.
If contextual pool is empty, skip the `[Relevant to this message]` header.

### Step 7 — Frontend: update UI

**Files:** `interface/src/api/client.ts`, `interface/src/routes/Settings.tsx`

**7a. TypeScript types** (`client.ts`):

Replace `MemoryInjectionConfig` / `MemoryInjectionConfigUpdate`:
```typescript
export interface MemoryInjectionConfig {
    enabled: boolean;
    search_limit: number;
    semantic_threshold: number;
    context_window_depth: number;
    pinned_types: string[];
    pinned_limit: number;
    pinned_sort: string;
    max_total: number;
}
```

**7b. Settings UI** (`Settings.tsx`):

Remove the 6 old controls: Identity Type, High Importance, Recent, Vector Search, Recent Window, Importance Threshold.

New layout:
```
┌─ Memory Injection ──────────────────────────────┐
│ [Toggle] Enable Memory Injection                │
│                                                 │
│ ── Contextual Search ──                         │
│ Search Limit:    [NumberStepper 1-100, def 20]  │
│ Max Total:       [NumberStepper 1-100, def 25]  │
│                                                 │
│ ── Deduplication ──                             │
│ Semantic Threshold: [Slider 0.5-1.0, def 0.85] │
│ Re-injection Delay: [NumberStepper 1-200, def 50] turns │
│                                                 │
│ ── Ambient Awareness (Advanced) ──              │
│ Pinned Types: [Multi-select checkboxes]         │
│   ☐ Identity  ☐ Goal  ☐ Decision  ☐ Todo       │
│   ☐ Preference  ☐ Fact  ☐ Event  ☐ Observation │
│ Per-type Limit: [NumberStepper 1-20, def 3]     │
│ Sort by: [Select: recent | importance]          │
│                                                 │
│ [Save Changes]                                  │
└─────────────────────────────────────────────────┘
```

The "Ambient Awareness" section is collapsible, labeled "Advanced", with a note: "Enable pinned types to always inject the most recent or important memories of these types, regardless of the current message. Useful for personal assistants. Leave empty for community bots."

### Step 8 — Tracing

**File:** `src/agent/channel.rs`

Add per-memory debug logging:
```rust
tracing::debug!(
    memory_id = %memory.id,
    memory_type = %memory.memory_type,
    source = %source_tag, // "pinned" or "contextual"
    "memory injected"
);
```

Add summary info logging:
```rust
tracing::info!(
    channel_id = %self.id,
    pinned = pinned_count,
    contextual = contextual_count,
    total = total_count,
    deduped = deduped_count,
    elapsed_ms = elapsed.as_millis() as u64,
    "memory injection complete"
);
```

### Step 9 — Tests

**File:** `src/agent/channel.rs` (test module)

Update existing `ChannelInjectionState` tests for the new `semantic_buffer` format `(Vec<f32>, usize)`.

Add new tests:
- `test_budget_enforcement` — pinned + contextual exceeds `max_total`, verify cap
- `test_pinned_types_empty_default` — with `pinned_types = []`, only contextual results
- `test_semantic_buffer_turn_pruning` — entries older than `context_window_depth` are pruned

### Execution order

Steps 1-2 form a group (config + API). Build and test together.
Step 3 is independent (search.rs).
Steps 4-6 form a group (channel pipeline).
Step 7 is independent (frontend).
Steps 8-9 are finalization.

Suggested commit sequence:
1. `refactor(config): replace memory injection V1 settings with V2 schema`
2. `fix(search): wire search_limit to max_results_per_source, configurable graph seed`
3. `refactor(channel): rewrite compute_memory_injection with parallel retrieval + budget`
4. `feat(ui): update memory injection settings for V2 config`
5. `test(channel): add budget enforcement and dedup tests`

## Implementation status (2026-02-24)

### Commit applied (without docs)

- `0ce70f5` — `memory-injection: implement v2 retrieval pipeline and settings`
- `d6177e3` — `memory-injection: add ambient toggle and contextual score threshold`
- `697ec5e` — `feat(memory-injection): add bounded persistence and compactor filtering`
- `a364097` — `feat(agent-config): support per-agent memory injection settings`
- `71c8cc6` — `feat(ui): add per-agent memory injection section`
- `83eca63` — `feat(ui): polish memory injection override UX`
- Files included in commit:
  - `src/config.rs`
  - `src/api/settings.rs`
  - `src/memory/search.rs`
  - `src/agent/channel.rs`
  - `interface/src/api/client.ts`
  - `interface/src/routes/Settings.tsx`
    - `src/agent/compactor.rs`
    - `src/api/config.rs`
    - `src/api/agents.rs`
    - `interface/src/routes/AgentConfig.tsx`

### Additional completed work (post-plan)

#### A) Bounded persistence model implemented (ADR execution)

- Injection blocks are now identified by a shared prefix helper and pruned before insertion.
- Persistence is bounded with `max_injected_blocks_in_history` (`0` = ephemeral mode).
- Compactor transcript rendering ignores injection blocks to avoid summary pollution.
- Default `context_window_depth` changed from `50` to `10`.

#### B) Per-agent memory injection overrides implemented

- `memory_injection` is now supported in `[[agents]]` with fallback to `[defaults].memory_injection`.
- Runtime resolution behavior:
    - no agent override → agent inherits global default
    - agent override present → agent uses override
- Runtime reload now applies each agent’s resolved `memory_injection` config.

#### C) API support for override detection and reset

- Agent config response now includes `memory_injection_overridden`.
- Agent update API supports `reset_memory_injection_override = true` to remove `[[agents]].memory_injection` and return to defaults.

#### D) UI harmonization and override UX

- Global Memory Injection UI clarifies that it is the default config for agents.
- Per-agent Memory Injection UI now mirrors global sections (Contextual Search / Deduplication / Ambient Awareness).
- Per-agent status shows `Using Default` vs `Override`.
- `Revert to Default` button is available when overridden.
- `Experimental` badge is shown once in the header.
- Ambient Awareness control simplified:
    - single `Enabled` toggle
    - advanced controls are shown only when enabled
    - same behavior in global and per-agent screens.

### Step-by-step status vs plan

1. **Config migration (Step 1)** — ✅ **Done**
    - V2 schema implemented (`search_limit`, `contextual_min_score`, `pinned_types`, `ambient_enabled`, `pinned_limit`, `pinned_sort`, `max_total`)
    - V1 fields removed from config struct and TOML mapping
    - ArcSwap cloning fixed after removing `Copy`

2. **API settings backend (Step 2)** — ✅ **Done**
    - API response/update structs migrated to V2 fields
    - TOML read/write migrated to V2 keys
    - V1 keys removed from API settings handling
    - Added explicit API support for `contextual_min_score` and `ambient_enabled`

3. **Hybrid search wiring (Step 3)** — ✅ **Done**
    - `max_results_per_source` now set from injection `search_limit`
    - `SearchConfig` extended with `graph_seed_threshold` and `graph_seed_limit`
    - `hybrid_search` uses config-based graph seed parameters (no hardcoded 0.8/20)

4. **Channel pre-hook refactor (Step 4)** — ✅ **Done**
    - 4-source sequential V1 flow replaced by V2 flow:
      - optional pinned retrieval (ambient context)
      - contextual hybrid retrieval
    - contextual min-score filtering (`contextual_min_score`)
      - dedup
      - budget enforcement (`max_total`)
      - structured output sections
    - ambient master toggle (`ambient_enabled`) respected before pinned retrieval

5. **Dedup improvements (Step 5)** — ✅ **Done**
    - Semantic buffer now stores `(embedding, turn)`
    - Added `prune_semantic_buffer(current_turn, context_window_depth)`
    - Semantic duplicate check ignores stale entries beyond context window

6. **Structured output format (Step 6)** — ✅ **Done**
    - Output now uses sectioned format:
      - `[Pinned context]`
      - `[Relevant to this message]`

7. **Frontend migration (Step 7)** — ✅ **Done**
    - TypeScript API types migrated to V2
    - Settings UI migrated to V2 controls
    - Advanced ambient-awareness section added (`ambient_enabled`, `pinned_types`, `pinned_limit`, `pinned_sort`)
    - Added soft guidance for recommended pinned types (`todo`, `goal`, optional `decision`)

8. **Tracing (Step 8)** — ✅ **Done**
    - Per-memory debug logs include source tag (`pinned`/`contextual`)
    - Summary info log includes counts + latency + dedup count
    - Added info-level skip logs for no-injection paths (disabled / no candidates after dedup / empty after budget)

9. **Tests (Step 9)** — 🟡 **Partially done**
    - Done:
      - existing injection-state tests updated to new semantic buffer shape
      - new `semantic_buffer_turn_pruning` test added
      - targeted tests pass:
         - `cargo test -q --lib channel_injection_state_updates`
         - `cargo test -q --lib semantic_buffer_turn_pruning`
    - Remaining from plan:
      - `test_budget_enforcement`
      - `test_pinned_types_empty_default`

10. **Persistence model rollout (ADR)** — ✅ **Done**
    - `INJECTION_BLOCK_PREFIX`, `is_injection_block`, and block pruning wired in channel turn path
    - `render_messages_as_transcript` now skips injection blocks
    - Config + API + UI support for `max_injected_blocks_in_history`

11. **Per-agent overrides + UX polish** — ✅ **Done**
    - Per-agent `memory_injection` config resolution with fallback to defaults
    - Agent Config API and UI support for editing overrides
    - Explicit `Using Default` / `Override` status and `Revert to Default` action
    - Ambient advanced section behavior unified between global/per-agent

### Remaining follow-ups

- Add the 2 remaining unit tests listed above.
- ~~Resolve and document the persistence model explicitly~~ → **DECIDED + IMPLEMENTED.** See `memory-injection-persistence-model.md`.
- Optional hardening: emit a warning for each dropped invalid `pinned_type` during config resolution (currently invalid entries are filtered out silently).
- Full workspace compile (`cargo check`) currently blocked by external dependency errors in `geoarrow-array` (`wkb` crate resolution), not by memory-injection changes.

## Persistence Model Implementation Plan

**Decision:** Bounded persistence + compactor-aware filtering (Option D).
**ADR:** `memory-injection-persistence-model.md`

### Summary

Injected `[Context from memory]` blocks persist in the channel's RAM history with a hard cap. Before each new injection, the oldest blocks beyond the cap are pruned. The compactor skips injection blocks when building transcripts, preventing summarization pollution and reinforcement loops.

### Step P1 — Shared injection block utilities

**File:** `src/agent/channel.rs`

Create a shared constant and predicate at module level (not inside an impl block):

```rust
/// Stable prefix for injected memory context blocks.
pub(crate) const INJECTION_BLOCK_PREFIX: &str = "[Context from memory]";

/// Check if a Rig message is an injected memory context block.
pub(crate) fn is_injection_block(message: &Message) -> bool {
    match message {
        Message::User { content } => content.iter().any(|item| {
            matches!(item, UserContent::Text(t) if t.text.starts_with(INJECTION_BLOCK_PREFIX))
        }),
        _ => false,
    }
}
```

Update the existing injection code in `run_agent_turn` to use `INJECTION_BLOCK_PREFIX` instead of the inline string `"[Context from memory]:\n"`.

**Commit scope:** constant + predicate + existing usage migrated. No behavior change.

### Step P2 — Purge logic

**File:** `src/agent/channel.rs`

Add `prune_old_injection_blocks` as a free function:

```rust
/// Remove oldest injection blocks so that at most `max_keep` remain.
/// If `max_keep == 0`, all injection blocks are removed (ephemeral mode).
/// Called before each new injection to make room for the new block.
fn prune_old_injection_blocks(history: &mut Vec<Message>, max_keep: usize) {
    if max_keep == 0 {
        history.retain(|m| !is_injection_block(m));
        return;
    }

    let injection_indices: Vec<usize> = history.iter()
        .enumerate()
        .filter(|(_, m)| is_injection_block(m))
        .map(|(i, _)| i)
        .collect();

    if injection_indices.len() >= max_keep {
        let to_remove = injection_indices.len() - max_keep + 1; // +1 room for incoming block
        for &idx in injection_indices[..to_remove].iter().rev() {
            history.remove(idx);
        }
    }
}
```

Call site: in `run_agent_turn`, after cloning history and before injecting the new block:

```rust
let config = self.deps.runtime_config.memory_injection.load();
prune_old_injection_blocks(&mut history, config.max_injected_blocks_in_history);
// ... then inject new block ...
```

**Commit scope:** purge function + call site. Testable in isolation.

### Step P3 — Compactor filter

**File:** `src/agent/compactor.rs`

Import `is_injection_block` from channel module. Add a skip at the top of the message loop in `render_messages_as_transcript`:

```rust
use crate::agent::channel::is_injection_block;

fn render_messages_as_transcript(messages: &[Message]) -> String {
    let mut output = String::new();
    for message in messages {
        if is_injection_block(message) { continue; }
        // ... existing rendering logic ...
    }
    output
}
```

No changes needed to `estimate_history_tokens` (injection blocks counted → safe direction, triggers compaction slightly earlier) or `emergency_truncate` (drops oldest messages regardless; next turn re-injects).

**Commit scope:** single `continue` guard in compactor.

### Step P4 — Config: new field + default change

**File:** `src/config.rs`

Add to `MemoryInjectionConfig`:
```rust
/// Maximum number of injected memory blocks kept in conversation history.
/// 0 = ephemeral (blocks stripped after each turn).
pub max_injected_blocks_in_history: usize,
```

Default: `3`.

Change `context_window_depth` default from `50` to `10`. With persistent blocks providing continuity, the re-injection delay can be shorter.

Update `TomlMemoryInjectionConfig` (add `max_injected_blocks_in_history: Option<usize>`).
Update `resolve` and `Default` impls accordingly.

**File:** `src/api/settings.rs`

Add `max_injected_blocks_in_history` to `MemoryInjectionResponse` and `MemoryInjectionUpdate`. Wire read/write in `get_global_settings` / `update_global_settings`.

**File:** `interface/src/api/client.ts`

Add `max_injected_blocks_in_history: number` to `MemoryInjectionConfig` and `Partial<>` update type.

**File:** `interface/src/routes/Settings.tsx`

Add a `NumberStepper` control in the "Contextual Search" group:
- Label: "History Block Limit"
- Range: 0-10
- Default: 3
- Help text: "Maximum injection blocks kept in history. 0 = ephemeral (no persistence)."

**Commit scope:** config + API + frontend in one commit. All plumbing for the new field.

### Step P5 — Tests

**File:** `src/agent/channel.rs` (test module)

```rust
#[test]
fn test_prune_old_injection_blocks_cap() {
    // Build history with 4 injection blocks + interleaved messages
    // Call prune_old_injection_blocks(history, 3)
    // Assert: oldest injection block removed, 3 remain
    // Assert: non-injection messages untouched
}

#[test]
fn test_prune_old_injection_blocks_ephemeral() {
    // Build history with 3 injection blocks
    // Call prune_old_injection_blocks(history, 0)
    // Assert: all injection blocks removed
    // Assert: non-injection messages untouched
}

#[test]
fn test_prune_old_injection_blocks_under_cap() {
    // Build history with 2 injection blocks
    // Call prune_old_injection_blocks(history, 3)
    // Assert: no blocks removed (2 < 3)
}

#[test]
fn test_is_injection_block() {
    // Positive: User message starting with INJECTION_BLOCK_PREFIX → true
    // Negative: User message not starting with prefix → false
    // Negative: Assistant message → false
    // Negative: Tool message → false
}
```

**File:** `src/agent/compactor.rs` (test module)

```rust
#[test]
fn test_render_transcript_skips_injection_blocks() {
    // Build message slice with injection blocks and regular messages
    // Call render_messages_as_transcript
    // Assert: output contains regular messages
    // Assert: output does NOT contain INJECTION_BLOCK_PREFIX
}
```

**Commit scope:** all tests in one commit.

### Execution order

| Step | Depends on | Commit message |
|------|-----------|----------------|
| P1 | — | `refactor(channel): extract injection block constant and predicate` |
| P2 | P1 | `feat(channel): prune old injection blocks before each new injection` |
| P3 | P1 | `fix(compactor): skip injection blocks in compaction transcript` |
| P4 | P2 | `feat(config): add max_injected_blocks_in_history setting` |
| P5 | P1-P4 | `test: add injection block persistence and compactor filter tests` |

P2 and P3 can be done in parallel (both depend on P1 only). P4 depends on P2 (uses the purge function with the new config field). P5 is last.

### Validation

After all 5 steps, verify against the checklist in the ADR (`memory-injection-persistence-model.md`, section 6):

- [ ] Cap enforced: never more than N injection blocks in history
- [ ] `max_injected_blocks_in_history = 0` gives ephemeral behavior
- [ ] Compaction summaries contain zero `[Context from memory]` content
- [ ] Multi-turn follow-ups on same topic retain context
- [ ] Topic change naturally evicts old injection blocks
- [ ] `context_window_depth = 10` prevents excessive re-injection
- [ ] Memory persistence branches don't re-save at excessive rates
- [ ] Branches forked during injection see the injected context
- [ ] Emergency truncation doesn't crash on injection blocks

## Reference

### Design & planning archives
- `../ETUDE INITIALE/` — original design phase (MemOS study, architecture, implementation plan, tests strategy, perfectionist review, LLM onboarding guide)
- `../IMPLEMENTATION 1/memory-system-overview.md` — comprehensive analysis of V1 memory system and V2 proposal (data model, 8 types, write/read paths, MemOS comparison)
- `../IMPLEMENTATION 1/memory-v2-implementation-plan.md` — detailed implementation plan for V1 injection (phases 1-5, code snippets, `ChannelInjectionState`, `get_recent_since`)
- `../IMPLEMENTATION 1/memory-v2-status.md` — V1 implementation status and post-implementation review (completed tasks, open questions about type/source settings coherence)
- `memory-injection-persistence-model.md` — ADR for persistence model decision
### Code
- `src/agent/channel.rs` — `ChannelInjectionState`, `compute_memory_injection()`, injection in `run_agent_turn()`
- `src/memory/search.rs` — `MemorySearch`, `hybrid_search()`, `SearchConfig`, RRF
- `src/memory/store.rs` — `get_by_type()`, `get_high_importance()`, `get_recent_since()`
- `src/memory/embedding.rs` — `cosine_similarity()`, `is_semantically_duplicate()`
- `src/config.rs` — `MemoryInjectionConfig`
- `src/api/settings.rs` — REST API for memory injection settings
- `interface/src/routes/Settings.tsx` — UI for memory injection settings

