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
  - `get_associations_for`: `SELECT meta::id(in) AS source, meta::id(out) AS target, relation_type, weight, created_at FROM relates WHERE in IN $ids OR out IN $ids` (bind `Vec<RecordId>`; empty→empty) → `AssocRow`→`Association`. (Same edge query `get_neighbors` uses, IN-list form.)
  - `load_many`: `SELECT {MEMORY_COLS} FROM memory WHERE id IN $ids` (bind `Vec<RecordId>`; empty→empty) → `MemoryRow`→`Memory`. **Do NOT add `AND forgotten = false`** — unlike the `get_neighbors` hydrate, `load_many` must return forgotten rows too (the caller, `traverse_graph`, marks-visited-then-skips forgotten in Rust; filtering here would change parity). Same contract as the SQLite `load`/`load_many` (which never filter forgotten).
  Add gated tests mirroring the SQLite ones (incl. a forgotten row IS returned by `load_many`).

  Also verify (`grep -rn "impl MemoryBackend for" src/`) that `SqliteBackend` and `SurrealMemoryStore` are the ONLY impls — the trait has no default method bodies, so any other impl would fail to compile until it adds both new methods.

- [ ] **Step 5: Verify** — SQLite tests (bounded, no ort): `cargo test -p spacebot --lib memory::backend` green. Surreal (real ort): `systemd-run … cargo test --features surreal-memory --lib memory::surreal_store` green. Default `cargo check -p spacebot --lib` clean.

- [ ] **Step 6: Commit** — `feat(memory): batched get_associations_for + load_many on MemoryBackend`

---

### Task D2: rewrite `traverse_graph` level-batched (semantics-preserving)

**Files:** `src/memory/search.rs`. Test: `src/memory/search.rs` tests.

**Interfaces:** Consumes D1's `get_associations_for` + `load_many`. `traverse_graph` signature unchanged.

- [ ] **Step 1: Characterization test FIRST** (lock current behaviour)

Add a test building a scored-traversal graph and asserting the EXACT `ScoredMemory` set + scores the current implementation produces (run it against the OLD code first to capture the values), covering: a `RelatedTo` chain (re-expands), a `Contradicts`/`Updates` edge off the path (scored, NOT re-expanded), a forgotten neighbour (skipped but marked-visited), a node reachable from **two DIFFERENT frontier nodes at the same level** (first-reaching-by-frontier-order wins the score — this is the deterministic case; do NOT test two edges from the SAME source node, which is order-incidental in both old and new — see the parity caveat), and the `max_depth` bound. Capture the expected scores numerically. Keep the OLD `traverse_graph` runnable (e.g. a temporary `traverse_graph_legacy` copy) ONLY long enough to capture the golden values, then delete it.

- [ ] **Step 2: Rewrite `traverse_graph` level-batched**

Replace the per-node loop with per-LEVEL processing. **The collection is a SINGLE pass that consults AND updates `visited` as it goes** (NOT two phases) — this is what makes a node incident to two frontier nodes get scored exactly once, by the first edge, matching the old BFS:
```
visited = {start_id}; frontier = [start_id]; depth = 0
while !frontier.is_empty() && depth <= max_depth:
    edges = backend.get_associations_for(&frontier)            // 1 query
    # group edges by their incident FRONTIER node (an edge can touch a frontier
    # node via in or out); preserve the order of `frontier`.
    by_node: Map<frontier_id, Vec<Association>>  // grouped; frontier-order preserved
    # SINGLE pass — visited updated inline so duplicates within the level are dropped:
    new: Vec<(neighbour_id, RelationType, weight)> = []
    for fnode in frontier (in order):
        for assoc in by_node[fnode] (in returned order):
            nid = the endpoint of assoc that is NOT fnode
            if visited.contains(nid): continue
            visited.insert(nid)                 # mark first-seen NOW (mirrors old insert-before-load)
            new.push((nid, assoc.relation_type, assoc.weight))
    mems = backend.load_many(&new.ids).into map by id          // 1 query
    next_frontier = []
    for (nid, rel, weight) in new (in order):
        if let Some(m) = mems.get(nid) and !m.forgotten:
            score = m.importance * weight * type_multiplier(rel)
            results.push(ScoredMemory{memory: m, score})
            if rel in {RelatedTo, PartOf}: next_frontier.push(nid)
    frontier = next_frontier; depth += 1
```
Key parity points: (a) `visited` is inserted DURING the single collection pass (above), so an intra-level node reachable from two frontier nodes is collected/scored once by the first reaching edge; (b) the OLD code marks `visited` BEFORE `load` (`search.rs:292` before `:294`), so forgotten/missing neighbours ARE marked visited and never reconsidered — the pseudocode does the same (insert in the collection pass, skip scoring later if forgotten/missing); (c) `start_id` pre-visited; (d) already-visited neighbours skipped (no re-score); (e) match the multiplier table exactly. **`load_many` MUST NOT filter `forgotten`** — the forgotten check is done in Rust here (after marking visited); filtering it in SQL would "un-visit" forgotten nodes and change parity.

> ⚠️ **Parity caveat (be honest about it):** the original `MemoryStore::get_associations` has **no `ORDER BY`** (`store.rs:416`), so "the first edge that reaches a node" *within a single source node's* multi-edge set was ALREADY order-incidental (SQLite rowid order, not contractual) in the old code. The batched version deterministically preserves the **cross-frontier-node** first-reaching (frontier-order replay) — the realistic and meaningful case — but the same-single-node duplicate-edge case is best-effort in BOTH old and new. The characterization test's two-edges-to-one-node case should use edges from TWO DIFFERENT frontier nodes (deterministically preserved), not two edges from the same node (order-incidental). Document this in a code comment.

- [ ] **Step 3: Run the characterization test** — `systemd-run … cargo test -p spacebot --lib memory::search` (real ort to link). Expected: identical scores to Step 1. If any score/set differs, the ordering replay is wrong — fix until exact.

- [ ] **Step 4: Commit** — `perf(memory): level-batched traverse_graph (O(depth) queries, was O(nodes) N+1)`

---

### Task D3: gates + docs

- [ ] **Step 1: Dual-build gates** — feature-off `systemd-run … ./scripts/gate-pr.sh` ALL GREEN; feature-on `clippy --all-targets` (ORT bypass) clean; feature-on tests RUN real (`--lib memory::surreal_store` + `--test surreal_memory`) green.
- [ ] **Step 2: Docs** — mark `followups.md` #16 RESOLVED (batched, O(depth)); note the new trait primitives in `handoff.md` code map. Commit.

---

## Self-Review (updated after Opus review of this plan)

- **Spec coverage:** new batched primitives (D1), semantics-preserving rewrite with a characterization test (D2), gates+docs (D3). Closes #16.
- **Behaviour risk:** the only risk is reordering which edge "first-reaches" a multiply-incident node. Resolved: the collection pass updates `visited` inline (single pass, frontier-then-edge order) so cross-frontier-node first-reaching is deterministically preserved; the same-single-source duplicate-edge case was ALREADY order-incidental in the original (`get_associations` has no `ORDER BY`), so parity there is best-effort by definition — the characterization test deliberately uses the deterministic cross-node case.
- **Verified-correct by the plan review (against source):** parity contract (multipliers, score formula, re-expand RelatedTo/PartOf), `visited`-before-`load`, `depth > max_depth` ⇔ loop `depth <= max_depth`, FIFO = clean level-order. The Surreal/SQLite query shapes reuse proven patterns.
- **`load_many` must NOT filter `forgotten`** (the easiest latent bug — called out in D1 Step 3/4 and D2).
- **Default build:** D1/D2 change the SQLite (default) path too — gates cover it; the SQLite primitives are plain `IN` queries. Trait has no default bodies → verified `SqliteBackend` + `SurrealMemoryStore` are the only impls (D1 Step 4).
- **Scope:** does NOT touch `get_neighbors` (Plan C) or hybrid_search's vector/FTS/RRF; only the graph-traversal seed expansion.

## Out of scope
`surreal_migrate` runtime wiring (#15), decision C (backup), CI feature build (#8), Lance removal (#9), cfg-split dedup (#14).
