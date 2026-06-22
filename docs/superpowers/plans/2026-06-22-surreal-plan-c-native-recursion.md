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
- **Kodex reference (READ THESE — empirically-verified SurrealDB v3 patterns from a working production codebase):** every implementer AND reviewer on this plan should consult `/opt/Kodex/docs/references/surrealdb-v3/`:
  - `graph-traversal.md` — `{..N+collect}` / `{..N+shortest}`, forward/backward union, `+inclusive`, dead-end/forgotten behaviour, the "deepest-per-path vs +collect" rule (C1/C2).
  - `full-text-search.md` — `FULLTEXT … ANALYZER … BM25`, `snowball(lang)` filters, `@N@` / `search::score(N)` (C3).
  - `define-index.md` / `define-field.md` — HNSW `DIMENSION/TYPE/DIST` grammar (default TYPE is F64 — specify F32), index syntax (C5).
  - Kodex's own schema/repo (`/opt/Kodex/backend/db/schema.surql`, `app/relations/repository.py`) show these in production. See also the memory note [[kodex-surrealdb-reference]].

## Behaviour to preserve — `get_neighbors` (the parity contract)

Current `SurrealMemoryStore::get_neighbors(memory_id, depth, exclude_ids) -> (Vec<Memory>, Vec<Association>)`:
1. **Undirected** traversal (follows both `in` and `out` of `relates`).
2. `visited` seeded with `exclude_ids` + the start id; neighbours already visited/excluded are not re-added to `memories` and not re-expanded.
3. **Edges**: every association incident to an **expanded** node is pushed — INCLUDING edges whose other endpoint is excluded/already-visited (the edge is recorded before the visited check). So "all edges seen while expanding," with possible duplicates.
4. **Memories**: first-seen, **non-forgotten**, excluding start + `exclude_ids`.
5. **Does NOT traverse through `forgotten` nodes** (forgotten neighbours are recorded as edges but never enqueued).
6. Depth-bounded (`d < depth`).

⚠️ **EXPANDED ≠ COLLECTED (the edge-set trap).** A node is *expanded* (its edges pushed) only if it was dequeued with `d < depth`. The deepest level (nodes at hop `depth`) is *collected* into `memories` but **never expanded**, so their outgoing edges to hop `depth+1` are NOT in the BFS result. Therefore the native edge query must use the **expanded** set = `{root} ∪ {nodes within depth−1 hops}`, NOT the full collected set `$ids` (which includes hop-`depth` nodes). Using `$ids` would over-collect edges. Also: `depth == 0` expands nothing → returns `(empty, empty)`; the native plan must special-case it (SurrealDB `{..0}` is illegal — do NOT clamp 0→1).

Native recursion does not natively honour rule 5 (it would traverse through forgotten nodes). Task C1 resolves this empirically; the **expected primary strategy is option (b)** (traverse-through + hydrate-filter, accepting that nodes reachable only via a forgotten hop may appear — a benign superset for a graph-view), since the edge-filter option (a) reads the target node's field, which the Kodex reference never does. Task C2 implements with the accepted delta **explicitly documented and tested**.

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

With `c` forgotten, check what `{..2+collect}` returns and whether nodes reachable ONLY through `c` appear. **Default to strategy (b)**; only fall back to (a) if you empirically confirm the edge-filter works on 3.1.x (the Kodex reference filters only on edge/linked fields, never the destination node's scalar — so (a) is likely unsupported):
- (b) **PRIMARY — accept delta**: native traverses through forgotten; the hydrate step (`WHERE … AND forgotten = false`) still excludes forgotten from returned `memories`, so only the *reachable set* differs (a benign superset — extra nodes reachable via a forgotten hop). Document this as an accepted behaviour change for the graph-view API.
- (a) **probe only**: `->relates[WHERE out.forgotten = false]->memory` — verify whether the edge `WHERE` can read the target's `forgotten` on 3.1.x. If it works, prefer it (exact parity); if not (expected), use (b).

- [ ] **Step 3: Verify the 3-query plan returns the same node + edge sets as a reference BFS**

In the spike, implement both (i) the old hand-rolled BFS and (ii) the native plan. **Define the two sets distinctly:**
```
# COLLECTED (= returned memories): nodes within `depth` hops, both directions, minus start+excludes
ids      = dedup( $root.{..depth+collect}->relates->memory  ∪  $root.{..depth+collect}<-relates<-memory )
# EXPANDED (= edge sources): root ∪ nodes within (depth-1) hops  [for depth==1, expanded = {root}]
expanded = if depth >= 2 { [root] ∪ dedup( $root.{..(depth-1)+collect}->… ∪ …<-… ) } else { [root] }

1. memories = SELECT … FROM memory WHERE id IN $ids AND forgotten = false
2. edges    = SELECT … FROM relates WHERE in IN $expanded OR out IN $expanded
```
Assert (ii) matches (i)'s node set (modulo the documented forgotten delta) AND that (ii)'s edge set equals (i)'s (this is the 🔴 parity point — edges come from `$expanded`, NOT `$ids`). Test `depth ∈ {0,1,2}` (0 → empty,empty). Capture query latency for a ~1k-node graph (3-4 fixed queries vs BFS N+1 round-trips). If `<->` undirected recursion is unsupported (expected per Kodex), the union of forward `->` + backward `<-` collects is the baseline.

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

Replace the `while let … queue` BFS with, exactly mirroring C1-Step3's two-set definition:
- **Guard `depth == 0` → return `(vec![], vec![])`** immediately (the BFS expands nothing; `{..0}` is illegal — do not clamp). Clamp the upper bound to `256` (gotchas.md).
- **`ids`** (returned memories): `{..depth+collect}` forward `->relates->memory` ∪ backward `<-relates<-memory`, deduped, minus start + `exclude_ids`. Hydrate: `SELECT {MEMORY_COLS} FROM memory WHERE id IN $ids AND forgotten = false`.
- **`expanded`** (edge sources): `[root] ∪ {..(depth-1)+collect}` (both directions) for `depth >= 2`; just `[root]` for `depth == 1`. Edges: `SELECT … FROM relates WHERE in IN $expanded OR out IN $expanded`, mapped via `AssocRow`→`Association::from`.
- Keep all ids as bound `RecordId`s (gotchas.md: IN/RELATE need typed record values). Empty-input guards before each `IN` query.
- Apply the C1 forgotten strategy (default (b): traverse-through, hydrate excludes forgotten — document the accepted reachable-set superset in a code comment).

- [ ] **Step 4: Green the parity test** — same bounded command. Expected: pass.

- [ ] **Step 5: Commit** — `git commit -m "perf(memory): SurrealMemoryStore::get_neighbors via native {..+collect} recursion (was N+1 BFS)"`

> Note (scope): `MemorySearch::traverse_graph` (the hybrid-search seed traversal in `search.rs`) is a SEPARATE generic N+1 path with relation-type scoring and selective re-expansion (only `RelatedTo`/`PartOf`). Optimizing it for Surreal needs either a new trait method or a backend hint — OUT OF SCOPE for C2; record as a follow-on in `followups.md` (do not silently leave it implying it was done).

---

### Task C3: FTS stemming parity (`followups.md` #5)

**Files:** Modify `src/memory/surreal_store.rs` (`define_schema` analyzer); gated FTS test.

- [ ] **Step 1: Add `snowball(english)` to the analyzer**

In `define_schema`, change `DEFINE ANALYZER … memory_an TOKENIZERS class FILTERS lowercase, ascii;` → `… FILTERS lowercase, ascii, snowball(english);` (`snowball(lang)` is valid 3.1 syntax — Kodex uses `snowball(french)`; brings ranking closer to Lance's Tantivy stemming).
**`IF NOT EXISTS` caveat (with teeth):** no live SurrealDB store exists yet (the backend isn't production-enabled — decision C open), so `IF NOT EXISTS` is fine NOW and fresh stores (tests, new agents) get the new analyzer. BUT document explicitly in a code comment + `followups.md`: **if/when an on-disk Surreal store already exists, this change does NOT take effect** — applying it to a live store requires `DEFINE ANALYZER OVERWRITE …` **and** rebuilding the FTS index (re-index), because the existing index was built with the old analyzer. Do not silently rely on `IF NOT EXISTS` for a live migration.

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

- [ ] **Step 1: Build a synthetic 384-dim corpus** in the spike: N≈10k–20k random unit vectors inserted into a **fresh `kv-surrealkv`** (on-disk, prod engine — NOT `kv-mem`, to measure the real index) store. The existing spike store/index is dim-4 — define a NEW index `DEFINE INDEX … HNSW DIMENSION 384 TYPE F32 DIST COSINE` (gotchas.md: default TYPE is F64 — MUST specify `TYPE F32`). Plant ~50–100 **near-duplicate clusters** (a base vector + small-perturbation neighbours) with KNOWN nearest neighbours — these are the recall ground truth.
- [ ] **Step 2: Measure recall@10 on the PLANTED set** — query with each planted base vector; recall@10 = fraction of its known perturbed neighbours returned by `<|10,EF|>`, compared against the exact-cosine top-10 computed in Rust. (Recall on the bulk *random* corpus is near-meaningless — 384-dim random unit vectors are near-orthogonal/distance-concentrated, so top-10 is a near-tie; use the planted clusters as the signal, the random vectors only as index "noise"/scale.) Report recall@10 + p50/p95 latency, sweeping `EF ∈ {40, 80, 160, 320, 640}`.
- [ ] **Step 3: Record results + pick EF** — write the table to the task report; choose the smallest `EF` achieving recall@10 ≥ 0.95 (or document the achievable ceiling). State the recommended formula.
- [ ] **Step 4: If the recommendation differs from the current `(limit*4).max(40)`**, update `vector_search`/`find_similar` in `src/memory/surreal_store.rs` accordingly + re-run the gated feature-on tests (bounded). Otherwise record that the current heuristic is validated.
- [ ] **Step 5: Commit** — `bench(surreal): 384-dim HNSW recall/latency + EF tuning` (+ the store change if any).

---

### Task C6: gates + docs sync

- [ ] **Step 1: Dual-build gates** — feature-off `systemd-run --scope -q -p MemoryMax=40G ./scripts/gate-pr.sh` (ALL GREEN); feature-on `mkdir -p /tmp/ortlib && systemd-run --scope -q -p MemoryMax=40G env ORT_LIB_LOCATION=/tmp/ortlib ORT_PREFER_DYNAMIC_LINK=1 cargo clippy --all-targets --features surreal-memory -- -Dwarnings` (clean); feature-on tests RUN for real: `cargo test --features surreal-memory --test surreal_memory --lib memory::surreal_store` (green).
- [ ] **Step 2: Update `followups.md`** — mark #5 (snowball), #6 (EF tuned), #7 (recall benchmarked), #13 (removed) as RESOLVED with the evidence; add the `traverse_graph` follow-on noted in C2; update `handoff.md` "What's NOT done".
- [ ] **Step 3: Commit** the docs.

---

## Self-Review (updated after Opus review of this plan)

- **Spec coverage:** native recursion de-risked in spike (C1) then ported with parity tests (C2); FTS stemming (C3, #5); landmine removed (C4, #13); recall/latency benchmark + EF (C5, #6/#7); gates + docs (C6). The `traverse_graph` N+1 is explicitly scoped OUT and recorded, not silently skipped.
- **Default build untouched:** only `surreal_store.rs`/`spikes` (gated/standalone) + a dead-code deletion in `context.rs`; C6 compiles the feature-off path.

**Corrections applied from the Opus review:**
- 🔴 **Edge-set parity**: the edge query uses the **EXPANDED** set (`{root} ∪ within-(depth−1)`), NOT the collected `$ids` — defined explicitly in C1-Step3 + C2-Step3. Using `$ids` would over-collect deepest-level outgoing edges the BFS never records.
- 🟠 **Forgotten handling**: lead with option (b) (traverse-through + hydrate-filter, accepted superset); option (a) edge-filter on the target node's field is likely unsupported on 3.1.x (Kodex filters only edge/linked fields) → probe-only.
- 🟡 **`depth == 0` guard**: returns `(empty, empty)`; never clamp `0→1` (`{..0}` is illegal SurrealQL).
- 🟡 **`<->` expectation**: expect undirected recursion to be unsupported on 3.1.x; forward `->` + backward `<-` union is the BASELINE (Kodex's verified port did exactly this), not a fallback.
- 🟡 **C5 recall ground truth**: measure recall on **planted near-duplicate clusters**, not bulk random 384-dim vectors (distance concentration makes random top-10 a near-tie). Index must be `DIMENSION 384 TYPE F32` (default is F64).
- 🟢 **C3 caveat with teeth**: `IF NOT EXISTS` suffices now (no live store), but a live analyzer change later needs `OVERWRITE` + FTS re-index — documented, not silently relied on.

**Remaining risk:** the forgotten-traversal parity delta (option b) is a benign reachable-set superset for the graph-view API — C2 tests it explicitly so the change is conscious, not silent.

## Out of scope (future)
`MemorySearch::traverse_graph` native-recursion optimization (needs a trait method); `surreal_migrate` runtime wiring (`followups.md` #15); Lance removal / dependency-weight (#9); SurrealKV backup story (decision C); CI feature buildability (#8).
