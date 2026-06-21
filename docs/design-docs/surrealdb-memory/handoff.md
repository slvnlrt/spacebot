# Handoff — `feat/surrealdb-memory`

Where the branch stands and what to do next. Read [`README.md`](./README.md) for
the objective and [`gotchas.md`](./gotchas.md) before editing code.

## What's done

A complete, **compile-checked** SurrealDB memory backend behind the optional
`surreal-memory` Cargo feature (default build untouched), with its logic proven
by a runnable reference crate.

| Area | Where | State |
| --- | --- | --- |
| Store (CRUD, soft-delete, associations, graph BFS, vector KNN, FTS, `find_similar`, **atomic `merge`**, **server-side `prune_below`**) | `src/memory/surreal_store.rs` | compile-checked; logic proven in reference |
| Hybrid + metadata search (vector+FTS+BFS+RRF) | `src/memory/surreal_search.rs` | compile-checked; proven in reference |
| Maintenance (decay / prune / merge) | `src/memory/surreal_maintenance.rs` | compile-checked |
| Migration (SQLite+Lance → SurrealDB) | `src/memory/surreal_migrate.rs` | compile-checked |
| In-crate integration tests (`kv-mem`) | `tests/surreal_memory.rs` | typecheck only here (needs ONNX RT to run) |
| Standalone probe + reference port | `spikes/surreal-memory/` | **12 tests run green here** |

Design decisions baked in: SurrealDB is **memory-scoped** and coexists with
SQLite; embeddings stay external (fastembed); ids stay opaque UUID strings;
`Memory`/`Association` are unchanged, bridged to the DB via internal `*Row`
structs. Two stacks (SQLite and SurrealDB) deliberately live in parallel — the
`MemoryBackend` trait abstraction is a **separate future session**, not this one.

## How to build / test

```bash
# Runnable reference + probe (no ONNX needed — depends only on surrealdb):
cd spikes/surreal-memory
cargo run            # raw SurrealQL probe (prints findings)
cargo test           # reference backend — 12 green

# Compile-check the in-crate feature (works even with ONNX download blocked):
mkdir -p /tmp/ortlib
ORT_LIB_LOCATION=/tmp/ortlib ORT_PREFER_DYNAMIC_LINK=1 \
  cargo clippy --lib --tests --features surreal-memory

# Run the in-crate tests (needs a working ONNX Runtime to link):
cargo test --features surreal-memory --test surreal_memory
```

## What's NOT done (next steps)

1. **Live wiring / cutover.** Add a `surreal` handle to the `Db` bundle
   (`src/db.rs`) and build the stores at the construction sites
   (`src/main.rs` ~2890, `src/api/agents.rs` ~834), switched by the feature /
   config. This is the main remaining work and changes runtime behaviour — do it
   only after decision (A).
2. **Run the tests + benchmark for real.** Execute `tests/surreal_memory.rs` in
   an env with ONNX Runtime, then benchmark HNSW recall/latency at **384-dim**
   and a realistic corpus (tune `EF`). All validation so far is dim-4 / small.
3. **Work the `followups.md` backlog** — notably FTS-vs-Tantivy parity (#5),
   dependency-weight / build-size measurement (#9), CI buildability of the
   feature (#8).
4. **Then** the `MemoryBackend` trait (de-duplicate the two stacks) — its own
   session, per owner.

## Open decisions (needed before cutover)

- **(A) Per-agent instance model.** One embedded SurrealKv instance **per agent**
  (mirrors today's per-agent `data_dir`; strong isolation) vs **one shared
  instance** with `use_db(agent_id)` (fewer handles, new shared failure/
  concurrency domain). Determines the concurrency/backup model. `open()` today
  assumes per-agent; `from_handle()` supports the shared case.
- **(B) Cross-store atomicity.** Working memory stays on SQLite, so a write that
  touches both memory (SurrealDB) and `working_memory_*` (SQLite) — e.g.
  `MemorySaved` — spans two engines with no shared transaction. Inventory those
  paths and decide: accept best-effort, or move working memory too.
- **(C) SurrealKV backup/restore** story (SQLite is a copyable file; SurrealKV's
  on-disk format needs a defined backup path) before trusting it with long-term
  memory.

## ⚠️ Process note

The implementation was written in one autonomous session and **only the design
doc was ever reviewed** — the ~700 lines of in-crate code were not. A cold
self-review already found and fixed two real issues (non-atomic merge,
client-side prune; see `followups.md`). **First action next session: run a code
review on the implementation diff** before wiring it live.

## Branch

All work is on `feat/surrealdb-memory` (off `main` @ v0.5.0). No PR opened.
Default build is unaffected (feature off). Nothing is wired into the daemon yet,
so merging the branch is safe but inert until the cutover.
</content>
