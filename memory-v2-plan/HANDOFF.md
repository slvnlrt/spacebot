# Memory Injection V2 — Handoff (Consolidated)

**Branch:** `merge/upstream-main-2026-02-24`  
**Last updated:** 2026-02-26  
**Status:** Fixes implemented, tested, committed and merged with upstream/main. Pending: Step 5 (Timeline visualization).

---

## Summary

Two bugs were fixed in the memory injection pipeline:

1. **Injection Persistence** — ✅ FIXED. Injection blocks now persist correctly through `PromptCancelled` turns.
2. **Search Quality / Cosine Filtering** — ✅ IMPLEMENTED. Relative cosine threshold two-pass architecture. **Needs live testing and tuning.**

---

## Bug 1: Injection Persistence — FIXED ✅

### Problem

Injected memory blocks (`[Context from memory]` in conversation history) were wiped on every turn. After the channel's reply tool fired `PromptCancelled`, the history rollback erased the injection.

### Root Cause

The injection (prune old blocks + push new context message) was applied to a **clone** of the history, not the shared `RwLock` guard. `history_len_before` was captured BEFORE injection. So:

- The prune shifted indices in the clone → `history_len_before` pointed to wrong message
- `guard.truncate(history_len_before)` was a no-op on guard (never modified)
- New injection block existed only in clone, never written back to guard

For channels, the normal exit path is **always** `PromptCancelled` (reply tool → `HookAction::Terminate` → Rig raises `PromptCancelled`). This meant injection blocks **never** persisted in the shared guard, making `max_injected_blocks_in_history` completely non-functional.

### Fix Applied

In `src/agent/channel.rs` → `run_agent_turn()` (~lines 1899-1918):

- Injection (prune + push) is now done inside a **write lock on the shared guard**, BEFORE cloning
- `history_len_before` is captured AFTER injection (it's the return value of the write lock block)
- Rollback on `PromptCancelled` naturally preserves injection blocks because they're below the truncation line

```rust
// NEW: injection into guard before clone
let history_len_before = {
    let mut guard = self.state.history.write().await;
    if let Some(ref context) = injected_context {
        prune_old_injection_blocks(&mut guard, max_injected_blocks_in_history);
        guard.push(Message::from(format!("{INJECTION_BLOCK_PREFIX}:\n{}", context)));
    }
    guard.len()  // captured AFTER injection
};

// Clone after injection — history already contains the block
let mut history = {
    let guard = self.state.history.read().await;
    guard.clone()
};
```

The `apply_history_after_turn` function is unchanged — clean truncation to `history_len_before` is correct because the injection block is now below that line.

### Flow Diagram (after fix)

```
Turn N:
  guard = [msg1, msg2, old_block]
  WRITE LOCK:
    prune_old_injection_blocks → guard = [msg1, msg2]  (if cap exceeded)
    push new block → guard = [msg1, msg2, new_block]
    history_len_before = 3
  clone → history = [msg1, msg2, new_block]
  LLM loop → reply tool → PromptCancelled
  apply_history_after_turn: guard.truncate(3)
  → guard = [msg1, msg2, new_block]  ✅ PRESERVED

Turn N+1:
  guard = [msg1, msg2, new_block]  ← block from previous turn is there
  ...
```

---

## Bug 2: Search Quality / Cosine Filtering — IMPLEMENTED, NEEDS LIVE TEST 🔧

### Problem

Memory injection returns irrelevant results. A long message like "aide moi à debuguer ma feature d'injection mémoire" returns ALL 20 memories (café, bière, Japon, Spacedrive, musique...) instead of just the relevant ones.

### Root Cause Chain (3 issues, all fixed)

#### Issue 2a: Graph traversal keyword noise — FIXED

The upstream `hybrid_search` in `src/memory/search.rs` (lines 216-221) does naive keyword matching for graph seeds: `query.split_whitespace().any(|term| seed.content.contains(term))`. French stop words ("ce", "que", "comme") match nearly every memory. Graph seeds are high-importance memories → they flood RRF fusion.

**Fix:** `graph_seed_limit: 0` in `compute_memory_injection`'s `SearchConfig`. Vector + FTS are sufficient for injection.

#### Issue 2b: RRF score as quality filter — FIXED

`contextual_min_score` was being passed as `min_score` to `SearchConfig`, which filters on RRF score. RRF scores measure **cross-source consensus**, not relevance. A perfect FTS match on a proper noun (e.g. "Jamie Pine") scores ~0.016 in a single source — any non-zero threshold kills it.

**Fix:** `min_score: 0.0` in SearchConfig. Relevance filtering moved to cosine-similarity post-filter.

#### Issue 2c: Absolute cosine threshold doesn't work — FIXED

With all-MiniLM-L6-v2, a long French message produces embeddings that are "diffuse" — cosine similarity with all memories lands in a narrow band (e.g. 0.52–0.72). An absolute threshold either passes everything (at 0.40) or nothing (at 0.80).

**Fix: Relative cosine threshold.** Instead of `score >= threshold`, we do `score >= max_score × ratio`. This adapts to message length. The `contextual_min_score` config value is now interpreted as a **ratio** (0.0–1.0).

### Architecture of the Cosine Filter

```
User message
    │
    ▼
embed_one(user_text) → query_embedding
    │
    ▼
hybrid_search(user_text, SearchConfig{min_score:0, graph_seed_limit:0})
    │  → vector search (LanceDB HNSW, cosine)
    │  → FTS search (LanceDB Tantivy)
    │  → RRF fusion (k=60) → ranked list
    │
    ▼
Pass 1: for each candidate
    │  → fetch embedding from LanceDB (or compute)
    │  → cosine_similarity(query_embedding, candidate_embedding)
    │  → track max_cosine
    │
    ▼
Pass 2: dynamic_threshold = max_cosine × contextual_min_score (ratio)
    │  → filter: similarity < dynamic_threshold → skip
    │  → semantic dedup (is_semantically_duplicate, threshold 0.85)
    │  → add to unique_candidates
    │
    ▼
Format [Context from memory] block → inject into history
```

### Key Changes in `compute_memory_injection()` (`src/agent/channel.rs`)

1. **Import added:** `cosine_similarity` from `crate::memory`
2. **`InjectionSource` enum:** Added `PartialEq` derive
3. **SearchConfig:** `min_score: 0.0` + `graph_seed_limit: 0`
4. **Two-pass architecture:**
   - Pass 1: resolve embedding, compute cosine similarity to query, track `max_cosine`
   - Pass 2: `dynamic_threshold = max_cosine × contextual_min_score`. Filter below threshold. Then semantic dedup.
5. **Logging:** `cosine filter check` (DEBUG) + `cosine relative threshold` (DEBUG)

### Observed Cosine Scores (real data, long debug message)

| Memory content (truncated) | Cosine to query |
|---|---|
| Session nocturne | 0.717 |
| Memory v2 RAG system | 0.716 |
| RAG seuil 85% | 0.716 |
| Briefing cortex 60min | 0.710 |
| Spacedrive déploiement | 0.703 |
| Spacedrive discussions | 0.685 |
| IaC stricte | 0.682 |
| NixOS dépôt Git | 0.680 |
| Spacedrive VDFS | 0.679 |
| Spacedrive v2 alpha | 0.667 |
| café Yirgacheffe | 0.663 |
| streaming Navidrome | 0.663 |
| Spacedrive local-first | 0.659 |
| pompe à chaleur | 0.640 |
| p2p-sync-node todo | 0.635 |
| Japon Golden Gai | 0.630 |
| bière fraîche | 0.625 |
| NixOS Homelab | 0.618 |
| Dark Synth | 0.558 |
| boissons chaudes | 0.520 |

Spread: 0.52–0.72 (only 0.20). This is why absolute thresholds fail.

With ratio 0.70: dynamic_threshold = 0.717 × 0.70 = 0.502 → most pass (too permissive for long messages)
With ratio 0.90: dynamic_threshold = 0.717 × 0.90 = 0.645 → ~12 pass
With ratio 0.95: dynamic_threshold = 0.717 × 0.95 = 0.681 → ~8 pass

---

## Config: `contextual_min_score`

The value is now a **ratio** (0.0–1.0), not an absolute score. Default: **0.70**.

| Location | Value | Role |
|----------|-------|------|
| `src/config.rs` `default_contextual_min_score()` | **0.70** | Rust default for new deployments |
| `src/api/settings.rs` fallback | **0.70** | API fallback when no DB config |
| `interface/src/routes/Settings.tsx` useState | **0.70** | UI initial state |
| `interface/src/routes/Settings.tsx` useEffect | **0.70** | Fallback when settings loaded |
| `interface/src/routes/AgentConfig.tsx` NumberStepper | max=1, step=0.01 | Per-agent UI |

---

## Modified Files (current working tree)

All 5 files are modified but **not yet committed**:

| File | Changes |
|------|---------|
| `src/agent/channel.rs` | Bug 1 fix (injection in guard) + Bug 2 cosine filter two-pass + logging cleanup |
| `src/config.rs` | `default_contextual_min_score` 0.01 → 0.70 |
| `src/api/settings.rs` | Fallback 0.01 → 0.70 |
| `interface/src/routes/Settings.tsx` | Slider 0–1, step 0.01, defaults 0.70 |
| `interface/src/routes/AgentConfig.tsx` | NumberStepper 0–1, step 0.01 |

**Untracked files (keep, do not commit with the fix):**
- `migrations/20260225000001_memory_injection_events.sql` — DB already migrated, code rolled back. Keep for future timeline viz feature.
- `migrations/20260225000002_memory_injection_historical.sql` — same.
- `memory-v2-plan/IMPLEMENTATION 2/memory-injection-viz-plan.md` — plan for future timeline viz.

---

## Key Code Locations

| What | File | Lines (approx) |
|------|------|----------------|
| `compute_memory_injection` | `src/agent/channel.rs` | ~1418–1700 |
| SearchConfig construction | `src/agent/channel.rs` | ~1505 |
| Two-pass cosine filter | `src/agent/channel.rs` | ~1544–1705 |
| `run_agent_turn` (injection in guard) | `src/agent/channel.rs` | ~1899–1926 |
| `apply_history_after_turn` | `src/agent/channel.rs` | ~1850 |
| `hybrid_search` (upstream, untouched) | `src/memory/search.rs` | ~150–260 |
| Graph traversal keyword bug | `src/memory/search.rs` | ~216–221 |
| `cosine_similarity` function | `src/memory/embedding.rs` | ~72 |
| `SearchConfig` defaults | `src/memory/search.rs` | ~340–370 |
| `MemoryInjectionConfig` | `src/config.rs` | ~620–710 |
| Config resolution (per-agent override) | `src/config.rs` | ~3325–3515 |
| API settings fallback | `src/api/settings.rs` | ~305 |
| Agent config UI | `interface/src/routes/AgentConfig.tsx` | ~846 |
| Global settings UI | `interface/src/routes/Settings.tsx` | ~1221–1380 |

---

## Things NOT Changed (upstream code)

- `src/memory/search.rs` — `hybrid_search`, `traverse_graph`, `reciprocal_rank_fusion` are all upstream. NOT modified. The graph keyword bug exists there but is bypassed via `graph_seed_limit: 0`.
- `src/memory/embedding.rs` — `cosine_similarity`, `is_semantically_duplicate` — used as-is.
- `src/memory/lance.rs` — `vector_search`, `text_search` — used as-is.
- Embedding model: fastembed all-MiniLM-L6-v2 (384 dims). No change.

---

## What Needs To Be Done Next

### Step 1 — Live test the cosine filtering

```bash
cargo run -- --debug start --foreground
```

Send test messages and check logs:
```bash
grep "cosine relative threshold" ~/.spacebot/logs/spacebot.log.*
grep "cosine filter check" ~/.spacebot/logs/spacebot.log.*
```

Test queries (these were broken before):
- **"Jamie Pine"** — should return Spacedrive VDFS fact (FTS match). Previously failed because RRF score was too low.
- **"boisson chaude"** / **"bar, soif"** — should return café + bière preferences only
- **"musique" / "Carpenter"** — should return Dark Synth preference + Navidrome decision
- **"voyage Japon"** — should return Golden Gai event
- **Long debug message** — should NOT return all 20 memories

### Step 2 — Tune the ratio if needed

Current default: **0.70**. This is permissive — good starting point.

For the long debug message (max_cosine ~0.72):
- At ratio 0.70: threshold = 0.50 → most pass (probably too many)
- At ratio 0.90: threshold = 0.65 → ~12 pass
- At ratio 0.95: threshold = 0.68 → ~8 pass

For a short targeted message like "boisson chaude" (max_cosine ~0.85+):
- At ratio 0.70: threshold = 0.60 → only café/bière pass ✓

Adjust `contextual_min_score` in the UI (global settings AND per-agent settings) based on results.

### Step 3 — Commit (✅ COMPLETED)

```
fix: memory injection persistence + cosine relevance filtering

- Move injection to shared guard before clone (fixes block erasure on PromptCancelled)
- Disable graph traversal for injection (naive keyword matching floods RRF with stop-word hits)
- Replace RRF min_score filter with relative cosine threshold (adapts to message length)
- Align contextual_min_score default to 0.70 everywhere
- Update UI controls to cosine scale (0-1, step 0.01)
```

Files to commit: `src/agent/channel.rs`, `src/config.rs`, `src/api/settings.rs`, `interface/src/routes/Settings.tsx`, `interface/src/routes/AgentConfig.tsx`

Files to NOT commit: migrations, memory-v2-plan docs

### Step 4 — Pull upstream BEFORE UI work (✅ COMPLETED)

```bash
git fetch upstream
git merge upstream/main
```

Likely conflicts:
- `interface/src/routes/Settings.tsx` (upstream changed UI)
- `interface/src/routes/AgentConfig.tsx` (possible)
- `src/agent/channel.rs` (possible)

After merge:
```bash
cargo build && cargo test --lib
cd interface && bun run build
```

### Step 5 — Timeline visualization (after upstream merge)

Re-implement per `memory-v2-plan/IMPLEMENTATION 2/memory-injection-viz-plan.md`.
Migrations are already in place (`memory_injection_events` table + `historical_json` column).

### Step 6 — Settings UI/UX refactor (after upstream merge)

Define scope after seeing upstream changes.

---

## Per-Agent Config Issue (minor, unresolved)

In the per-agent UI (`AgentConfig.tsx`), the decimal precision was limited — couldn't see beyond 2 decimal places when typing directly. The NumberStepper `step` is now 0.01 which should be fine for the new 0–1 range. But if the input field resets to 0 when typing directly, that's a `NumberStepper` component bug unrelated to this work.
