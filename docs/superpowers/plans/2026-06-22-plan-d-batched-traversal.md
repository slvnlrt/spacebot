# Plan D — Batched graph traversal for hybrid search (followups #16)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development. Steps use checkbox (`- [ ]`).

**Goal:** Remove the last hand-rolled N+1 BFS — `MemorySearch::traverse_graph` (the hybrid-search seed traversal) — by adding two batched `MemoryBackend` primitives and rewriting the traversal level-by-level, keeping the scoring/selective-expansion logic generic (no per-backend duplication).

**Architecture:** `traverse_graph` (`src/memory/search.rs`) does, per node, `backend.get_associations(node)` (1 query) then `backend.load(neighbor)` per first-seen neighbour (1 query each) — O(nodes) round-trips. Plan C made the OTHER BFS (`SurrealMemoryStore::get_neighbors`) native; this one is generic over `dyn MemoryBackend` and carries scoring (relation-type multipliers) + selective re-expansion (only `RelatedTo`/`PartOf`), so it can't reuse `get_neighbors`. Instead, add `get_associations_for(&[ids])` (edges incident to ANY id) and `load_many(&[ids])` to the trait, and rewrite `traverse_graph` to process each BFS level in 2 batched queries — O(depth) round-trips — while replaying the EXACT same per-node/per-edge first-seen scoring in Rust.

**Tech Stack:** Rust 2024, `async-trait`. Touches `src/memory/backend.rs` (trait + `SqliteBackend`), `src/memory/surreal_store.rs` (Surreal impl, gated), `src/memory/search.rs` (the rewrite). Default build affected (SQLite path changes) → must stay green.

## Global Constraints

- Follow `RUST_STYLE_GUIDE.md`. `async-trait` for the new methods.
- **RAM/disk safety:** wrap cargo in `systemd-run --scope -q -p MemoryMax=40G -p MemorySwapMax=0 …`; `CARGO_BUILD_JOBS=8` global; keep `target/` warm (no `cargo clean`). Don't loop builds; stop after two same-cause failures.
- Real ort available — RUN tests for real (no ORT bypass for running; bypass only for feature-on clippy compile-check).
- **Behaviour preservation is the contract:** `traverse_graph`'s output (the set of `ScoredMemory` and each score) must be UNCHANGED. The score of a neighbour is `importance × edge.weight × type_multiplier` of the **first edge that reaches it in BFS order** (first-seen wins); only `RelatedTo`/`PartOf` edges re-expand; forgotten neighbours are skipped; `start_id` is pre-visited. Multipliers: Updates 1.5, CausedBy/ResultOf 1.3, RelatedTo 1.0, PartOf 0.8, Contradicts 0.5.
- All Surreal code stays `#[cfg(feature="surreal-memory")]`-gated.
- Reference: `/opt/Kodex/docs/references/surrealdb-v3/graph-traversal.md` (the recursion patterns — though this plan uses per-level batched IN queries, not deep recursion, because of the selective-expansion + per-edge scoring).

## File Structure

- **Modify** `src/memory/backend.rs`: add `get_associations_for(&self, ids: &[String]) -> Result<Vec<Association>>` and `load_many(&self, ids: &[String]) -> Result<Vec<Memory>>` to `trait MemoryBackend`; implement for `SqliteBackend`.
- **Modify** `src/memory/store.rs`: add the underlying `MemoryStore` methods if not present (`get_associations_for` = `WHERE source_id IN (…) OR target_id IN (…)`; `load_many` = `WHERE id IN (…)`), or implement directly in `SqliteBackend` via the pool.
- **Modify** `src/memory/lance.rs`: not needed (these are SQLite/graph reads).
- **Modify** `src/memory/surreal_store.rs`: implement the two trait methods (inherent + trait), gated. `get_associations_for` = `SELECT … FROM relates WHERE in IN $ids OR out IN $ids` (the edge query `get_neighbors` already uses); `load_many` = `SELECT {MEMORY_COLS} FROM memory WHERE id IN $ids`.
- **Modify** `src/memory/search.rs`: rewrite `traverse_graph` to level-batched.

### Trait additions (locked signatures)

```rust
/// All associations incident to ANY of `ids` (either endpoint). Empty → empty.
async fn get_associations_for(&self, ids: &[String]) -> Result<Vec<Association>>;
/// Batch-load memories by id (order unspecified; missing ids omitted). Empty → empty.
async fn load_many(&self, ids: &[String]) -> Result<Vec<Memory>>;
```

---

### Task D1: add `get_associations_for` + `load_many` to the backend

**Files:** `src/memory/backend.rs` (trait + SqliteBackend), `src/memory/store.rs` (SQLite queries), `src/memory/surreal_store.rs` (Surreal, gated). Tests in `backend.rs` (SQLite, no ort) and `surreal_store.rs` (gated).

**Interfaces:** Produces the two trait methods above on every backend.

- [ ] **Step 1: SQLite — failing tests first** (in `backend.rs` tests, no ort needed)

```rust
#[tokio::test]
async fn get_associations_for_returns_incident_edges() {
    let (be, _dir) = sqlite_backend().await;
    let (a,b,c) = (Memory::new("a",MemoryType::Fact), Memory::new("b",MemoryType::Fact), Memory::new("c",MemoryType::Fact));
    for m in [&a,&b,&c] { be.save(m, None).await.unwrap(); }
    be.create_association(&Association::new(&a.id,&b.id,RelationType::RelatedTo)).await.unwrap();
    be.create_association(&Association::new(&b.id,&c.id,RelationType::RelatedTo)).await.unwrap();
    // incident to {a}: only a→b
    let e = be.get_associations_for(&[a.id.clone()]).await.unwrap();
    assert_eq!(e.len(), 1);
    // incident to {a,c}: a→b and b→c (b→c has c as endpoint)
    let e2 = be.get_associations_for(&[a.id.clone(), c.id.clone()]).await.unwrap();
    assert_eq!(e2.len(), 2);
    assert!(be.get_associations_for(&[]).await.unwrap().is_empty());
}
#[tokio::test]
async fn load_many_returns_present_memories() {
    let (be,_dir) = sqlite_backend().await;
    let a = Memory::new("x", MemoryType::Fact); be.save(&a, None).await.unwrap();
    let got = be.load_many(&[a.id.clone(), "missing".into()]).await.unwrap();
    assert_eq!(got.len(), 1); assert_eq!(got[0].id, a.id);
    assert!(be.load_many(&[]).await.unwrap().is_empty());
}
```

- [ ] **Step 2: Run, watch fail** — `systemd-run --scope -q -p MemoryMax=40G cargo test -p spacebot --lib memory::backend::tests::get_associations_for_returns_incident_edges memory::backend::tests::load_many_returns_present_memories` → fails (methods missing).

- [ ] **Step 3: Implement.** Trait: add the two `async fn`. `SqliteBackend`: delegate to new `MemoryStore` methods. In `store.rs` add:
  - `get_associations_for(&self, ids)`: empty→`Ok(vec![])`; build `IN (?,?,…)` placeholders (mirror the existing `get_associations_between` style — `src/memory/store.rs`), SQL `SELECT id, source_id, target_id, relation_type, weight, created_at FROM associations WHERE source_id IN (…) OR target_id IN (…)`, bind each id once per IN list.
  - `load_many(&self, ids)`: empty→`Ok(vec![])`; `SELECT … FROM memories WHERE id IN (…)`, map rows via the same row→`Memory` path `load` uses.
  `SqliteBackend::get_associations_for`/`load_many` just call these. (`load_many` does NOT need Lance.)

- [ ] **Step 4: Surreal impl (gated).** In `surreal_store.rs` add inherent + trait methods:
  - `get_associations_for`: `SELECT meta::id(in) AS source, meta::id(out) AS target, relation_type, weight, created_at FROM relates WHERE in IN $ids OR out IN $ids` (bind `Vec<RecordId>`; empty→empty) → `AssocRow`→`Association`.
  - `load_many`: `SELECT {MEMORY_COLS} FROM memory WHERE id IN $ids` (bind `Vec<RecordId>`; empty→empty) → `MemoryRow`→`Memory`.
  Add gated tests mirroring the SQLite ones.

- [ ] **Step 5: Verify** — SQLite tests (bounded, no ort): `cargo test -p spacebot --lib memory::backend` green. Surreal (real ort): `systemd-run … cargo test --features surreal-memory --lib memory::surreal_store` green. Default `cargo check -p spacebot --lib` clean.

- [ ] **Step 6: Commit** — `feat(memory): batched get_associations_for + load_many on MemoryBackend`

---

### Task D2: rewrite `traverse_graph` level-batched (semantics-preserving)

**Files:** `src/memory/search.rs`. Test: `src/memory/search.rs` tests.

**Interfaces:** Consumes D1's `get_associations_for` + `load_many`. `traverse_graph` signature unchanged.

- [ ] **Step 1: Characterization test FIRST** (lock current behaviour)

Add a test building a scored-traversal graph and asserting the EXACT `ScoredMemory` set + scores the current implementation produces (run it against the OLD code first to capture the values), covering: a `RelatedTo` chain (re-expands), a `Contradicts`/`Updates` edge off the path (scored, NOT re-expanded), a forgotten neighbour (skipped), a node reachable by two edges (first-seen edge wins the score), and `max_depth` bound. Capture the expected scores numerically.

- [ ] **Step 2: Rewrite `traverse_graph` level-batched**

Replace the per-node loop with per-LEVEL processing, preserving first-seen-in-BFS-order scoring:
```
visited = {start_id}; frontier = [start_id]; depth = 0
while !frontier.is_empty() && depth <= max_depth:
    edges = backend.get_associations_for(&frontier)           // 1 query
    # group edges by which frontier node they're incident to, preserving frontier order,
    # and within a node preserve edges' returned order — to replay first-seen exactly.
    # collect first-seen neighbour ids (not in visited), in that order, each tagged with
    # the (relation_type, weight) of the FIRST edge that reached it.
    new = ordered first-seen neighbours with their reaching edge
    mems = backend.load_many(&new_ids).into map by id                 // 1 query
    next_frontier = []
    for (nid, rel, weight) in new (in order):
        visited.insert(nid)                  # mark first-seen
        if let Some(m) = mems.get(nid) and !m.forgotten:
            score = m.importance * weight * type_multiplier(rel)
            results.push(ScoredMemory{memory:m, score})
            if rel in {RelatedTo, PartOf}: next_frontier.push(nid)
    frontier = next_frontier; depth += 1
```
Key parity points: (a) a neighbour is marked visited the first time it is SEEN as a first-seen candidate in this level's ordered scan (so a node incident to two frontier nodes is scored once, by the first edge — same as the old BFS where `visited.insert` happened on first dequeue-expansion); (b) the OLD code marked `visited` even for forgotten/unloadable neighbours (it inserts into `visited` before the `load`), so do the same — insert into `visited` for every first-seen id, then skip scoring/expansion if forgotten/missing; (c) `start_id` pre-visited; (d) edges where the neighbour is already visited are skipped (no re-score). Match the multiplier table exactly.

> ⚠️ Subtlety: the old code processes nodes in QUEUE order and, within a node, associations in `get_associations` return order, inserting `visited` as it goes — so the "first edge that reaches a node" is well-defined by that traversal order. The batched version MUST reproduce that order: iterate `frontier` in order, and for each frontier node iterate its incident edges (grouped from `get_associations_for`) — but `get_associations_for` returns edges for the WHOLE frontier in unspecified order, so you must group-by-incident-frontier-node and iterate frontier-node-by-frontier-node to match. Document this in a comment; the characterization test guards it.

- [ ] **Step 3: Run the characterization test** — `systemd-run … cargo test -p spacebot --lib memory::search` (real ort to link). Expected: identical scores to Step 1. If any score/set differs, the ordering replay is wrong — fix until exact.

- [ ] **Step 4: Commit** — `perf(memory): level-batched traverse_graph (O(depth) queries, was O(nodes) N+1)`

---

### Task D3: gates + docs

- [ ] **Step 1: Dual-build gates** — feature-off `systemd-run … ./scripts/gate-pr.sh` ALL GREEN; feature-on `clippy --all-targets` (ORT bypass) clean; feature-on tests RUN real (`--lib memory::surreal_store` + `--test surreal_memory`) green.
- [ ] **Step 2: Docs** — mark `followups.md` #16 RESOLVED (batched, O(depth)); note the new trait primitives in `handoff.md` code map. Commit.

---

## Self-Review

- **Spec coverage:** new batched primitives (D1), semantics-preserving rewrite with a characterization test (D2), gates+docs (D3). Closes #16.
- **Behaviour risk:** the ONLY risk is reordering changing which edge "first-reaches" a multiply-incident node (→ different score). Mitigated by: (a) the explicit frontier-order + per-node edge grouping replay, (b) a characterization test capturing exact pre-change scores. If the test can't be made to match, STOP — do not ship a scoring change.
- **Default build:** D1/D2 change the SQLite (default) path too — gates cover it; the SQLite primitives are plain `IN` queries.
- **Scope:** does NOT touch `get_neighbors` (Plan C) or hybrid_search's vector/FTS/RRF; only the graph-traversal seed expansion.

## Out of scope
`surreal_migrate` runtime wiring (#15), decision C (backup), CI feature build (#8), Lance removal (#9), cfg-split dedup (#14).
