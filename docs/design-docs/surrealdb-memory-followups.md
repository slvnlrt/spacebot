# SurrealDB memory backend — follow-ups & known issues

Tracking doc for the `feat/surrealdb-memory` work. Companion to
`surrealdb-memory-backend.md` (the design). Captures self-review findings on the
**implementation** (not the design), their severity, and status.

## Process note (why these surfaced late)

The two adversarial Opus reviews ran against the **design doc only** — they
happened before any implementation existed. The in-crate modules
(`surreal_store.rs`, `surreal_search.rs`, `surreal_maintenance.rs`,
`surreal_migrate.rs`, ~700 lines) were written afterward in one autonomous
session and **never went through a code review pass**. The findings below come
from a cold self-review after the fact. Action: run a review agent on the
implementation diff before the live wiring/cutover.

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
| 3 | 🟠 | **No `MemoryBackend` abstraction → duplicated logic.** `surreal_search`/`surreal_maintenance` duplicate `search.rs`/`maintenance.rs`; the two stacks can drift. | **deferred** (dedicated session, per owner) |
| 4 | 🟡 | In-crate code is **compile-checked only, never executed** here (can't link ort). Logic proven by the reference crate; in-crate serde / `Datetime`↔chrono round-trip on real `Memory` unproven. | open |
| 5 | 🟡 | **FTS results will differ from Tantivy** (analyzer: `class`+lowercase/ascii vs Tantivy stemming). Ranking changes — a behaviour change, not a pure port. | open |
| 6 | 🟡 | **KNN `EF = (limit*4).max(40)` is arbitrary** — affects recall; needs tuning/benchmark. | open |
| 7 | 🟡 | **Scale unproven** — all validation at dim-4 / ≤1060 rows (HNSW ≈ brute force). Benchmark recall/latency at 384-dim and realistic corpus. | open |
| 8 | 🔵 | **CI can't build the feature** without onnxruntime or the `ORT_LIB_LOCATION` bypass → gated code can rot without compile coverage. | open |
| 9 | 🔵 | **Dependency weight** — `surrealdb` pulls a large tree (rustls/jwt/tungstenite) even for embedded; Lance not yet removed, so feature-on = both. Build-size unmeasured. | open |
| 10 | 🔵 | **`Association.id` synthesized** (`src:rt:tgt`) — edges lose the original UUID; differs from the current contract (no impact on search/maintenance). | open |
| 11 | 🔵 | **Schema re-applied on every `open`/`from_handle`** — idempotent but wasteful on a shared instance. | open |

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
