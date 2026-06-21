# Phase 0 spike + reference backend — SurrealDB embedded

Standalone, runnable validation harness **and a tested reference implementation**
of the memory backend for `docs/design-docs/surrealdb-memory-backend.md`. **Not
part of the spacebot crate** (own `[workspace]`, `publish = false`) — it depends
only on `surrealdb`, so it compiles and runs where the full spacebot crate cannot
(the main crate pulls `fastembed`/`ort`, whose ONNX Runtime binary download is
network-blocked here).

Two pieces:
- **`src/main.rs`** — the original probe (raw SurrealQL behaviour checks).
- **`src/lib.rs` + `tests/backend.rs`** — a faithful, **passing** reference port
  of `src/memory/{store,search,lance}.rs` (CRUD, associations, graph BFS, vector
  KNN, FTS, `find_similar`, hybrid RRF search) against embedded SurrealDB. This is
  the proven blueprint for the in-crate Phase 1 port.

```bash
cd spikes/surreal-memory
cargo run          # probe (raw findings)
cargo test         # reference backend — 12 integration tests, all green
```

Pinned: **SurrealDB 3.1.5**, engine `kv-surrealkv` (on-disk, embedded).

## What it answers, and the results (all from a real run)

| # | Question | Result |
| --- | --- | --- |
| 0 | Embedded SurrealKv on disk connects? | ✅ yes |
| 1 | Schema: `memory` SCHEMAFULL + HNSW + FULLTEXT + `RELATION` edges applies? | ✅ yes |
| 2 | UUID-string record ids | ✅ via **`type::record('memory', $uuid)`** — see finding A |
| 4 | KNN `embedding <\|K,EF\|> $q` | ✅ returns exactly K, sorted by distance |
| 5 | **Filtered KNN** `… AND forgotten = false` (the #6949 question) on embedded | ✅ **works** — returns K, no forgotten rows leak; #6949 does **not** reproduce on embedded |
| 6 | K/EF as bound params `<\|$k,$ef\|>` | ❌ **must be integer literals** (finding B) |
| 7 | FTS `content @0@ $term` + `search::score(0)` BM25 | ✅ works; discriminative term scored 2.14 (a term in 100% of docs scores ~0, as BM25 expects) |
| 8 | Graph: 1-hop, **edge-metadata projection**, 2-hop chain | ✅ all work in a single query (finding C) |
| 9 | `find_similar`: self-referential KNN (`LET $vec = (SELECT VALUE embedding FROM ONLY …)`) | ✅ works, excludes self |
| 10 | Scale: 1060 rows, k=10 filtered | ✅ returned exactly 10 rows, **0 forgotten leaked** |

## Findings that change the design doc

**A. `type::thing` does not exist in v3 — it is `type::record`.** The design doc
(and both prior reviews) assumed `type::thing('memory', $id)`. In 3.1.5 that is a
parse error ("did you maybe mean `type::record`"). Use **`type::record('table',
$id)`**. Hyphenated UUID-v4 strings round-trip cleanly; `meta::id(id)` extracts
the string key back. R6 (record-id consistency) is effectively resolved: ids stay
opaque strings end to end.

**B. KNN K/EF must be literals, not bound params.** `<|$k,$ef|>` is a parse
error. Build the operator with integer literals (`format!("<|{k},{ef}|>")`) — the
values are `i64`, so this is injection-safe.

**C. Graph edge metadata is available in one query.** `->relates.{ out,
relation_type, weight }` returns the edge's fields, and `->relates->memory->…`
chains multiple hops. So the per-relation-type scoring the Rust BFS does today
could run from a single traversal query; the BFS can stay in Rust (recommended,
it's tested) but is no longer forced by the storage layer.

**D. #6949 is a non-issue for this design.** The headline risk — HNSW filtered
KNN returning all rows — does **not** occur on embedded SurrealKv 3.1.5, at 60
rows or 1060 rows. The vector path can assume normal `WHERE … AND forgotten =
false` filtering. (The upstream report was against the remote/SDK path.)

**E. `RELATE` does not accept `type::record(...)` as endpoints.**
`RELATE type::record('memory',$s)->relates->type::record('memory',$t)` is a parse
error ("Unexpected token `::`"). Bind `RecordId` values instead and use the arrow
form: `RELATE $s->relates->$t SET …`, with
`surrealdb::types::RecordId::new("memory", id_string)`. (`CREATE`/`UPDATE`/
`DELETE`/`SELECT … FROM type::record(...)` are all fine — only the `RELATE` arrow
targets reject the function call.) The reference `add_association` uses the
RecordId form.

**F. Chrono interop is trivial.** `surrealdb::types::Datetime` wraps
`chrono::DateTime<Utc>` with `From`/`Into`, so spacebot's chrono timestamps map
directly: `Datetime::from(dt)` to write, `datetime.into()` to read.

## Compile-checking the in-crate port (`surreal-memory` feature)

The main crate's `cargo check`/`clippy` work even where the ONNX Runtime binary
download is blocked, by pointing `ort-sys` at a (possibly empty) lib dir so its
build script skips the download — `check` does not link:

```bash
mkdir -p /tmp/ortlib
ORT_LIB_LOCATION=/tmp/ortlib ORT_PREFER_DYNAMIC_LINK=1 \
  cargo check --lib --features surreal-memory
```

`cargo test` for the in-crate store still needs a real onnxruntime to link
(fastembed/ort), so the runnable tests live here in the standalone crate.

## Caveats / still to validate later

- Tiny embedding dim (4) and modest row counts — enough to prove behavior and the
  filter, not to benchmark HNSW recall/latency at 384-dim / large corpora.
- BM25 analyzer config (`class` tokenizer, `lowercase`+`ascii` filters) is a
  starting point, not tuned to match Tantivy's current behavior.
- `meta::id` vs `record::id` naming and the exact edge-projection shape may shift
  across point releases — pin the version.
</content>
