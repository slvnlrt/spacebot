# SurrealDB Memory — Plan C: native graph recursion, FTS parity, recall benchmark

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development. Steps use checkbox (`- [ ]`).

**Goal:** Pay down the SurrealDB backend's remaining quality debt — replace the hand-rolled BFS with native SurrealDB recursion (the original motivation), add stemming to FTS, benchmark HNSW recall at 384-dim and tune `EF`, and remove the dead-code backend-bypass landmine.

**Architecture:** The Surreal backend (`src/memory/surreal_store.rs`, feature `surreal-memory`) currently uses a hand-rolled N+1 BFS in `get_neighbors` and an untuned `EF`. Plan C de-risks the native-recursion SurrealQL in the runnable `spikes/surreal-memory/` crate FIRST (no ort, fast, empirical — same approach that validated the original design), then ports it into the real store with semantic-parity tests, adds `snowball(english)` to the analyzer, benchmarks/​tunes HNSW in the spike, and cleans up debt #13.

**Tech Stack:** Rust 2024, embedded SurrealDB 3.1 (`kv-surrealkv`/`kv-mem`), the `spikes/surreal-memory` reference crate (depends only on `surrealdb` — no fastembed/ort, so it builds fast and needs no ONNX).

## Global Constraints

- Follow `RUST_STYLE_GUIDE.md`. All main-crate Surreal code stays `#[cfg(feature="surreal-memory")]`-gated; the default build must stay unchanged.
- **RAM/disk safety:** wrap main-crate cargo in `systemd-run --scope -q -p MemoryMax=40G -p MemorySwapMax=0 …`; `CARGO_BUILD_JOBS=8` is global. The **spike crate is light** (no ort) — it builds/tests in seconds and needs no cap, but still avoid full-workspace commands. Keep `target/` warm (no `cargo clean`). Don't loop builds; stop after two same-cause failures.
- Real ort IS available — feature-on tests RUN here (`cargo test --features surreal-memory --test surreal_memory`). Use the `ORT_LIB_LOCATION=/tmp/ortlib` bypass ONLY for compile-check (`clippy`), never to run tests.
- **Empirical-first:** every new SurrealQL recursion shape must be proven in `spikes/surreal-memory/` before it lands in the main store. Pin SurrealDB 3.1.x (version-sensitive — see `docs/design-docs/surrealdb-memory/gotchas.md`).
- Reference for the recursion syntax: `/opt/Kodex/docs/references/surrealdb-v3/graph-traversal.md` (empirically verified `{..N+collect}` / `{..N+shortest}` patterns).

## Behaviour to preserve — `get_neighbors` (the parity contract)

Current `SurrealMemoryStore::get_neighbors(memory_id, depth, exclude_ids) -> (Vec<Memory>, Vec<Association>)`:
1. **Undirected** traversal (follows both `in` and `out` of `relates`).
2. `visited` seeded with `exclude_ids` + the start id; neighbours already visited/excluded are not re-added to `memories` and not re-expanded.
3. **Edges**: every association incident to an expanded node is pushed — INCLUDING edges whose other endpoint is excluded/already-visited (the edge is recorded before the visited check). So "all edges seen while expanding," with possible duplicates.
4. **Memories**: first-seen, **non-forgotten**, excluding start + `exclude_ids`.
5. **Does NOT traverse through `forgotten` nodes** (forgotten neighbours are recorded as edges but never enqueued).
6. Depth-bounded (`d < depth`).

Native recursion does not natively honour rule 5 (it would traverse through forgotten nodes). Task C1 resolves this empirically; Task C2 implements with any accepted delta **explicitly documented and tested**.

---

### Task C1 (spike): prototype + empirically verify the native-recursion SurrealQL

**Files:** Modify `spikes/surreal-memory/src/main.rs` (or add a probe fn) and/or `spikes/surreal-memory/tests/backend.rs`.

**Interfaces:** Produces verified query strings + a written decision (in the task report) for: undirected collect, forgotten-node handling, and incident-edge collection. No main-crate change yet.

- [ ] **Step 1: Probe undirected recursive collect at bounded depth**

In the spike (kv-mem), build a small graph (e.g. a→b→c, a→d, mark c `forgotten`) and probe, capturing actual results:
```surql
-- forward + backward collect, depth-bounded:
$root.{..2+collect}->relates->memory;
$root.{..2+collect}<-relates<-memory;
-- and test whether undirected one-shot works on 3.1.x:
$root.{..2+collect}<->relates<->memory;
```
Record which forms parse/run on embedded 3.1.x and whether `<->` recursion is supported (gotchas.md notes v3 syntax sensitivity). If `<->` is unsupported, the port uses forward+backward unioned.

- [ ] **Step 2: Determine forgotten-node traversal behaviour**

With `c` forgotten, check what `{..2+collect}` returns and whether nodes reachable ONLY through `c` appear. Decide the parity strategy and write it down:
- (a) **edge-filter**: `->relates[WHERE out.forgotten = false]->memory` to avoid expanding through forgotten (verify the edge `WHERE` can read the target's `forgotten` on 3.1.x), or
- (b) **accept delta**: native traverses through forgotten; the hydrate step still excludes forgotten from returned `memories`, so only the *reachable set* differs (extra nodes reachable via a forgotten hop). Document this as an accepted, benign behaviour change for the graph-view API.

- [ ] **Step 3: Verify the 3-query plan returns the same node + edge sets as a reference BFS**

In the spike, implement both (i) the old hand-rolled BFS and (ii) the native plan:
```
1. ids = $root.{..depth+collect}<both directions>->relates->memory   (+ dedup)
2. memories = SELECT … FROM memory WHERE id IN $ids AND forgotten=false
3. edges = SELECT … FROM relates WHERE in IN $visited OR out IN $visited
```
Assert (ii) matches (i)'s node set (modulo the documented forgotten delta) and that `edges` covers all incident edges. Capture query latency for a ~1k-node graph (collect vs BFS round-trips).

- [ ] **Step 4: Run the spike tests** — `cd spikes/surreal-memory && cargo test` (fast, no ort). Expected: green; the report records the verified query strings + the forgotten decision.

- [ ] **Step 5: Commit** — `git add spikes/surreal-memory && git commit -m "spike(surreal): verify native {..+collect} recursion for get_neighbors (parity + forgotten handling)"`

---

### Task C2 (main crate): replace `get_neighbors` BFS with native recursion

**Files:** Modify `src/memory/surreal_store.rs` (`get_neighbors`); test in its gated `#[cfg(test)] mod tests`.

**Interfaces:** Consumes the C1-verified query shapes. `get_neighbors` signature unchanged: `(memory_id, depth, exclude_ids) -> Result<(Vec<Memory>, Vec<Association>)>`.

- [ ] **Step 1: Write a parity test FIRST** (gated, kv-mem)

Build the same graph as C1 in the in-crate test; assert the native `get_neighbors` returns the expected memories (non-forgotten, excluding start+excludes, depth-bounded) and the expected incident edges. Include a case with an excluded node and a forgotten node (encoding the C1 decision).

- [ ] **Step 2: Run it, watch it fail/regress** — `systemd-run --scope -q -p MemoryMax=40G cargo test --features surreal-memory --lib memory::surreal_store::tests::get_neighbors_native` (real ort links). Expected: fails or differs before the rewrite.

- [ ] **Step 3: Reimplement `get_neighbors` with the 3-query plan**

Replace the `while let … queue` BFS with: (1) collect reachable ids via `{..depth+collect}` (forward+backward per C1, applying the chosen forgotten strategy), bound `depth` to `1..=256` (SurrealDB limit — gotchas.md), exclude start+`exclude_ids`; (2) hydrate non-forgotten memories `WHERE id IN $ids`; (3) fetch incident edges `WHERE in IN $visited OR out IN $visited` (reuse the `AssocRow`→`Association::from` mapping). Keep ids as bound `RecordId`s (gotchas.md: RELATE/IN need typed record values). Empty-input guards throughout.

- [ ] **Step 4: Green the parity test** — same bounded command. Expected: pass.

- [ ] **Step 5: Commit** — `git commit -m "perf(memory): SurrealMemoryStore::get_neighbors via native {..+collect} recursion (was N+1 BFS)"`

> Note (scope): `MemorySearch::traverse_graph` (the hybrid-search seed traversal in `search.rs`) is a SEPARATE generic N+1 path with relation-type scoring and selective re-expansion (only `RelatedTo`/`PartOf`). Optimizing it for Surreal needs either a new trait method or a backend hint — OUT OF SCOPE for C2; record as a follow-on in `followups.md` (do not silently leave it implying it was done).

---

### Task C3: FTS stemming parity (`followups.md` #5)

**Files:** Modify `src/memory/surreal_store.rs` (`define_schema` analyzer); gated FTS test.

- [ ] **Step 1: Add `snowball(english)` to the analyzer**

In `define_schema`, change `DEFINE ANALYZER … memory_an TOKENIZERS class FILTERS lowercase, ascii;` → `… FILTERS lowercase, ascii, snowball(english);` (matches Kodex's `snowball(french)` pattern; brings ranking closer to Lance's Tantivy stemming). Note: since the schema uses `IF NOT EXISTS`, an existing dev DB won't pick up the change — verify on a fresh kv-mem store (tests use fresh stores, so fine; document that existing on-disk stores need re-index/`OVERWRITE` if this ships to a live store).

- [ ] **Step 2: Test stemming behaviour** (gated): index "running quickly" and assert a query for "run" matches (stemmed), which the un-stemmed analyzer would miss. Run bounded feature-on `--lib memory::surreal_store::tests`. Commit.

---

### Task C4: remove the dead-code backend-bypass landmine (`followups.md` #13)

**Files:** `src/conversation/context.rs` (+ any test referencing it).

- [ ] **Step 1: Confirm it's dead** — `grep -rnE "build_channel_context" src/ tests/`. Expected: only the definition. If a caller exists, STOP — it's not dead; convert its signature to `&Arc<dyn MemoryBackend>` instead of deleting.
- [ ] **Step 2: Delete the dead `build_channel_context` fn** (YAGNI — it has no callers and would bypass the backend abstraction). Remove any now-unused imports it pulled in.
- [ ] **Step 3: Default-build check** — `systemd-run --scope -q -p MemoryMax=40G cargo check -p spacebot --lib`. Expected: clean. Commit `fix(memory): remove dead build_channel_context (would bypass MemoryBackend)`.

---

### Task C5 (spike): 384-dim HNSW recall/latency benchmark + EF tuning (`followups.md` #6, #7)

**Files:** Add `spikes/surreal-memory/benches/` or a `--bench`-style probe in `spikes/surreal-memory/src/main.rs`; no ort needed (use random/synthetic 384-dim vectors).

**Interfaces:** Produces a recall/latency table over `EF` values + a recommended `EF` formula; if it differs from `(limit*4).max(40)`, Task feeds C5-Step-4.

- [ ] **Step 1: Build a synthetic 384-dim corpus** in the spike: N≈10k–20k random unit vectors inserted into a `kv-surrealkv` (on-disk, prod engine — NOT `kv-mem`, to measure the real index) store with the `HNSW DIMENSION 384 TYPE F32 DIST COSINE` index. Add a handful of planted near-duplicates with known nearest neighbours.
- [ ] **Step 2: Measure recall@10 vs brute force** — for a sample of query vectors, compare the `<|10,EF|>` KNN result set against exact cosine (compute brute-force top-10 in Rust). Report recall@10 and p50/p95 query latency, sweeping `EF ∈ {40, 80, 160, 320, 640}`.
- [ ] **Step 3: Record results + pick EF** — write the table to the task report; choose the smallest `EF` achieving recall@10 ≥ 0.95 (or document the achievable ceiling). State the recommended formula.
- [ ] **Step 4: If the recommendation differs from the current `(limit*4).max(40)`**, update `vector_search`/`find_similar` in `src/memory/surreal_store.rs` accordingly + re-run the gated feature-on tests (bounded). Otherwise record that the current heuristic is validated.
- [ ] **Step 5: Commit** — `bench(surreal): 384-dim HNSW recall/latency + EF tuning` (+ the store change if any).

---

### Task C6: gates + docs sync

- [ ] **Step 1: Dual-build gates** — feature-off `systemd-run --scope -q -p MemoryMax=40G ./scripts/gate-pr.sh` (ALL GREEN); feature-on `mkdir -p /tmp/ortlib && systemd-run --scope -q -p MemoryMax=40G env ORT_LIB_LOCATION=/tmp/ortlib ORT_PREFER_DYNAMIC_LINK=1 cargo clippy --all-targets --features surreal-memory -- -Dwarnings` (clean); feature-on tests RUN for real: `cargo test --features surreal-memory --test surreal_memory --lib memory::surreal_store` (green).
- [ ] **Step 2: Update `followups.md`** — mark #5 (snowball), #6 (EF tuned), #7 (recall benchmarked), #13 (removed) as RESOLVED with the evidence; add the `traverse_graph` follow-on noted in C2; update `handoff.md` "What's NOT done".
- [ ] **Step 3: Commit** the docs.

---

## Self-Review

- **Spec coverage:** native recursion de-risked in spike (C1) then ported with parity tests (C2); FTS stemming (C3, #5); landmine removed (C4, #13); recall/latency benchmark + EF (C5, #6/#7); gates + docs (C6). The `traverse_graph` N+1 is explicitly scoped OUT and recorded, not silently skipped.
- **Default build untouched:** only `surreal_store.rs`/`spikes` (gated/standalone) + a dead-code deletion in `context.rs`; C6 compiles the feature-off path.
- **Risks:** (a) `<->` undirected recursion may be unsupported on 3.1.x → C1 falls back to forward+backward union; (b) the forgotten-traversal parity delta is the main semantic risk → C1 decides empirically, C2 tests it; (c) the benchmark uses synthetic random vectors (worst case for HNSW recall — real embeddings cluster, so real recall ≥ measured); state this caveat in C5.

## Out of scope (future)
`MemorySearch::traverse_graph` native-recursion optimization (needs a trait method); `surreal_migrate` runtime wiring (`followups.md` #15); Lance removal / dependency-weight (#9); SurrealKV backup story (decision C); CI feature buildability (#8).
