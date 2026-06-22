# SurrealDB Backend Conformance + Cutover — Implementation Plan (Plan B)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax.

**Goal:** Make `SurrealMemoryStore` a first-class `MemoryBackend`, selectable at runtime (behind the `surreal-memory` feature) via config, and delete the now-redundant duplicated `surreal_search.rs` / `surreal_maintenance.rs`.

**Architecture:** Plan A made memory storage pluggable behind `trait MemoryBackend` and collapsed search/maintenance onto it. Plan B implements that trait for the existing `SurrealMemoryStore` (which already has ~all the methods), adds a `memory_backend` config selector, wires the two construction sites to pick SQLite or SurrealDB, and removes the parallel `SurrealMemorySearch` / surreal maintenance code (now covered by the generic `MemorySearch` + `maintenance.rs` operating on `dyn MemoryBackend`). The default build (feature off) is completely unaffected.

**Tech Stack:** Rust 2024, tokio, `async-trait` (already a dep), embedded SurrealDB 3.1 (`surrealdb` crate, feature-gated), fastembed/ort (links the crate).

## Global Constraints

- Follow `RUST_STYLE_GUIDE.md`. `#[async_trait::async_trait]` for the impl.
- **All Surreal code stays behind `#[cfg(feature = "surreal-memory")]`.** The default (no-feature) build must be byte-for-byte unchanged — verify with a feature-off build.
- **RAM/disk safety (incident 2026-06-22):** never run unbounded/full-workspace cargo. Wrap every cargo in `systemd-run --scope -q -p MemoryMax=24G -p MemorySwapMax=0 …`; targeted builds only; `CARGO_BUILD_JOBS` is globally capped at 4. Do not loop builds; stop after two same-cause failures.
- **ONNX/ort linking:** the crate links fastembed/ort, so any `cargo build`/`test` that links needs the ORT bypass for compile-check: `mkdir -p /tmp/ortlib && ORT_LIB_LOCATION=/tmp/ortlib ORT_PREFER_DYNAMIC_LINK=1 cargo clippy --lib --features surreal-memory`. Running feature tests for real needs a working ONNX runtime; if unavailable, compile-check only and say so.
- **NEVER edit an applied migration.** No migrations change in Plan B (SurrealKV is schemaless-by-`define_schema`, not sqlx migrations).
- Decision A (per-agent SurrealKV instance, at `data_dir/surreal`) is settled — mirrors the existing per-agent data dir and `SurrealMemoryStore::open`.
- Decision B (cross-store atomicity): working memory stays on SQLite, so a `MemorySaved` write touches the memory backend (SurrealDB) and `working_memory_*` (SQLite) across two engines with no shared transaction. Accepted best-effort for the cutover — **document it in the PR summary**, do not attempt a distributed transaction.

---

## File Structure

- **Modify** `src/memory/surreal_store.rs`: add `#[async_trait] impl MemoryBackend for SurrealMemoryStore`; add inherent `get_associations_between`; the trait's `set_embedding(&Memory, …)` delegates to the existing id-based logic.
- **Modify** `src/memory.rs`: remove `pub mod surreal_search;` / `pub mod surreal_maintenance;` and their `pub use`; keep `surreal_store` (+ `surreal_migrate`). Re-export nothing new (the impl is a trait impl).
- **Delete** `src/memory/surreal_search.rs` and `src/memory/surreal_maintenance.rs` (redundant — `MemorySearch` + `maintenance.rs` are backend-agnostic after Plan A).
- **Modify** `src/config/types.rs`: add `MemoryBackendKind` enum (`Sqlite` default, `Surreal`) and a `memory_backend: MemoryBackendKind` field on `DefaultsConfig`; add the matching field to the TOML schema if `src/config/toml_schema.rs` mirrors it.
- **Modify** `src/main.rs` (~2886-2906) and `src/api/agents.rs` (~833-852): select the backend by feature + config.
- **Modify** `tests/surreal_memory.rs`: drop `SurrealMemorySearch`; exercise the generic `MemorySearch` over a `SurrealMemoryStore` backend (and `maintenance::run_maintenance` if it covered that).

### The trait gap (only two real deltas; everything else already matches)

`SurrealMemoryStore` already has, with signatures matching `MemoryBackend`: `agent_id`, `save(&Memory, Option<&[f32]>)`, `load`, `update`, `delete`, `forget`, `record_access`, `create_association`, `get_associations`, `delete_associations_for_memory`, `get_neighbors`, `get_by_type`, `get_high_importance`, `get_sorted`, `prune_below`, `merge(survivor_id, loser_id, new_content, new_embedding)`, `vector_search`, `text_search`, `find_similar`. The deltas:
1. **`set_embedding`**: inherent is `set_embedding(&self, id: &str, embedding: &[f32])`; trait is `set_embedding(&self, memory: &Memory, embedding: &[f32])`. Resolve by renaming the inherent to `set_embedding_by_id` and having the trait method call `self.set_embedding_by_id(&memory.id, embedding)` (avoids same-name recursion). Update internal callers of the inherent.
2. **`get_associations_between`**: missing — add an inherent + trait method (SurrealQL: select `relates` edges whose both endpoints are in `$ids`).

---

### Task 1: Implement `MemoryBackend` for `SurrealMemoryStore`

**Files:**
- Modify: `src/memory/surreal_store.rs`
- Test: `src/memory/surreal_store.rs` (`#[cfg(test)]` using `kv-mem`, gated) — add a `get_associations_between` test; the trait impl is exercised by existing store tests.

**Interfaces:**
- Consumes: `MemoryBackend` (`src/memory/backend.rs`), `Memory`/`Association`/`MemoryType`, `SearchSort`.
- Produces: `impl MemoryBackend for SurrealMemoryStore`; inherent `get_associations_between(&self, ids: &[String]) -> Result<Vec<Association>>`; renamed inherent `set_embedding_by_id`.

- [ ] **Step 1: Add `get_associations_between` (failing test first)**

```rust
// in the gated #[cfg(test)] mod
#[tokio::test]
async fn associations_between_returns_only_internal_edges() {
    let store = mem_store().await; // existing kv-mem helper
    let a = mem("a"); let b = mem("b"); let c = mem("c");
    for m in [&a,&b,&c] { store.save(m, None).await.unwrap(); }
    store.create_association(&Association::new(&a.id,&b.id,RelationType::RelatedTo)).await.unwrap();
    store.create_association(&Association::new(&b.id,&c.id,RelationType::RelatedTo)).await.unwrap();
    let within = store.get_associations_between(&[a.id.clone(), b.id.clone()]).await.unwrap();
    assert_eq!(within.len(), 1); // a->b only; b->c excluded (c not in set)
}
```

- [ ] **Step 2: Run it, watch it fail** (bounded, kv-mem needs no ort):
`cd spikes/surreal-memory` is NOT this — run in-crate gated test: `systemd-run --scope -q -p MemoryMax=24G cargo test --features surreal-memory --lib memory::surreal_store::tests::associations_between_returns_only_internal_edges` (needs ort to link; if ort unavailable, compile-check with the ORT_LIB_LOCATION bypass and run the equivalent in `spikes/surreal-memory` reference instead). Expected: fails (method missing).

- [ ] **Step 3: Implement `get_associations_between`**

```rust
pub async fn get_associations_between(&self, ids: &[String]) -> Result<Vec<Association>> {
    if ids.is_empty() { return Ok(Vec::new()); }
    let recs: Vec<RecordId> = ids.iter().map(|s| RecordId::new("memory", s.clone())).collect();
    let mut r = self.db
        .query("SELECT meta::id(in) AS source, meta::id(out) AS target, relation_type, weight, \
                created_at FROM relates WHERE in IN $ids AND out IN $ids")
        .bind(("ids", recs)).await.map_err(err)?;
    let rows: Vec<AssocRow> = r.take(0).map_err(err)?;
    Ok(rows.into_iter().map(Association::from).collect())
}
```

- [ ] **Step 4: Rename `set_embedding` → `set_embedding_by_id`; add the trait impl**

Rename the inherent `pub async fn set_embedding(&self, id: &str, …)` to `set_embedding_by_id` and update any in-crate caller (grep `set_embedding` in `src/memory/`). Then add the impl block:

```rust
#[async_trait::async_trait]
impl crate::memory::backend::MemoryBackend for SurrealMemoryStore {
    fn agent_id(&self) -> &str { self.agent_id() }
    async fn save(&self, m: &Memory, e: Option<&[f32]>) -> Result<()> { self.save(m, e).await }
    async fn set_embedding(&self, m: &Memory, e: &[f32]) -> Result<()> { self.set_embedding_by_id(&m.id, e).await }
    async fn delete(&self, id: &str) -> Result<()> { self.delete(id).await }
    async fn load(&self, id: &str) -> Result<Option<Memory>> { self.load(id).await }
    async fn update(&self, m: &Memory) -> Result<()> { self.update(m).await }
    async fn forget(&self, id: &str) -> Result<bool> { self.forget(id).await }
    async fn record_access(&self, id: &str) -> Result<()> { self.record_access(id).await }
    async fn get_by_type(&self, t: MemoryType, l: i64) -> Result<Vec<Memory>> { self.get_by_type(t, l).await }
    async fn get_high_importance(&self, th: f32, l: i64) -> Result<Vec<Memory>> { self.get_high_importance(th, l).await }
    async fn get_sorted(&self, s: SearchSort, l: i64, t: Option<MemoryType>) -> Result<Vec<Memory>> { self.get_sorted(s, l, t).await }
    async fn create_association(&self, a: &Association) -> Result<()> { self.create_association(a).await }
    async fn get_associations(&self, id: &str) -> Result<Vec<Association>> { self.get_associations(id).await }
    async fn get_associations_between(&self, ids: &[String]) -> Result<Vec<Association>> { self.get_associations_between(ids).await }
    async fn delete_associations_for_memory(&self, id: &str) -> Result<u64> { self.delete_associations_for_memory(id).await }
    async fn get_neighbors(&self, id: &str, d: u32, ex: &[String]) -> Result<(Vec<Memory>, Vec<Association>)> { self.get_neighbors(id, d, ex).await }
    async fn vector_search(&self, q: &[f32], l: usize) -> Result<Vec<(String, f32)>> { self.vector_search(q, l).await }
    async fn text_search(&self, q: &str, l: usize) -> Result<Vec<(String, f32)>> { self.text_search(q, l).await }
    async fn find_similar(&self, id: &str, th: f32, l: usize) -> Result<Vec<(String, f32)>> { self.find_similar(id, th, l).await }
    async fn prune_below(&self, th: f32, older: chrono::DateTime<chrono::Utc>) -> Result<u64> { self.prune_below(th, older).await }
    async fn merge(&self, s: &str, l: &str, c: &str, e: Option<&[f32]>) -> Result<()> { self.merge(s, l, c, e).await }
}
```
(The `agent_id` trait method vs inherent `agent_id()` have the same name and signature — the inherent shadows fine via `self.agent_id()`; if the compiler complains about recursion, inline `&self.agent_id` field access instead.)

- [ ] **Step 5: Build-check (feature on) + run gated tests if ort available**

`mkdir -p /tmp/ortlib && systemd-run --scope -q -p MemoryMax=24G env ORT_LIB_LOCATION=/tmp/ortlib ORT_PREFER_DYNAMIC_LINK=1 cargo clippy --lib --tests --features surreal-memory -- -Dwarnings`
Expected: clean. If a real ONNX runtime is present, also `cargo test --features surreal-memory --lib memory::surreal_store` (bounded) — all green.

- [ ] **Step 6: Commit** — `git add src/memory/surreal_store.rs && git commit -m "feat(memory): impl MemoryBackend for SurrealMemoryStore + get_associations_between"`

---

### Task 2: Config backend selector

**Files:**
- Modify: `src/config/types.rs` (enum + `DefaultsConfig` field), and `src/config/toml_schema.rs` if it mirrors `DefaultsConfig`.
- Test: `src/config/types.rs` (serde default test).

**Interfaces:**
- Produces: `pub enum MemoryBackendKind { Sqlite, Surreal }` (serde `rename_all = "lowercase"`, `Default` = `Sqlite`); `DefaultsConfig.memory_backend: MemoryBackendKind`.

- [ ] **Step 1: Add the enum + field with a default test**

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemoryBackendKind {
    #[default]
    Sqlite,
    Surreal,
}
```
Add to `DefaultsConfig`: `#[serde(default)] pub memory_backend: MemoryBackendKind,`. Test: deserializing a `DefaultsConfig` TOML without the key yields `MemoryBackendKind::Sqlite`; with `memory_backend = "surreal"` yields `Surreal`.

- [ ] **Step 2: Run the test (bounded)** — `systemd-run --scope -q -p MemoryMax=24G cargo test -p spacebot --lib config::` (no ort needed for config tests if they don't link the whole bin — if they do, use the ort bypass). Expected: pass.

- [ ] **Step 3: Mirror in `toml_schema.rs`** if that file defines a parallel schema (check with `grep -n memory_janitor src/config/toml_schema.rs` — if `DefaultsConfig` fields are mirrored there, add `memory_backend`). If not mirrored, skip.

- [ ] **Step 4: Commit** — `git commit -m "feat(config): memory_backend selector (sqlite|surreal)"`

---

### Task 3: Wire the construction sites to select the backend

**Files:**
- Modify: `src/main.rs` (~2886-2906), `src/api/agents.rs` (~833-852).

**Interfaces:**
- Consumes: `MemoryBackendKind`, `SurrealMemoryStore::open`, `SqliteBackend::new`, the embedding dimension constant (`crate::memory::lance::EMBEDDING_DIM` or the 384 used by `define_schema`).

- [ ] **Step 1: Replace the backend construction at both sites with a feature+config switch**

```rust
let backend: Arc<dyn MemoryBackend> = {
    #[cfg(feature = "surreal-memory")]
    {
        if matches!(resolved_memory_backend, MemoryBackendKind::Surreal) {
            let dim = crate::memory::lance::EMBEDDING_DIM as usize;
            Arc::new(
                crate::memory::SurrealMemoryStore::open(&data_dir, &agent_config.id, dim).await?,
            ) as Arc<dyn MemoryBackend>
        } else {
            let store = MemoryStore::with_agent_id(db.sqlite.clone(), &agent_config.id);
            let embedding_table = EmbeddingTable::open_or_create(&db.lance).await?;
            embedding_table.ensure_fts_index().await.ok();
            Arc::new(SqliteBackend::new(store, embedding_table))
        }
    }
    #[cfg(not(feature = "surreal-memory"))]
    {
        let store = MemoryStore::with_agent_id(db.sqlite.clone(), &agent_config.id);
        let embedding_table = EmbeddingTable::open_or_create(&db.lance).await?;
        embedding_table.ensure_fts_index().await.ok();
        Arc::new(SqliteBackend::new(store, embedding_table))
    }
};
let memory_search = Arc::new(MemorySearch::new(backend, embedding_model.clone()));
```
`resolved_memory_backend` = the agent's effective `memory_backend` (defaults inherited from `DefaultsConfig`). `SurrealMemoryStore::open` returns `Arc<Self>`; wrap/coerce to `Arc<dyn MemoryBackend>`. Mirror exactly in `api/agents.rs` (it uses `agent_id`/`db` similarly). Keep the SQLite branch's `ensure_fts_index` call (Plan A behaviour).

- [ ] **Step 2: Default-build compile-check (feature OFF — must be unchanged behaviour)**

`systemd-run --scope -q -p MemoryMax=24G cargo check -p spacebot --bin spacebot` (or the lib+bins). Expected: clean; the `#[cfg(not(...))]` arm is identical to today.

- [ ] **Step 3: Feature-on compile-check**

`mkdir -p /tmp/ortlib && systemd-run --scope -q -p MemoryMax=24G env ORT_LIB_LOCATION=/tmp/ortlib ORT_PREFER_DYNAMIC_LINK=1 cargo clippy --features surreal-memory --bin spacebot -- -Dwarnings`. Expected: clean.

- [ ] **Step 4: Commit** — `git commit -m "feat(memory): select SQLite/SurrealDB backend at construction by config"`

---

### Task 4: Delete redundant surreal search/maintenance; migrate the surreal test

**Files:**
- Delete: `src/memory/surreal_search.rs`, `src/memory/surreal_maintenance.rs`
- Modify: `src/memory.rs` (drop their `pub mod` + `pub use SurrealMemorySearch`), `tests/surreal_memory.rs`.

**Interfaces:**
- Consumes: generic `MemorySearch` (Plan A), `maintenance::run_maintenance` (Plan A), `SurrealMemoryStore` as `Arc<dyn MemoryBackend>`.

- [ ] **Step 1: Confirm nothing references the deleted items**

`grep -rnE "SurrealMemorySearch|surreal_search|surreal_maintenance" src/ tests/` — the only hits should be the module decls/re-exports in `memory.rs` and the usage in `tests/surreal_memory.rs`. If anything in `src/` (other than memory.rs) uses them, STOP and report (the cutover should already route Surreal through generic `MemorySearch`).

- [ ] **Step 2: Delete the files and their module wiring**

`git rm src/memory/surreal_search.rs src/memory/surreal_maintenance.rs`; in `src/memory.rs` remove the two `#[cfg(feature="surreal-memory")] pub mod surreal_{search,maintenance};` lines and the `pub use surreal_search::SurrealMemorySearch;`.

- [ ] **Step 3: Migrate `tests/surreal_memory.rs`**

Replace the `SurrealMemorySearch::new(store)` usage (~line 230) with the generic path: build `let backend: Arc<dyn MemoryBackend> = store.clone();` (a `SurrealMemoryStore` is now a backend) and `let search = MemorySearch::new(backend, embedding_model);`, then assert the same hybrid/metadata search behaviour. If the test asserted maintenance behaviour via `surreal_maintenance`, switch to `maintenance::run_maintenance(backend.clone(), embedding_model, &config)`. Where the test needs an `EmbeddingModel`, reuse the crate's shared-model test helper pattern (as in `tests/maintenance.rs`).

- [ ] **Step 4: Feature-on compile-check + run if ort available**

`mkdir -p /tmp/ortlib && systemd-run --scope -q -p MemoryMax=24G env ORT_LIB_LOCATION=/tmp/ortlib ORT_PREFER_DYNAMIC_LINK=1 cargo clippy --features surreal-memory --lib --tests -- -Dwarnings`. Expected: clean (no references to deleted modules). If ONNX present: `cargo test --features surreal-memory --test surreal_memory` (bounded) green.

- [ ] **Step 5: Commit** — `git commit -m "refactor(memory): drop duplicated surreal search/maintenance (covered by generic MemorySearch/maintenance)"`

---

### Task 5: Gates + dual-build verification

- [ ] **Step 1: Default build gates (feature OFF)** — bounded `just gate-pr` equivalent: `systemd-run --scope -q -p MemoryMax=24G ./scripts/gate-pr.sh`. Expected: all green (fmt, check --all-targets, clippy -Dwarnings, test --lib, test --no-run).
- [ ] **Step 2: Feature-on clippy (bounded, ort bypass)** — `mkdir -p /tmp/ortlib && systemd-run --scope -q -p MemoryMax=24G env ORT_LIB_LOCATION=/tmp/ortlib ORT_PREFER_DYNAMIC_LINK=1 cargo clippy --features surreal-memory --all-targets -- -Dwarnings`. Expected: clean.
- [ ] **Step 3: If a real ONNX runtime is available**, run the feature-on memory + surreal tests bounded and (optionally) the 384-dim recall sanity from Plan C's scope; otherwise record that runtime validation of the live Surreal path is deferred to an ort-capable environment.
- [ ] **Step 4: Commit any fmt fixes; update the branch handoff doc** noting decisions A (per-agent) settled, B (cross-store best-effort) documented, C (SurrealKV backup) still open.

---

## Self-Review

- **Spec coverage:** trait impl + gaps (Task 1), config selector (Task 2), construction wiring (Task 3), delete duplicates + migrate test (Task 4), gates (Task 5). Followup #3 (duplication) is closed by Task 4.
- **Default build untouched:** Tasks 3/5 explicitly compile-check the feature-off path; all Surreal code is `#[cfg]`-gated.
- **Known risks to flag for review:** (a) `set_embedding`/`agent_id` same-name inherent-vs-trait — handled by rename / field access, but verify no infinite recursion; (b) `SurrealMemoryStore::open` returns `Arc<Self>` — confirm the `as Arc<dyn MemoryBackend>` coercion compiles (may need `Arc<SurrealMemoryStore>` → `Arc<dyn _>` unsizing, which works); (c) the surreal store still uses a hand-rolled BFS in `get_neighbors`/search seeds (Plan C replaces it) — out of scope here; (d) running the feature for real needs ONNX — compile-checks are the gate in this environment.

## Out of scope (Plan C)
Native graph recursion (`{..N+collect}`/`{..N+shortest}`) replacing the hand-rolled BFS, `snowball(english)` FTS analyzer, EF tuning, 384-dim recall/latency benchmark.
