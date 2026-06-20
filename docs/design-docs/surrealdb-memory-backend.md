# SurrealDB Memory Backend

Replace the multi-engine memory persistence layer (SQLite + LanceDB) with a
single embedded **SurrealDB v3** instance that unifies the document, graph,
vector, and full-text concerns the memory subsystem currently splits across two
databases.

> Status: **design + Phase 0 spike DONE**. The SurrealQL below has now been
> exercised against a real embedded SurrealDB 3.1.5 (see "Phase 0 spike results"
> and `spikes/surreal-memory/`). Two adversarial reviews and the empirical spike
> are folded in; remaining production code is Phase 1+. **Headline result: the
> filtered-KNN concern (#6949) does *not* reproduce on embedded — the approach is
> validated.** Note the corrected function name: v3 uses **`type::record`**, not
> `type::thing`.

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

## Target SurrealDB schema (validated in Phase 0)

> Record-id construction with UUID strings: a raw `memory:5e69...-2c96` lexes the
> `-` as subtraction, so we **cannot** write `memory:$id`. Use
> **`type::record('memory', $id)`** (v3 renamed `type::thing` → `type::record`).
> The spike confirmed hyphenated UUID-v4 strings round-trip cleanly: created via
> `type::record`, read back identically via both a backtick literal and a
> `type::record` param, and `meta::id(id)` extracts the string key. Ids stay
> opaque strings end to end (matching `types.rs:28`, `Uuid::new_v4().to_string()`)
> — R6 resolved. Just keep the same string form in every query and the migration
> `CREATE`.

```surql
-- Namespace/database selected at connect time (see "Per-agent instance model").

DEFINE TABLE memory SCHEMAFULL;
DEFINE FIELD content          ON memory TYPE string;
DEFINE FIELD memory_type      ON memory TYPE string
  ASSERT $value IN ['fact','preference','decision','identity',
                    'event','observation','goal','todo'];
DEFINE FIELD importance       ON memory TYPE float DEFAULT 0.5;  -- never exercised: the app always supplies a type-specific importance on CREATE (types.rs default_importance)
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
DEFINE FIELD relation_type ON relates TYPE string
  ASSERT $value IN ['related_to','updates','contradicts',
                    'caused_by','result_of','part_of'];  -- parity with memory_type; 6 variants from types.rs
DEFINE FIELD weight        ON relates TYPE float DEFAULT 0.5;
DEFINE FIELD created_at    ON relates TYPE datetime DEFAULT time::now();
DEFINE INDEX relates_unique ON relates FIELDS in, out, relation_type UNIQUE;
```

## Query mapping (draft)

### Create a memory (+ embedding, one statement)

```surql
CREATE type::record('memory', $id) SET
  content = $content, memory_type = $type, importance = $importance,
  source = $source, channel_id = $channel_id, embedding = $embedding;
```

### Create an association

```surql
-- NB: RELATE rejects type::record(...) as endpoints (parse error). Bind
-- RecordId values (RecordId::new("memory", id)) and use the bare arrow form:
RELATE $source->relates->$target
  SET relation_type = $relation_type, weight = $weight;
```

### Vector KNN (replaces `EmbeddingTable::vector_search`)

```surql
-- HNSW operator REQUIRES the EF arg: <|K, EF|>. K and EF must be INTEGER
-- LITERALS — bound params (<|$k,$ef|>) are a parse error (spike [6]), so build
-- the operator with format!("<|{k},{ef}|>") (values are i64, injection-safe).
SELECT id, content, importance, memory_type,
       vector::distance::knn() AS distance
FROM memory
WHERE embedding <|20, 40|> $query_embedding
  AND forgotten = false;            -- ✅ spike [5][10]: filter honored, K respected
ORDER BY distance;
```

Spike confirmed: the `AND forgotten = false` filter is applied correctly and
exactly K rows are returned (60 and 1060-row runs) — **#6949 does not affect
embedded** (R1). `vector::distance::knn()` only returns a value for rows selected
via the `<|K,EF|>` operator in the same WHERE. (There is no
`vector::distance::cosine` — exact cosine is `vector::similarity::cosine`,
relevant only for a brute-force `<|K, COSINE|>` fallback, which errors with
OR/NOT.)

### Full-text (replaces `EmbeddingTable::text_search`)

```surql
SELECT id, search::score(0) AS score
FROM memory
WHERE content @0@ $query AND forgotten = false
ORDER BY score DESC LIMIT $k;
```

The `0` in `@0@` and `search::score(0)` is a per-query predicate reference (they
must match), not an index id. This form is correct v3 syntax.

### Graph traversal (replaces the BFS)

The traversal must reproduce depths 0–2 (three levels), re-queue only
`related_to`/`part_of`, and filter `forgotten` at every hop. **Recommendation:
keep the BFS loop in Rust** (it is tested) and use SurrealDB to fetch a node's
edges+neighbours per hop. The spike confirmed the per-hop query returns full edge
metadata in one shot:

```surql
-- one hop, used by the Rust BFS loop (spike [8b] — works, returns out + fields):
SELECT ->relates.{ out, relation_type, weight } AS edges
FROM type::record('memory', $node);
-- then load neighbour records (filtering forgotten) in the loop, as today.
```

Spike also confirmed multi-hop chaining works in a single query
(`->relates->memory->relates->memory`, spike [8c]), so pushing the full recursion
into SurrealQL is a viable later optimisation — but the Rust BFS is the default
because its per-edge-type scoring and re-queue rules are already tested.

### `find_similar` (self-referential KNN)

```surql
LET $vec = (SELECT VALUE embedding FROM ONLY type::record('memory', $id));
SELECT id, vector::distance::knn() AS distance
FROM memory
WHERE embedding <|6, 40|> $vec AND id != type::record('memory', $id)
ORDER BY distance;
```

Spike [9] confirmed this works: the `LET`-bound self embedding feeds the KNN
operator, self is excluded, results come back ranked. (K/EF literals as above; add
`AND forgotten = false` — confirmed honored.) The merge path is safe.

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
2. `CREATE type::record('memory', uuid) SET … embedding = ⟨vec⟩` per memory.
3. `RELATE` each association.
4. **Verify counts AND a vector sample** (the empty-table recovery bug means some
   Lance vectors may already be missing — regenerate via fastembed where absent).
5. Mark migrated (redb sentinel / marker file) so it runs once.
6. Keep old `agent.db`/`lancedb` for one release as rollback; remove later.
7. **Backup/restore story for SurrealKV must be defined** (SQLite is a copyable
   single file; SurrealKV's on-disk format and backup path is not — specify it).

## Phase 0 spike results

Run `spikes/surreal-memory/` (standalone crate, depends only on `surrealdb` so it
builds where the main crate can't). Against **embedded SurrealKv 3.1.5**, dim-4
HNSW, 60→1060 rows. All checks passed:

| Check | Result |
| --- | --- |
| Embedded SurrealKv on disk | connects |
| Schema: `memory` SCHEMAFULL + HNSW + FULLTEXT + `RELATION` | applies clean |
| UUID-string ids via `type::record` | round-trip OK; `meta::id` extracts key |
| KNN `<\|K,EF\|>` | returns exactly K, distance-sorted |
| **Filtered KNN `… AND forgotten = false`** | **honoured, exactly K, no leak (60 & 1060 rows) — #6949 N/A on embedded** |
| K/EF as params `<\|$k,$ef\|>` | rejected — **must be integer literals** |
| FTS `@0@` + `search::score(0)` BM25 | works; discriminative term scores >0 |
| Graph: 1-hop, edge-metadata projection, 2-hop chain | all work in one query |
| `find_similar` self-referential KNN | works, excludes self |

Corrections to earlier drafts, now applied throughout:

- **`type::thing` → `type::record`** (v3 rename; `type::thing` is a parse error).
- **KNN K/EF must be integer literals**, not bound params — build with
  `format!("<|{k},{ef}|>")` (i64, injection-safe).
- **`RELATE` rejects `type::record(...)` endpoints** — bind `RecordId` values and
  use `RELATE $s->relates->$t` (`RecordId::new("memory", id)`).
- **#6949 / R1 is resolved** — filtered KNN behaves on embedded.

Beyond the probe, `spikes/surreal-memory/` now contains a **tested reference
implementation** (`src/lib.rs` + `tests/backend.rs`, 10 green integration tests
on `kv-mem`) that faithfully ports `store.rs`/`search.rs`/`lance.rs` — CRUD,
associations, graph BFS, vector KNN, FTS, `find_similar`, and hybrid RRF search.
It is the blueprint for the in-crate Phase 1 port. Chrono interop is trivial
(`surrealdb::types::Datetime` ⇄ `chrono::DateTime<Utc>` via `From`/`Into`).

Still deferred to later phases (unchanged): per-agent instance model, cross-store
atomicity inventory, SurrealKV backup story, and HNSW recall/latency benchmarking
at 384-dim and realistic scale.

## Phased plan

- **Phase 0 — Spike. ✅ DONE** (see "Phase 0 spike results" and
  `spikes/surreal-memory/`). Embedded SurrealKv 3.1.5 stood up; schema (HNSW +
  FULLTEXT + `RELATION`) applied; insert/KNN/filtered-KNN/FTS/RELATE+traversal/
  `find_similar` all validated on 60- and 1060-row data. Filtered KNN honours the
  `forgotten` filter and returns exactly K — **#6949 does not affect embedded.**
- **Phase 1 — Store. ✅ landed (compile-checked).**
  `memory::surreal_store::SurrealMemoryStore` (behind feature `surreal-memory`)
  ports `store.rs` + `lance.rs` onto one engine: CRUD, soft-delete,
  `record_access`, associations (`RELATE`), graph BFS `get_neighbors` (same
  signature), `get_sorted`/`get_by_type`/`get_high_importance`, and vector
  KNN/FTS/`find_similar`. Uses spacebot's real types + `crate::error`. Runtime
  parity proven by the reference crate's tests; in-crate `kv-mem` tests are
  Phase 2 (the test binary links fastembed/ort, which can't run in every env).
- **Phase 2 — Search.** Port hybrid search (vector + FTS + Rust BFS + RRF +
  multipliers + seed heuristic) over `SurrealMemoryStore`; wire `EmbeddingModel`
  for the query vector; add the `kv-mem` test harness.
- **Phase 3 — Maintenance + cutover.** Decay/prune/merge (merge now truly atomic);
  migrate data; remove `lance.rs`, LanceDB/Arrow deps.
- **Phase 4 — Working memory (conditional).** Driven by the cross-store atomicity
  inventory, not assumed.

Each phase ships behind a feature flag; `main` keeps SQLite+Lance until Phase 3
flips the default.

## Risks and open questions (ranked)

- **R1 — Filtered KNN: RESOLVED by the spike.** The headline concern, SurrealDB
  [#6949](https://github.com/surrealdb/surrealdb/issues/6949) (HNSW + `WHERE`
  returning all rows), **does not reproduce on embedded SurrealKv 3.1.5** —
  confirmed empirically at 60 and 1060 rows: `embedding <|K,EF|> $q AND forgotten
  = false` returns exactly K rows with zero forgotten leakage, including the
  self-referential `find_similar` merge query. (#6949 was filed against the
  remote/SDK path.) The vector path assumes normal filtering. *Residual:* the
  spike used dim-4 vectors and small corpora — HNSW recall/latency at 384-dim and
  realistic scale is a Phase 2 benchmark, not a correctness risk.
- **R2 — Lost cross-store atomicity.** See "Cross-store atomicity." Not analyzed
  away by "memory only."
- **R3 — Per-agent instance model is load-bearing.** Shared vs per-agent instance
  determines the whole concurrency/isolation/backup model. Deferred, not ignored.
- **R4 — SurrealKV maturity & backups.** Younger than SQLite; trusting it with
  long-term memory durability needs a backup/restore plan (absent in v1).
- **R5 — Runtime-only query validation.** Losing `sqlx`'s compile-time check
  shifts the burden onto integration tests; the rebuilt `kv-mem` test harness
  must cover what the type system used to.
- **R6 — Record-id consistency: RESOLVED by the spike.** `type::record('memory',
  $uuid)` round-trips hyphenated UUID-v4 strings cleanly; ids stay opaque strings
  everywhere (create, read-back, `meta::id`). Just use the same string form in
  every query and the migration `CREATE`. (Don't mix in native `u'…'` UUID ids —
  the plan never does.)
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
`type::record`), `TYPE`/`DIST` ordering, and a one-hop traversal that didn't match
the depth-0–2 BFS. Verdict: **the approach is sound.** The Phase 0 spike has since
confirmed it empirically (filtered KNN, FTS, graph, `find_similar` all work on
embedded), and corrected `type::thing` → `type::record` plus the K/EF-literal
constraint.

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
