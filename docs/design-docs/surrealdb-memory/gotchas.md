# SurrealDB gotchas (v3.1.5, embedded, Rust SDK)

Landmines hit while building this backend. Everything here was verified
empirically against embedded SurrealDB **3.1.5** (`spikes/surreal-memory/`).
Pin the version — several of these are version-sensitive.

## SurrealQL

- **`type::thing` is gone — use `type::record`.** v3 renamed it. `type::record('memory', $id)` builds a record id from a bound string. `type::thing(...)` is a parse error ("did you maybe mean `type::record`"). The design doc and *both* prior reviews assumed `type::thing`.
- **Build record ids from UUID strings via `type::record`, never `memory:$id`.** A raw `memory:5e69...-2c96` lexes the `-` as subtraction. Hyphenated UUID-v4 strings round-trip cleanly through `type::record` / `meta::id`. IDs stay opaque strings end to end.
- **`meta::id(id)` extracts the string key** from a record id (for reads we select `meta::id(id) AS id`).
- **KNN operator needs literal K and EF: `embedding <|K,EF|> $q`.** Bound params (`<|$k,$ef|>`) are a parse error ("expected an unsigned integer"). Build with `format!("<|{k},{ef}|>")` — the values are `i64`, so it's injection-safe. `vector::distance::knn()` returns the distance, but **only** for rows selected via the `<|…|>` operator in the same `WHERE`.
- **There is no `vector::distance::cosine`.** Exact cosine is `vector::similarity::cosine` (relevant only for a brute-force `<|K,COSINE|>` fallback, which errors when combined with `OR`/`NOT`).
- **HNSW index grammar: `DIMENSION n [TYPE t] [DIST d]`** — `TYPE` before `DIST`. Default `TYPE` is **F64**, so specify `TYPE F32` to match f32 embeddings: `DEFINE INDEX … HNSW DIMENSION 384 TYPE F32 DIST COSINE`.
- **Full-text is `FULLTEXT`, not the old `SEARCH`** (since 3.0-beta): `DEFINE INDEX … FULLTEXT ANALYZER <a> BM25`. Query with `content @0@ $term` and rank with `search::score(0)` — the `0`s are a per-query predicate reference and must match.

## Graph / edges (`RELATE`)

- **`RELATE` rejects `type::record(...)` as endpoints.** `RELATE type::record('memory',$s)->relates->type::record('memory',$t)` is a parse error ("Unexpected token `::`"). **Bind `RecordId` values** and use the bare arrow form:
  ```rust
  let s = surrealdb::types::RecordId::new("memory", id_string);
  db.query("RELATE $s->relates->$t SET relation_type=$rt, weight=$w")
      .bind(("s", s)).bind(("t", t))…
  ```
  (`CREATE`/`UPDATE`/`DELETE`/`SELECT … FROM type::record(...)` are all fine — only the `RELATE` arrow targets reject the function call.)
- **Edge endpoints (`in`/`out`) are immutable.** `UPDATE relates SET in = $new WHERE …` silently no-ops. To "rewire" an edge you must `RELATE` a new one and `DELETE` the old.
- **A duplicate `RELATE` under a UNIQUE index errors** ("index already contains …"). When rewiring, pre-delete the conflicting target edge first (`DELETE relates WHERE in=$s AND out=$t AND relation_type=$rt`) — the SurrealDB equivalent of `ON CONFLICT`.
- **Deleting a record cascade-deletes its incident edges.** Verified: after `DELETE memory:x`, no `relates` rows referencing `x` remain, and neighbours show no dangling associations. (So `delete` doesn't need a manual edge sweep — but `merge` still does its own, because it deletes edges *before* soft-deleting.)

## Transactions

- **`BEGIN; …; COMMIT;` is a real transaction and rolls back on any failed statement.** Verified: a failing statement inside the block leaves prior statements un-applied. Use this for multi-step atomic ops (e.g. `merge`). Build the block as one query string with indexed bound params (`$s0,$t0,…`).

## Rust SDK (`surrealdb` 3.1.5 crate)

- **`query(...).take::<T>(i)` requires `T: SurrealValue`,** not serde. Derive it: `#[derive(surrealdb::types::SurrealValue)]` on row structs. Public types (`Memory` etc.) keep serde/utoipa; use separate internal `*Row` structs for the DB boundary.
- **`surrealdb::types::Datetime` wraps `chrono::DateTime<Utc>`** with `From`/`Into`. Map directly: `Datetime::from(dt)` to write, `dt.into()` to read. So spacebot's chrono timestamps need no special handling.
- **Engines:** `surrealdb::engine::local::SurrealKv` (on-disk, prod) and `Mem` (ephemeral, tests) both yield `Surreal<surrealdb::engine::local::Db>` — so store code can be concrete on `Surreal<Db>`. Features: `kv-surrealkv`, `kv-mem`.
- **`.bind` accepts `(String, value)`** keys (needed for the indexed params in `merge`).

## Build / environment

- **The main crate can't link without ONNX Runtime** (fastembed → ort downloads a binary from a host that may be blocked). For compile-checking the feature without linking:
  ```bash
  mkdir -p /tmp/ortlib
  ORT_LIB_LOCATION=/tmp/ortlib ORT_PREFER_DYNAMIC_LINK=1 \
    cargo check --tests --features surreal-memory
  ```
  `cargo test` (which links) still needs a real onnxruntime — so the runnable
  tests live in the standalone `spikes/surreal-memory/` crate, which depends
  only on `surrealdb`.

## #6949 (the headline risk) — non-issue on embedded

[surrealdb#6949](https://github.com/surrealdb/surrealdb/issues/6949) reports
HNSW + `WHERE` filter returning *all* rows. **Does not reproduce on embedded
SurrealKv 3.1.5** — verified at 60 and 1060 rows, and at 95–98 % `forgotten`
ratio it returns exactly `min(K, live)`. The report was against the remote/SDK
path.

## Caveats (not bugs, but mind them)

- **Scale/recall unproven.** All validation is at dim-4 / ≤1060 rows, where HNSW
  ≈ brute force. Benchmark recall/latency at 384-dim and a realistic corpus
  before trusting it in prod. The `EF = (limit*4).max(40)` heuristic is arbitrary.
- **FTS results will differ from Tantivy.** The `class` tokenizer + lowercase/ascii
  analyzer is not tuned to match Lance's Tantivy (stemming etc.) — search
  rankings change.
- **`kv-mem` (tests) ≠ `kv-surrealkv` (prod).** Same API, different storage
  engine; tests validate logic, not the prod engine's durability.
</content>
