# SurrealDB memory backend — follow-ups & known issues

Tracking doc for the `feat/surrealdb-memory` work. Companion to
`design.md`. Captures self-review findings on the
**implementation** (not the design), their severity, and status.

## Process note (why these surfaced late)

The two adversarial Opus reviews ran against the **design doc only** — they
happened before any implementation existed. The in-crate modules were written
afterward in one autonomous session and initially never code-reviewed; this list
came from a cold self-review.

**UPDATE 2026-06-22:** that review debt is now paid. Plan A + Plan B brought the
code through per-task reviews (Opus→Sonnet) AND two final whole-branch Opus
reviews (verdicts: Plan A *READY WITH MINOR FIXES* → fixed; Plan B *READY TO
MERGE*). The plans themselves were Opus-reviewed before execution (8 plan defects
caught for A, 6 for B). The cutover is wired and the Surreal store is
runtime-validated (8 feature-on tests green vs real SurrealKV). Statuses below
reflect this.

## Verified NON-issues (suspected, then empirically checked — fine)

- **delete() leaving orphan edges** — SurrealKv 3.1.5 cascade-deletes incident
  `relates` edges when a `memory` record is deleted (probed: 0 edges, 0 dangling
  neighbour associations remain). No bug.
- **Filtered HNSW returning < K at high forgotten ratio** — probed at 95–98 %
  forgotten: returns exactly `min(K, live)`. The `forgotten` filter is applied
  correctly. (Caveat: validated at dim-4 / ≤200 rows where HNSW ≈ brute force;
  recall at 384-dim/large corpora is still R-scale, item 7.)

## Open items

| # | Sev | Item | Status |
| --- | --- | --- | --- |
| 1 | 🔴 | **"Atomic merge" was false** — `merge` was a multi-query sequence. **Fixed**: `merge` now runs in a single `BEGIN … COMMIT` transaction (survivor update + edge rewires with conflict pre-delete + loser-edge drop + `updates` edge + soft-delete), which SurrealDB rolls back on any failure. Probed: edge endpoints are immutable (rewire via RELATE+DELETE), duplicate RELATE violates the UNIQUE index (hence the pre-delete), and BEGIN/COMMIT rolls back on failure. Reference test green. | **fixed** |
| 2 | 🔴 | **`prune`/`decay` scanned rows into Rust** and filtered client-side. **Fixed**: `prune` is now a single server-side `DELETE memory WHERE … RETURN BEFORE` (no cap; edges cascade) — `store.prune_below`, reference-tested. `decay` keeps the exact Rust formula (pushing it server-side would reset `updated_at` and break age accumulation) but now iterates **per non-identity type** (1000/type, matching the original) instead of one global 2000-row scan, removing the silent-skip. | **fixed** |
| 12 | 🔵 | **`migrate` is not transactional** (per-memory save + per-edge RELATE). Acceptable because it is idempotent (re-runnable), but each memory+edges could be wrapped in a txn for cleaner partial-failure recovery. | open |
| 3 | 🟠 | **No `MemoryBackend` abstraction → duplicated logic.** | **RESOLVED (Plan A+B)** — `trait MemoryBackend` introduced; `surreal_search.rs`/`surreal_maintenance.rs` **deleted**; the single generic `search.rs`/`maintenance.rs` now drive both stacks. No drift possible. |
| 4 | 🟡 | In-crate code **compile-checked only, never executed**; serde / `Datetime`↔chrono round-trip on real `Memory` unproven. | **RESOLVED** — 8 feature-on tests run green against real embedded SurrealKV with real ort (save/load round-trip, merge, prune, associations, vector/FTS/find_similar, `get_associations_between`). Real-`Memory` serde round-trip exercised. |
| 5 | 🟡 | **FTS differs from Tantivy** (analyzer lacks stemming). | **RESOLVED (Plan C)** — added `snowball(english)` to `memory_an`; 2 gated stemming tests (query "run" matches "running", "database" matches "databases") that fail against the old analyzer. Caveat documented: `IF NOT EXISTS` means a live on-disk store needs `OVERWRITE` + FTS re-index to pick it up. |
| 6 | 🟡 | **KNN `EF = (limit*4).max(40)` arbitrary.** | **RESOLVED (Plan C)** — benchmarked (spike `bench_hnsw`). recall@10 = 1.0 at all EF on planted 384-dim clusters; EF=40 showed a severe **tail-latency pathology** (p99 ≈ 15s on a pathological query). Floor raised `(limit*4).max(40)` → `.max(80)` in `vector_search`+`find_similar` (eliminates the tail, ~+1ms median). |
| 7 | 🟡 | **Scale/recall unproven.** | **RESOLVED (Plan C)** — recall@10 measured at **384-dim** on planted near-dup clusters (10–20k-vector on-disk `kv-surrealkv` index, `TYPE F32`) = 1.0 across EF∈{40..640} vs exact-cosine ground truth; p50/p95 latency captured. (Bulk-random recall is meaningless in 384-dim — planted clusters are the signal.) |
| 8 | 🔵 | **CI can't build the feature** without onnxruntime/bypass. | open — note: real ort IS available in this env (tests run); CI just needs onnxruntime installed or the bypass for compile-coverage. |
| 9 | 🔵 | **Dependency weight** — `surrealdb` pulls a large tree; Lance not removed, so feature-on = both backends compiled. | open — Lance removal only possible if/when SurrealDB becomes the sole backend (the config selector keeps both available by design). Build-size still unmeasured. |
| 10 | 🔵 | **`Association.id` synthesized** (`src:rt:tgt`) — edges lose the original UUID. | open (no impact on search/maintenance). |
| 11 | 🔵 | **Schema re-applied on every `open`** — idempotent (`IF NOT EXISTS`), wasteful. | open (low — `IF NOT EXISTS` no-ops cheaply; non-issue per-agent). |
| 13 | 🟠 | **`build_channel_context(&MemoryStore)` landmine** (`src/conversation/context.rs`) — dead code that would bypass the backend abstraction. | **RESOLVED (Plan C)** — dead fn deleted (0 callers; tombstone comment left noting the correct `&Arc<dyn MemoryBackend>` approach). |
| 14 | 🔵 | **SQLite construction branch duplicated 4×** across the cfg-split in `main.rs`/`agents.rs`. | open — extract a helper (cosmetic). |
| 16 | 🟠 | **`MemorySearch::traverse_graph` is still N+1** — the hybrid-search seed traversal (`search.rs`) calls `get_associations`+`load` per node (generic, with relation-type scoring + selective re-expansion). Plan C's native recursion only replaced `get_neighbors` (the API graph-view path). | open → follow-on; needs a trait method (e.g. `get_neighbors`-like with scoring hooks) or a backend-specific fast path so the Surreal backend avoids the round-trips here too. |
| 17 | 🔵 | Minor test gaps (final-review triage): C2 `get_neighbors_excludes_specified_nodes` discards edge assertions; C5 p99 from 80 samples / implicit (not explicit brute-force) recall ground truth (stronger for ε=0.04, but weakens if reused). | open (cosmetic). |
| 15 | 🟠 | **`surreal_migrate` not wired into runtime** (no callers). | open — needs a CLI/tool entry to actually migrate existing SQLite+Lance data into SurrealDB; without it, switching an existing agent to `surreal` starts from an empty store. |

## Notes on fixes

- **#1**: read the loser's edges first, then run a single `BEGIN … COMMIT`
  transaction with the survivor update, edge rewires, edge deletes, `updates`
  edge, and loser soft-delete. Same approach for the parts of `migrate` that
  should be atomic per memory. Verify duplicate-edge handling under the UNIQUE
  index inside a transaction.
- **#2**: `prune` → one server-side `DELETE memory WHERE importance < $t AND
  created_at < $c AND memory_type != 'identity'`. `decay` → either a server-side
  `UPDATE … SET importance = …` using SurrealQL time/IF expressions, or keep the
  Rust loop but remove the silent cap (paginate). Mirror the existing formula
  exactly.
</content>
