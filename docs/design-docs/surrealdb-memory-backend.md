# SurrealDB Memory Backend

Replace the multi-engine memory persistence layer (SQLite + LanceDB) with a
single embedded **SurrealDB v3** instance that unifies the document, graph,
vector, and full-text concerns the memory subsystem currently splits across two
databases.

> Status: **design / investigation**. No production code yet. This document is
> the preliminary research and target design. **Every SurrealQL snippet below is
> a hypothesis to be falsified in the Phase 0 spike, not a final schema.** A
> prior adversarial review (see "Review findings" at the end) corrected several
> claims and SurrealQL errors in the first draft; those corrections are folded
> into the body below.

## Problem

The "memory" subsystem (`src/memory/`) is the agent's long-term + working
memory. To do hybrid recall it currently stitches **three** storage engines
together by hand:

| Engine | Crate | What it holds for memory |
| --- | --- | --- |
| **SQLite** | `sqlx` | `memories` (records), `associations` (graph edges), `working_memory_*` (temporal event log) |
| **LanceDB** | `lancedb` / `lance-index` | `memory_embeddings` — 384-dim vectors (HNSW, cosine) + full-text index (Tantivy) on `content` |
| **fastembed** | `fastembed` | Generates the 384-dim embeddings (`all-MiniLM-L6-v2`) |

This split has real costs — stated honestly, because some are smaller than they
first appear:

1. **The graph is simulated in SQL.** Associations are an edge table and
   traversal is a hand-written iterative BFS (`search.rs::traverse_graph`,
   `search.rs:270-337`) that issues one query per node and loads each memory
   individually. A **second** BFS exists for the graph-view API
   (`store.rs::get_neighbors`, `store.rs:491-548`). Neither can be expressed as
   one query today.
2. **Hybrid search is glued in application code.** `hybrid_search` runs three
   independent queries (vector via Lance, FTS via Lance, graph via SQLite) and
   fuses them with Reciprocal Rank Fusion (RRF) in Rust. Each vector/FTS hit then
   does a `store.load(id)` back to SQLite (`search.rs:182,207`). *Note: this
   round-trip is a local indexed PK lookup on an in-process pool — cheap, not a
   network hop. It is a tidiness win, not a performance headline.*
3. **Two sources of truth that can drift.** `content` is duplicated into Lance
   for FTS; embeddings can desync from `memories`. `lance.rs` has a "table is
   corrupted, drop and recreate" path (`lance.rs:32-62`) — but note it creates an
   **empty** table and only *logs* that embeddings "will be rebuilt"
   (`lance.rs:59`); there is no code that actually reindexes. So today's recovery
   **silently drops all vectors** — a latent data-loss bug we must not inherit
   blindly, and a reason to verify migration counts carefully (not a working
   feature being retired).
4. **Partial-atomicity already.** The near-duplicate merge is described as
   atomic, but only the SQLite side is: `merge_memories_atomic`
   (`store.rs:266-385`) is one transaction, while the embedding delete+re-store
   happens *after* it, outside any transaction (`maintenance.rs:365-379`). So the
   merge is already only half-atomic today.
5. **No abstraction layer.** `MemoryStore` wraps `SqlitePool`, `EmbeddingTable`
   wraps `lancedb::Table`, both directly. No `MemoryBackend` trait — the engines
   are wired straight into `store.rs`, `lance.rs`, `search.rs`, `maintenance.rs`,
   and `working.rs`.

SurrealDB collapses document + graph + vector + full-text into one engine with a
single query language and one ACID transaction boundary — which is the shape of
this subsystem. The main thing to design around early is how filtered KNN
behaves today (see Risks R1) — a solvable engineering detail, not a verdict on
the approach.

## Why SurrealDB (and what it does *not* do)

What it could replace here:

- **Graph natively.** `associations` become `RELATE` edges; traversal
  (`->relates->memory`) happens inside a query.
- **Vector natively.** HNSW index (`DIST COSINE`, `TYPE F32`), KNN via the
  `<|K, EF|>` operator — replaces LanceDB.
- **Full-text natively.** `DEFINE ANALYZER` + `FULLTEXT` index with BM25 —
  replaces the Lance/Tantivy FTS.
- **One record, one transaction.** Metadata, embedding, and content on the same
  `memory` record. No duplicated `content`, no drift, no empty-table recovery
  path.
- **Embedded / in-process.** Runs against **SurrealKV** (or RocksDB) on local
  disk — same file-backed, single-machine model as the SQLite + Lance dirs.

What it explicitly does **not** do:

- **It does not generate embeddings.** SurrealDB stores/indexes vectors but does
  not compute them (it points users at FastEmbed). **`fastembed` and
  `embedding.rs` stay unchanged.**

## Scope

**In scope** — the per-agent memory subsystem only:

- `memories` + `associations` + `memory_embeddings` → one SurrealDB `memory`
  table + `relates` edges.
- Optionally the `working_memory_*` tables (decision deferred — see "Cross-store
  atomicity").

**Out of scope** — SQLite stays the backend for everything else. It is not "the
memory database"; it is the app database (conversations, channels, and the
instance-level `tasks`/`projects`/`repos`/`worktrees`). SurrealDB is added
*alongside* SQLite, memory-scoped, not a global replacement.

## Current data model (what we must reproduce)

### `memories` → `types.rs::Memory`

`id` (uuid text, PK), `content`, `memory_type` (8-variant enum), `importance`
f32, `created_at`/`updated_at`/`last_accessed_at`, `access_count`, `source?`,
`channel_id?`. The `forgotten` soft-delete flag is **not** in the original
`migrations/20260211000001_memories.sql`; it is added by
`migrations/20260212000001_forgotten_memories.sql` (`ALTER TABLE ... ADD COLUMN
forgotten`). The 1:1 copy must account for the full migration chain, not just the
first file.

### `associations` → `types.rs::Association`

`id`, `source_id`, `target_id`, `relation_type` (6-variant:
`related_to`/`updates`/`contradicts`/`caused_by`/`result_of`/`part_of`),
`weight` f32, `created_at`. Unique on `(source_id, target_id, relation_type)`,
`ON DELETE CASCADE` from `memories`.

### `memory_embeddings` (LanceDB)

`id`, `content` (for FTS), `embedding` (FixedSizeList<f32>[384]). HNSW index
(cosine), FTS index on `content`. `EMBEDDING_DIM = 384` is hardcoded in
`lance.rs:12` and validated at `lance.rs:79,144`; under SurrealDB it moves into
the HNSW index DDL (`DIMENSION 384`), so a model change means redefining the
index and a full rebuild.

### Behaviours that must survive the port

- **Hybrid search** (`search.rs`): RRF over vector + FTS + graph. Graph seeds are
  `importance >= 0.8` memories whose content **substring-matches** a query token
  (`search.rs:228-232`) — a naive keyword gate, *not* the FTS engine; the
  fragile part to preserve is this seeding heuristic, not the multipliers.
  Scoring multipliers: `updates` 1.5, `caused_by`/`result_of` 1.3, `related_to`
  1.0, `part_of` 0.8, `contradicts` 0.5 (`search.rs:310-316`).
- **Traversal semantics (subtle).** The guard is `if depth > max_depth` with
  `max_depth=2` (`search.rs:286`), so it visits depths 0, 1, **and 2 — three
  levels**. *All* edge types produce a scored hit on first encounter, but only
  `related_to`/`part_of` edges are re-queued for deeper expansion
  (`search.rs:325-331`). `forgotten` is filtered at *every* hop
  (`search.rs:306-307`). A literal one-hop SurrealQL `->relates->memory` does
  **not** reproduce this.
- **Non-hybrid modes**: `Recent` / `Important` / `Typed` — plain metadata queries
  with sorts (`created_at`, `importance`, `access_count`).
- **Maintenance** (`maintenance.rs`): importance decay with access boosts; prune
  low-importance memories older than N days; merge near-duplicates (sim > 0.95)
  via `find_similar`, combining content, **rewiring associations**, soft-deleting
  the loser.
- **`forgotten`** memories are excluded from search/recall but retained.
- `find_similar(memory_id, threshold, limit)` (`lance.rs:194-254`): reads the
  memory's *own* stored embedding, KNN-searches excluding self, over-fetching
  `limit+1`. Used by merge (`maintenance.rs:234`) — i.e. a self-referential KNN,
  exactly where the filtered-KNN bug (R1) bites hardest.

## Target SurrealDB schema (draft — to be validated in Phase 0)

> Record-id construction with UUID strings is the load-bearing detail. A raw
> `memory:5e69...-2c96` lexes the `-` as subtraction, so we **cannot** write
> `memory:$id`. Use `type::thing('memory', $id)` or bracket-escaped
> `memory:⟨$id⟩`. The "keep UUIDs, API unchanged" plan depends on this parsing
> correctly — verify it first.

```surql
-- Namespace/database selected at connect time (see "Per-agent instance model").

DEFINE TABLE memory SCHEMAFULL;
DEFINE FIELD content          ON memory TYPE string;
DEFINE FIELD memory_type      ON memory TYPE string
  ASSERT $value IN ['fact','preference','decision','identity',
                    'event','observation','goal','todo'];
DEFINE FIELD importance       ON memory TYPE float DEFAULT 0.5;
DEFINE FIELD created_at       ON memory TYPE datetime DEFAULT time::now();
DEFINE FIELD updated_at       ON memory TYPE datetime DEFAULT time::now();
DEFINE FIELD last_accessed_at ON memory TYPE datetime DEFAULT time::now();
DEFINE FIELD access_count     ON memory TYPE int DEFAULT 0;
DEFINE FIELD source           ON memory TYPE option<string>;
DEFINE FIELD channel_id       ON memory TYPE option<string>;
DEFINE FIELD forgotten        ON memory TYPE bool DEFAULT false;
DEFINE FIELD embedding        ON memory TYPE option<array<float>>;

-- Vector index. Documented grammar is HNSW DIMENSION n [TYPE t] [DIST d];
-- TYPE precedes DIST. Default TYPE is F64, so F32 must be explicit to match.
DEFINE INDEX memory_embedding_hnsw ON memory FIELDS embedding
  HNSW DIMENSION 384 TYPE F32 DIST COSINE;

-- Full-text (v3.0+ uses FULLTEXT, not the old SEARCH).
DEFINE ANALYZER memory_analyzer TOKENIZERS class FILTERS lowercase, ascii;
DEFINE INDEX memory_content_fts ON memory FIELDS content
  FULLTEXT ANALYZER memory_analyzer BM25;

DEFINE INDEX memory_type_idx       ON memory FIELDS memory_type;
DEFINE INDEX memory_importance_idx ON memory FIELDS importance;

-- Associations as graph edges.
DEFINE TABLE relates SCHEMAFULL TYPE RELATION FROM memory TO memory;
DEFINE FIELD relation_type ON relates TYPE string;
DEFINE FIELD weight        ON relates TYPE float DEFAULT 0.5;
DEFINE FIELD created_at    ON relates TYPE datetime DEFAULT time::now();
DEFINE INDEX relates_unique ON relates FIELDS in, out, relation_type UNIQUE;
```

## Query mapping (draft)

### Create a memory (+ embedding, one statement)

```surql
CREATE type::thing('memory', $id) SET
  content = $content, memory_type = $type, importance = $importance,
  source = $source, channel_id = $channel_id, embedding = $embedding;
```

### Create an association

```surql
RELATE type::thing('memory', $source)->relates->type::thing('memory', $target)
  SET relation_type = $relation_type, weight = $weight;
```

### Vector KNN (replaces `EmbeddingTable::vector_search`)

```surql
-- HNSW operator REQUIRES the EF arg: <|K, EF|>. Whether K/EF can be bound
-- params or must be literals is unverified — test in Phase 0.
SELECT id, content, importance, memory_type,
       vector::distance::knn() AS distance
FROM memory
WHERE embedding <|20, 40|> $query_embedding
  AND forgotten = false;            -- ⚠️ see R1: filter may be ignored (bug #6949)
```

`vector::distance::knn()` is valid but only returns a value for rows selected via
the `<|K,EF|>` operator in the same WHERE. (There is no `vector::distance::cosine`
— exact cosine is `vector::similarity::cosine`, relevant only if we fall back to
brute-force `<|K, COSINE|>`, which errors when combined with OR/NOT.)

### Full-text (replaces `EmbeddingTable::text_search`)

```surql
SELECT id, search::score(0) AS score
FROM memory
WHERE content @0@ $query AND forgotten = false
ORDER BY score DESC LIMIT $k;
```

The `0` in `@0@` and `search::score(0)` is a per-query predicate reference (they
must match), not an index id. This form is correct v3 syntax.

### Graph traversal (replaces the BFS) — NOT a one-hop query

The draft must reproduce depths 0–2 (three levels), re-queue only
`related_to`/`part_of`, and filter `forgotten` at every hop. SurrealQL recursive
graph syntax for this is the **riskiest** part to express; recommendation is to
**keep the traversal in Rust** (it is tested and cheap) and use SurrealDB only to
fetch a node's edges+neighbors per hop:

```surql
-- one hop, used by the Rust BFS loop:
SELECT
  ->relates.{ id: out, relation_type, weight } AS edges
FROM type::thing('memory', $node)
WHERE forgotten = false;
-- then load neighbor records (filtering forgotten) in the loop, as today.
```

Pushing the full recursion into SurrealQL is a *possible* later optimisation, not
a Phase 2 requirement. Do not claim it is both "cheap, already tested in Rust"
and "pushed into SurrealQL" — pick one. Default: Rust.

### `find_similar` (self-referential KNN — highest R1 exposure)

```surql
LET $vec = (SELECT VALUE embedding FROM ONLY type::thing('memory', $id));
SELECT id, vector::similarity::cosine(embedding, $vec) AS sim
FROM memory
WHERE embedding <|$k1, 40|> $vec AND id != type::thing('memory', $id)
  AND forgotten = false;
```

This is precisely the pattern bug #6949 breaks. If Phase 0 confirms the filter is
ignored, `find_similar` must over-fetch and filter in Rust, and merge correctness
depends on that.

### Hybrid end-goal

RRF stays in Rust (pure, tested, trivial). The win is one query per source
returning full records — *if* filtered-KNN works. Fusing all sources into one
SurrealQL transaction is a later optimisation, not a near-term goal.

## Rust integration points

| File | Change |
| --- | --- |
| `src/db.rs` | Add a `surreal` handle to the `Db` bundle; open embedded SurrealKV; apply schema idempotently. Keep `sqlite`; drop `lance` only at Phase 3 cutover. |
| `src/memory/store.rs` | Re-implement CRUD + associations + **`get_neighbors` (the 2nd BFS)** against SurrealDB. `types.rs` structs stay (serde). |
| `src/memory/lance.rs` | **Removed** at Phase 3 — vector + FTS fold into the `memory` table. |
| `src/memory/embedding.rs` | **Unchanged** (fastembed stays). |
| `src/memory/search.rs` | Vector/FTS/graph via SurrealQL; keep RRF, multipliers, seed heuristic, and the BFS in Rust; drop the SQLite-load round-trips. |
| `src/memory/maintenance.rs` | Decay/prune/merge as SurrealQL; the merge becomes genuinely single-transaction (embedding now lives on the record). |
| `src/memory/working.rs` | Deferred (see Cross-store atomicity); SQLite until then. |
| **`#[cfg(test)]` harness** | **Significant.** All memory tests use `MemoryStore::connect_in_memory()` (in-memory SQLite, `store.rs:649-671`). SurrealKV has no in-process ephemeral mode; tests would use the `kv-mem` engine — a **different engine than production `kv-surrealkv`**. The harness must be rebuilt and the engine-parity gap acknowledged. |
| `src/main.rs` (~2890), `src/api/agents.rs` (~834) | Construction sites: build stores from the `surreal` handle instead of `db.lance`. Validate `Surreal<Db>` clone semantics vs today's cheap `Arc`/pool clones (`search.rs:43-51`). |
| `Cargo.toml` | Add `surrealdb = { version = "3", features = ["kv-surrealkv","kv-mem"] }`; remove `lancedb`, `lance-index`, Arrow deps at Phase 3. **Measure build time + binary size in Phase 0** — if R1 forces keeping Lance, we pay for both. |

### Embedded init sketch

```rust
use surrealdb::engine::local::SurrealKv; // feature = "kv-surrealkv"
use surrealdb::Surreal;

let surreal = Surreal::new::<SurrealKv>(data_dir.join("surreal")).await?;
surreal.use_ns("spacebot").use_db(agent_id).await?;
surreal.query(SCHEMA_SURQL).await?; // idempotent DEFINEs
```

## Two decisions deferred to the spike (stated, not hidden)

### Per-agent instance model

Today each agent has its own process/`data_dir` (`Db::connect(data_dir)`,
`db.rs:25`; per-agent construction in `main.rs` and `api/agents.rs:807`). Two
options, with different failure/concurrency/backup consequences:

- **(A) One SurrealKV instance per agent.** Mirrors today; strong isolation;
  `use_db(agent_id)` is redundant; N embedded KV stores.
- **(B) One shared instance, `use_db(agent_id)` per agent.** Fewer handles, but
  introduces a **shared failure domain and cross-agent concurrency surface that
  does not exist today**.

Phase 0 measures both; this doc does not pre-commit. (Owner decision: decide at
the spike.)

### Cross-store atomicity

If working memory stays on SQLite, any operation that writes *both* a memory
(SurrealDB) and a `working_memory` event (SQLite) — starting with
`WorkingMemoryEventType::MemorySaved` — spans two engines with no shared
transaction. Today they share one SQLite pool. Phase 0 deliverable: **inventory
every cross-store write path** and classify each as fire-and-forget (acceptable)
or needing atomicity (argues for un-deferring working-memory migration). Owner
decision: re-evaluate after that inventory.

## Migration of existing data

Per agent, one-time, idempotent:

1. Read all `memories` + `associations` (SQLite) and all vectors (Lance).
2. `CREATE type::thing('memory', uuid) SET … embedding = ⟨vec⟩` per memory.
3. `RELATE` each association.
4. **Verify counts AND a vector sample** (the empty-table recovery bug means some
   Lance vectors may already be missing — regenerate via fastembed where absent).
5. Mark migrated (redb sentinel / marker file) so it runs once.
6. Keep old `agent.db`/`lancedb` for one release as rollback; remove later.
7. **Backup/restore story for SurrealKV must be defined** (SQLite is a copyable
   single file; SurrealKV's on-disk format and backup path is not — specify it).

## Phased plan

- **Phase 0 — Spike.** Embedded SurrealKV up, schema applied, four primitives
  validated on realistic data: insert, KNN, FTS, `RELATE`+traversal. **First
  thing to settle:** how `embedding <|K,EF|> $q AND forgotten = false` behaves at
  realistic volume — does it return ≤K *filtered* rows? If the filter is ignored
  (the situation in #6949), pick the handling strategy: filter `forgotten` in
  Rust post-hoc / over-fetch, pin a version where it works, or patch upstream.
  Also settle: UUID record-id parsing, whether K/EF can be params, `Surreal`
  clone semantics, build-size delta. Output: findings + chosen approach appended
  here.
- **Phase 1 — Store.** Port all of `store.rs` (incl. `get_neighbors` and the
  dynamic IN-clause queries `store.rs:452-487`) + associations; **rebuild the
  test harness** on `kv-mem`. Dual-run vs SQLite for parity. (The `sqlx`
  compile-time check is lost — integration tests must compensate.)
- **Phase 2 — Search.** Vector/FTS via SurrealQL; keep RRF, multipliers, seed
  heuristic, and the Rust BFS; assert existing `search.rs` tests pass.
- **Phase 3 — Maintenance + cutover.** Decay/prune/merge (merge now truly atomic);
  migrate data; remove `lance.rs`, LanceDB/Arrow deps.
- **Phase 4 — Working memory (conditional).** Driven by the cross-store atomicity
  inventory, not assumed.

Each phase ships behind a feature flag; `main` keeps SQLite+Lance until Phase 3
flips the default.

## Risks and open questions (ranked)

- **R1 — Filtered KNN: known sharp edge to design around.** SurrealDB
  [#6949](https://github.com/surrealdb/surrealdb/issues/6949) reports HNSW +
  `WHERE` filter returning *all* rows instead of the K nearest — the shape of our
  `embedding <|K,EF|> $q AND forgotten = false` and the `find_similar` merge.
  Brute-force `<|K,DIST|>` errors with OR/NOT. This is a fixable engineering
  detail, not a reason to abandon the approach: handle it by filtering
  `forgotten` in Rust post-hoc / over-fetching, pinning a version where it
  behaves, or contributing an upstream fix. Settle the handling in Phase 0 so the
  rest of the design assumes a known-good vector path. The one place it really
  bites is the self-referential `find_similar` (merge) — that query gets the most
  scrutiny.
- **R2 — Lost cross-store atomicity.** See "Cross-store atomicity." Not analyzed
  away by "memory only."
- **R3 — Per-agent instance model is load-bearing.** Shared vs per-agent instance
  determines the whole concurrency/isolation/backup model. Deferred, not ignored.
- **R4 — SurrealKV maturity & backups.** Younger than SQLite; trusting it with
  long-term memory durability needs a backup/restore plan (absent in v1).
- **R5 — Runtime-only query validation.** Losing `sqlx`'s compile-time check
  shifts the burden onto integration tests; the rebuilt `kv-mem` test harness
  must cover what the type system used to.
- **R6 — Record-id parsing.** UUID-with-hyphens ids require `type::thing`/bracket
  escaping; verify in Phase 0 — it underpins "API unchanged."
- **R7 — Embedding generation stays external.** No change to fastembed (so it
  isn't mistaken for a feature). `EMBEDDING_DIM=384` now lives in index DDL.
- **R8 — Kodex reference (private, inaccessible this session).** Before Phase 1,
  mine the owner's `Kodex` project for its SurrealDB data model, KNN+graph query
  patterns, and embedded-init — to mirror a known-good integration (and to see
  whether it has already hit #6949).

## Review findings (folded in)

An adversarial Opus review of the first draft produced the corrections now
incorporated above: `forgotten` comes from a later migration; a second BFS
(`get_neighbors`) was missed; the `store.load` round-trip is cheap; the merge is
already half-atomic; Lance "recovery" silently drops vectors; the test harness
(`connect_in_memory`) must be rebuilt on a different engine; and several SurrealQL
errors — bare `<|$k|>` (needs `<|K,EF|>`), `memory:$id` interpolation (needs
`type::thing`), `TYPE`/`DIST` ordering, and a one-hop traversal that didn't match
the depth-0–2 BFS. Verdict: **the approach is sound; start the spike, settle the
filtered-KNN handling and the two deferred decisions early, and build on them.**

## References

- SurrealDB — Agent Memory: <https://surrealdb.com/use-cases/agent-memory>
- Vector database model: <https://surrealdb.com/docs/surrealdb/models/vector>
- `DEFINE INDEX` (HNSW / FULLTEXT): <https://surrealdb.com/docs/surrealql/statements/define/indexes>
- Vector functions: <https://surrealdb.com/docs/surrealql/functions/database/vector>
- Operators (KNN `<|…|>`, full-text `@…@`): <https://surrealdb.com/docs/surrealql/operators>
- Rust embedded engine: <https://docs.rs/surrealdb/latest/surrealdb/engine/local/index.html>
- SurrealKV: <https://github.com/surrealdb/surrealkv>
- Filtered-KNN bug: <https://github.com/surrealdb/surrealdb/issues/6949>
- SurrealDB 3.0 benchmarks: <https://surrealdb.com/blog/surrealdb-3-0-benchmarks-a-new-foundation-for-performance>
</content>
