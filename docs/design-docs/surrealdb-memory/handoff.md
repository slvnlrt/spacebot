# Handoff — `feat/surrealdb-memory`

Where the branch stands and what to do next. Read [`README.md`](./README.md) for
the objective and [`gotchas.md`](./gotchas.md) before editing code.

> Updated 2026-06-22 after Plan A + Plan B landed. Earlier revisions of this doc
> described a pre-cutover state — superseded below.

## What's done (Plan A + Plan B)

A **runtime-validated, pluggable** memory backend. The SQLite+Lance stack and
SurrealDB now both sit behind one `trait MemoryBackend`; the backend is chosen
per agent by config; the duplicated Surreal search/maintenance code is gone.

| Area | Where | State |
| --- | --- | --- |
| `trait MemoryBackend` + `SqliteBackend` (SQLite+Lance) | `src/memory/backend.rs` | done, reviewed, default-build tested (`test --lib` 881/0) |
| Generic hybrid search over `dyn MemoryBackend` | `src/memory/search.rs` | done (replaces the deleted `surreal_search`) |
| Generic decay/prune/merge over `dyn MemoryBackend` | `src/memory/maintenance.rs` | done (replaces the deleted `surreal_maintenance`) |
| `impl MemoryBackend for SurrealMemoryStore` + `get_associations_between` | `src/memory/surreal_store.rs` | done; **8 feature-on tests run green vs real SurrealKV (real ort)** |
| `memory_backend` config selector (defaults + per-agent override) | `src/config/types.rs`, `toml_schema.rs`, `load.rs` | done; config tests 110/0 |
| Backend selection at construction | `src/main.rs`, `src/api/agents.rs` | done; feature-off + feature-on both compile clean |
| Migration (SQLite+Lance → SurrealDB) | `src/memory/surreal_migrate.rs` | exists; **NOT wired into runtime** (no callers — manual/tool path) |
| Standalone probe + reference port | `spikes/surreal-memory/` | reference (12 green) |

Gates: feature-off `just gate-pr` ALL GREEN; feature-on `clippy --all-targets -Dwarnings` clean.

Design invariants held: SurrealDB is **memory-scoped** and coexists with SQLite;
embeddings stay external (fastembed); ids stay opaque UUID strings;
`Memory`/`Association` unchanged, bridged via internal `*Row` structs.

## How to build / test

```bash
# Default (feature off) — the production default, full gate:
just gate-pr               # or: systemd-run --scope -p MemoryMax=40G ./scripts/gate-pr.sh

# Feature-on compile-check (clippy doesn't link the final binary):
mkdir -p /tmp/ortlib
ORT_LIB_LOCATION=/tmp/ortlib ORT_PREFER_DYNAMIC_LINK=1 \
  cargo clippy --all-targets --features surreal-memory -- -Dwarnings

# Run the feature-on Surreal tests FOR REAL (real ort links; kv-mem, no network):
cargo test --features surreal-memory --test surreal_memory   # 7 store-primitive tests
cargo test --features surreal-memory --lib memory::surreal_store  # get_associations_between

# Cut an existing agent's memory over to SurrealDB (daemon must be STOPPED):
#   spacebot migrate-memory [--agent <id>]   (feature-gated; refuses if the daemon runs)

# RAM/disk safety: wrap heavy cargo in `systemd-run --scope -p MemoryMax=40G -p MemorySwapMax=0 …`;
# CARGO_BUILD_JOBS is capped (8) globally. Do NOT use the ORT_LIB_LOCATION bypass when you
# want to RUN tests — it only satisfies compile-check; real ort is available here.
```

## Plan C — DONE (2026-06-22)

`docs/superpowers/plans/2026-06-22-surreal-plan-c-native-recursion.md`. Tasks C1–C6:
- **Native graph recursion** — `SurrealMemoryStore::get_neighbors` now uses
  `{..N+collect}` (forward+backward union) + hydrate + incident-edge query (3–4
  fixed queries vs N+1 BFS; ~1.8× faster in-mem, more on disk). De-risked in the
  spike first, ported with parity tests (the EXPANDED≠COLLECTED edge-set trap +
  depth==0 guard handled; forgotten-traversal = documented benign superset).
- **FTS stemming** (#5) — `snowball(english)` on `memory_an`.
- **EF tuning + recall** (#6, #7) — benchmarked at 384-dim (recall@10=1.0; EF floor
  40→80 to kill a tail-latency pathology).
- **Landmine** (#13) — dead `build_channel_context` removed.

## Plan D — DONE (2026-06-22)

`docs/superpowers/plans/2026-06-22-plan-d-batched-traversal.md`. Closed the LAST
N+1 BFS: added batched `get_associations_for` + `load_many` to `MemoryBackend`
and rewrote `MemorySearch::traverse_graph` (the hybrid-search seed traversal)
level-by-level — O(depth)×2 queries instead of O(nodes). Behaviour-preserving
(golden characterization test; the generic scoring/selective-expansion logic
stays in `search.rs`, no per-backend duplication).

## Plan E — DONE (2026-06-22)

`docs/superpowers/plans/2026-06-22-plan-e-migration-cli-dedup.md`.
- **Migration CLI (#15)** — `spacebot migrate-memory [--agent <id>]` (feature-gated):
  refuses if the daemon is running, iterates `resolve_agents()`, builds the SQLite
  source + SurrealKV target per agent, runs `migrate_from_sqlite`. Run it stopped to
  cut an existing agent over to SurrealDB.
- **Construction dedup (#14)** — `crate::memory::sqlite_backend_arc` helper.

## What's still NOT done

1. **Dependency-weight / slim build (#9)** — DEFERRED per owner. Measured: +50 MiB
   (+18%) for embedding both backends; the slim path is dropping Lance (memory-only),
   not SQLite (the app DB). Needs compile-time-exclusive backend selection.
2. Accepted / won't-fix: `Association.id` synthesized (#10), schema re-applied per
   open (#11), `migrate` not transactional (#12, idempotent), C5 bench p99 sampling
   (#17). See `followups.md` for rationale.

**Everything else is DONE.** The branch is feature-complete and merge-ready (default
build unaffected; both configs gate-green; SurrealDB runtime-validated; backup story
verified; CI covers the feature; migration cutover available).

## Decisions

- **(A) Per-agent instance — SETTLED.** One embedded SurrealKv per agent at
  `agent_config.data_dir/surreal` (mirrors the per-agent data dir; strong isolation).
  `SurrealMemoryStore::open` is the path used at the construction sites.
- **(B) Cross-store atomicity — DOCUMENTED / accepted best-effort.** Working memory
  stays on SQLite (`WorkingMemoryStore`) while memory goes to the selected backend;
  a `MemorySaved` that touches both spans two engines with no shared transaction.
  Verified: no code path assumes a single txn across them. Accepted for now.
- **(C) SurrealKV backup/restore — RESOLVED (ops story, verified).** There is no
  in-app backup for ANY backend (SQLite/Lance included); backup = copy the agent
  `data_dir`, which already contains `data_dir/surreal`. A **cold filesystem copy**
  of a quiesced SurrealKV directory restores cleanly with data intact — verified by
  `spikes/surreal-memory` test `surrealkv_cold_copy_backup_restores`. For a LIVE
  copy, quiesce the agent first (stop it) — the **same constraint as SQLite WAL /
  LanceDB** (a live copy without a checkpoint can be inconsistent). So SurrealKV
  needs no special backup machinery: the existing "back up the data dir" story
  covers it; document "stop the agent for a consistent snapshot" for operators.

## Known landmines / debt

- `src/conversation/context.rs::build_channel_context(&MemoryStore, …)` takes the
  concrete SQLite store — **dead code (0 callers)**, but would bypass the backend
  abstraction (read an empty SQLite store under `memory_backend=surreal`) if wired
  in. Convert to `&Arc<dyn MemoryBackend>` or delete.
- The SQLite construction branch is duplicated 4× across the cfg-split in
  `main.rs`/`agents.rs` — correct but could be a shared helper.
- Full debt list with severities: [`followups.md`](./followups.md).

## Branch

All work on `feat/surrealdb-memory` (off `main` @ v0.5.0). No PR opened. Default
build unaffected (feature off). Plan B's final whole-branch review verdict:
**READY TO MERGE** — but the branch is held as a checkpoint pending Plan C.
