# PR text — `feat/surrealdb-memory`

> Draft PR description, kept with the feature docs. **Not opened** — the branch is
> the source of truth. Copy this into the PR body if/when one is opened.

---

## Title

`feat(memory): pluggable memory backend + embedded SurrealDB (opt-in, off by default)`

## TL;DR

Introduces a `MemoryBackend` trait so the memory subsystem's storage is pluggable,
and adds an embedded **SurrealDB** backend behind it — a single engine unifying
graph + vector + full-text where the default stack uses three (SQLite + LanceDB +
fastembed). **Off by default** (`surreal-memory` Cargo feature + `memory_backend`
config, both default to SQLite), so the production build and behaviour are
unchanged. SurrealDB is runtime-validated, benchmarked, and has a verified
backup + migration story.

## Why

The default memory graph is a hand-rolled BFS over SQL, hybrid search is glued
across two stores with per-hit round-trips, and content/embeddings can drift
across the SQLite/Lance boundary. SurrealDB collapses document + graph + vector +
FTS into one engine and one query language. This branch is the real-world test of
that thesis, done without disturbing the default path.

## What changed

**Core abstraction (always compiled, default path):**
- `src/memory/backend.rs` — **`trait MemoryBackend`** (the storage interface) +
  `SqliteBackend` (wraps the existing `MemoryStore` + `EmbeddingTable`). `MemorySearch`
  and `maintenance.rs` are now generic over `Arc<dyn MemoryBackend>`, so hybrid search
  (vector + FTS + graph + RRF) and decay/prune/merge have **one** implementation that
  drives any backend.
- Batched graph primitives `get_associations_for` / `load_many`, and a
  `sqlite_backend_arc` construction helper.

**SurrealDB backend (behind `#[cfg(feature = "surreal-memory")]`):**
- `src/memory/surreal_store.rs` — `SurrealMemoryStore: MemoryBackend`: CRUD, graph
  via native `RELATE` + `{..N+collect}` recursion, vector KNN (HNSW), FTS (BM25 +
  `snowball` stemming), atomic `merge`, server-side `prune`.
- `src/memory/surreal_migrate.rs` + `spacebot migrate-memory [--agent <id>]` CLI to
  cut an existing agent's SQLite+Lance memory over to SurrealDB.
- `memory_backend = "sqlite" | "surreal"` config selector (instance default +
  per-agent override); backend chosen at the two construction sites.

**Net:** ~21 src files, +2842 / −409. The duplicated `surreal_search.rs` /
`surreal_maintenance.rs` from earlier groundwork were deleted (the generic
`search.rs`/`maintenance.rs` replace them).

## Safety — zero default-build impact

- All SurrealDB code is `#[cfg(feature = "surreal-memory")]`-gated; with the feature
  off (the default), the binary and behaviour are unchanged. The one intentional
  default-path change: memory delete/prune now also drop the row's LanceDB embedding
  (a latent-orphan fix).
- **Gates green in BOTH configs:** feature-off `just gate-pr` ALL GREEN (885 lib
  tests, fmt, `check`/`clippy --all-targets -Dwarnings`); feature-on
  `clippy --all-targets` 0/0 + 98 tests run for real against embedded SurrealKV.
- CI now has a `check-surreal` job so the gated code can't rot.

## Verification

- Default: 885 lib tests + full gate-pr.
- SurrealDB: 8 store-primitive + graph/merge/prune/recursion tests run against a real
  embedded SurrealKV (real ONNX runtime); a golden characterization test pins
  `traverse_graph` scoring exactly across the batched rewrite.
- HNSW recall benchmarked at 384-dim (recall@10 = 1.0 on planted clusters; EF floor
  tuned 40→80 to kill a tail-latency pathology).
- Backup: cold-copy of the SurrealKV dir round-trips cleanly (verified).
- Binary size measured (release): default 270 MiB, both-backends 321 MiB (+50 MiB / +18%).

## How to use (opt-in)

```toml
# config.toml — per instance (or per agent)
[defaults]
memory_backend = "surreal"     # default: "sqlite"
```
Build with the feature: `cargo build --release --features surreal-memory`.
To migrate an existing agent's data (daemon stopped): `spacebot migrate-memory --agent <id>`.

## Deferred / accepted (tracked in `followups.md`)

- **#9 slim build** (deferred): drop LanceDB when SurrealDB is the sole memory
  backend (Lance is memory-only) → likely a binary < 270 MiB. Requires compile-time-
  exclusive backend selection. SQLite stays (it is the app-wide relational DB).
- Accepted/won't-fix: synthesized `Association.id` (#10), schema re-applied per open
  (#11), non-transactional migrate (#12, idempotent), bench p99 sampling (#17).
- Cross-store atomicity (decision B): working memory stays on SQLite; a `MemorySaved`
  spanning both engines is best-effort by design (no path assumes a shared txn).

## Review discipline

Built as five sequenced plans (A: trait, B: cutover, C: native recursion/FTS/
benchmark, D: batched traversal, E: migration CLI + dedup). Each plan was
adversarially reviewed before execution, executed task-by-task with a fresh
reviewer per task, and closed with a whole-branch review (all verdicts:
*READY TO MERGE*). Design rationale and debt are in this folder
(`README.md`, `design.md`, `handoff.md`, `gotchas.md`, `followups.md`).

## Suggested review path

1. `src/memory/backend.rs` — the trait + `SqliteBackend` (the seam).
2. `src/memory/search.rs` + `maintenance.rs` — generic-over-backend logic.
3. `src/memory/surreal_store.rs` — the SurrealDB implementation + native recursion.
4. Construction sites (`src/main.rs`, `src/api/agents.rs`) + `memory_backend` config.
5. `surreal_migrate.rs` + the `migrate-memory` CLI handler.

🤖 Generated with [Claude Code](https://claude.com/claude-code)
