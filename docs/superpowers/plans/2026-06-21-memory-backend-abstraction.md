# Memory Backend Abstraction — Implementation Plan (Plan A)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Introduce a `MemoryBackend` trait so memory storage is pluggable, with the current SQLite+LanceDB stack as the first implementation, leaving the default build behaviourally identical.

**Architecture:** Today `MemorySearch` is hard-wired to the concrete `MemoryStore` (SQLite) + `EmbeddingTable` (Lance), and ~19 call sites reach through `memory_search.store()` / `.embedding_table()`. This plan extracts an async trait `MemoryBackend` that owns the union of those primitives (CRUD, graph, vector/text, prune/merge), implements it as `SqliteBackend { store, embeddings }`, makes `MemorySearch` hold `Arc<dyn MemoryBackend>`, and collapses the duplicated hybrid-search/maintenance logic onto the trait. The SurrealDB backend (Plan B) and quality work (Plan C) build on this. No Surreal code is touched here.

**Tech Stack:** Rust (edition 2024), tokio, `async-trait = "0.1"` (already a dependency), sqlx (SQLite), lancedb, fastembed (ONNX — *not* needed for this plan's tests).

## Global Constraints

- Rust edition 2024; follow `RUST_STYLE_GUIDE.md`.
- `async-trait` 0.1 is already in `Cargo.toml` — use `#[async_trait::async_trait]` for the trait and impls.
- **NEVER edit an applied migration in place.** No migration changes are needed in Plan A.
- Default (no-feature) build behaviour is equivalent to today **with one intentional exception**: memory deletion (direct delete *and* prune) now also removes the row's LanceDB embedding, instead of leaving an orphan. This is a latent-bug fix; it changes `vector_search`/`find_similar` results only for stores that had previously accumulated orphan embeddings. Call this out in the PR summary. Everything else is a pure refactor.
- Delivery gates before any push: `just preflight` then `just gate-pr` (or `./scripts/preflight.sh` then `./scripts/gate-pr.sh`).
- The embedding **model** (`EmbeddingModel`/fastembed) stays OUTSIDE the backend — backends *store* embeddings, they do not *generate* them. Embeddings are passed in as `&[f32]`.
- Tests pass embeddings as fixtures (`vec![0.1_f32; DIM]`) so the model is never *invoked*, but note `fastembed`/ort is an unconditional dependency (`Cargo.toml`), so the test binary still links the ONNX toolchain regardless — there is no "ONNX-free" build. Construct the Lance store the way the existing tests do: a `tempfile::tempdir()` + `lancedb::connect(dir.path()...)` (see `src/memory/search.rs:577` and `src/memory/maintenance.rs:531`). Do **not** use `lancedb::connect("memory://")` — that idiom is not used anywhere in this codebase.

---

## File Structure

- **Create** `src/memory/backend.rs` — the `MemoryBackend` trait (the storage interface) and `SqliteBackend` (wraps `Arc<MemoryStore>` + `EmbeddingTable`).
- **Modify** `src/memory.rs` — declare `pub mod backend;` and re-export `MemoryBackend`, `SqliteBackend`.
- **Modify** `src/memory/search.rs` — `MemorySearch` holds `Arc<dyn MemoryBackend>` instead of concrete `store`/`embedding_table`; `hybrid_search`/`metadata_search`/`traverse_graph` call trait methods; drop the leaky `store()`/`embedding_table()` accessors (replaced by delegating methods).
- **Modify** `src/memory/maintenance.rs` — `run_maintenance*`, `merge_similar_memories`, decay, prune take `&dyn MemoryBackend` (+ `EmbeddingModel`) instead of `&MemoryStore` + `&EmbeddingTable`.
- **Modify** call sites that used `memory_search.store()` / `.embedding_table()` (19 sites): `src/tools/memory_save.rs`, `src/api/memories.rs`, `src/agent/cortex.rs`, `src/agent/maintenance.rs`, `src/main.rs`, `src/api/agents.rs`.
- **No deletions** in Plan A (`surreal_*` files are untouched; their collapse happens in Plan B once the trait is proven).

### The trait (locked interface — every task below depends on these exact signatures)

```rust
// src/memory/backend.rs
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use crate::error::Result;
use crate::memory::search::SearchSort;
use crate::memory::types::{Association, Memory, MemoryType};

#[async_trait]
pub trait MemoryBackend: Send + Sync + std::fmt::Debug {
    fn agent_id(&self) -> &str;

    // CRUD (+ embedding storage)
    async fn save(&self, memory: &Memory, embedding: Option<&[f32]>) -> Result<()>;
    async fn load(&self, id: &str) -> Result<Option<Memory>>;
    async fn update(&self, memory: &Memory) -> Result<()>;
    async fn set_embedding(&self, memory: &Memory, embedding: &[f32]) -> Result<()>;
    async fn delete(&self, id: &str) -> Result<()>; // removes memory + its embedding + incident edges
    async fn forget(&self, id: &str) -> Result<bool>;
    async fn record_access(&self, id: &str) -> Result<()>;

    // typed / sorted reads
    async fn get_by_type(&self, memory_type: MemoryType, limit: i64) -> Result<Vec<Memory>>;
    async fn get_high_importance(&self, threshold: f32, limit: i64) -> Result<Vec<Memory>>;
    async fn get_sorted(&self, sort: SearchSort, limit: i64, memory_type: Option<MemoryType>) -> Result<Vec<Memory>>;

    // associations / graph
    async fn create_association(&self, association: &Association) -> Result<()>;
    async fn get_associations(&self, memory_id: &str) -> Result<Vec<Association>>;
    async fn get_associations_between(&self, memory_ids: &[String]) -> Result<Vec<Association>>;
    async fn delete_associations_for_memory(&self, memory_id: &str) -> Result<u64>;
    async fn get_neighbors(&self, memory_id: &str, depth: u32, exclude_ids: &[String]) -> Result<(Vec<Memory>, Vec<Association>)>;

    // vector / text
    async fn vector_search(&self, query_embedding: &[f32], limit: usize) -> Result<Vec<(String, f32)>>;
    async fn text_search(&self, query: &str, limit: usize) -> Result<Vec<(String, f32)>>;
    async fn find_similar(&self, memory_id: &str, threshold: f32, limit: usize) -> Result<Vec<(String, f32)>>;

    // maintenance
    async fn prune_below(&self, threshold: f32, older_than: DateTime<Utc>) -> Result<u64>;
    async fn merge(&self, survivor_id: &str, loser_id: &str, new_content: &str, new_embedding: Option<&[f32]>) -> Result<()>;
}
```

### Reconciliation rules baked into `SqliteBackend` (the SQLite+Lance stack does not match the trait 1:1)

| Trait method | SQLite+Lance implementation |
| --- | --- |
| `save(m, Some(emb))` | `store.save(m).await?;` then `embeddings.store(&m.id, &m.content, emb).await?;` (today's two-step from `memory_save.rs`) |
| `save(m, None)` | `store.save(m).await?;` (no embedding yet) |
| `set_embedding(m, emb)` | `embeddings.store(&m.id, &m.content, emb).await?;` (Lance row carries content for its own FTS) |
| `delete(id)` | `store.delete(id).await?;` (SQLite FK `ON DELETE CASCADE` drops edges) then `embeddings.delete(id).await?;` (Lance has no FK — must delete explicitly; current `MemoryStore::delete` is SQLite-only, so this is the intentional orphan-cleanup behaviour change) |
| `vector_search` / `text_search` / `find_similar` | delegate to `EmbeddingTable` (FTS/vectors live in Lance, not SQLite) |
| `prune_below` | mirror the current `maintenance.rs` prune **exactly**: one SQL `SELECT id FROM memories WHERE importance < ? AND memory_type != 'identity' AND created_at < ?` (via `self.store.pool()`, **no LIMIT**), then loop `self.delete(id)` (which now also drops the Lance embedding). Return the count. Do NOT cap the scan. |
| `merge(survivor_id, loser_id, content, Some(emb))` | load survivor → set `content`+`updated_at=now()`; `store.merge_memories_atomic(&updated_survivor, &loser).await?;` (this call **internally** forgets the loser and rewires its associations onto the survivor — the caller does NOT set `forgotten`). Then replicate `merge_pair`'s embedding fix-up: `embeddings.delete(survivor_id)` → `embeddings.store(survivor_id, content, emb)` → `embeddings.delete(loser_id)`. (`EmbeddingTable::store` is an **append**, `lance.rs:118`, so the survivor's old row MUST be deleted first or you get duplicate vectors.) |
| everything else | direct 1:1 delegation to `MemoryStore` (`load`, `update`, `forget`, `record_access`, `get_by_type`, `get_high_importance`, `get_sorted`, `create_association`, `get_associations`, `get_associations_between`, `delete_associations_for_memory`, `get_neighbors`) |

---

### Task 1: Define `MemoryBackend` trait + `SqliteBackend`, with conformance tests

**Files:**
- Create: `src/memory/backend.rs`
- Modify: `src/memory.rs` (add `pub mod backend;` + re-exports)
- Test: `src/memory/backend.rs` (inline `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `MemoryStore` (`src/memory/store.rs`), `EmbeddingTable` (`src/memory/lance.rs`), `Memory`/`Association`/`MemoryType` (`src/memory/types.rs`), `SearchSort` (`src/memory/search.rs`).
- Produces: `pub trait MemoryBackend` (exact signatures above); `pub struct SqliteBackend` with `pub fn new(store: Arc<MemoryStore>, embeddings: EmbeddingTable) -> Self`.

- [ ] **Step 1: Write the failing test** (round-trip + delete cascade through the trait)

```rust
// in src/memory/backend.rs
#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::store::MemoryStore;
    use crate::memory::lance::EmbeddingTable;
    use crate::memory::types::{Memory, MemoryType};

    const DIM: usize = 384;

    // Returns the backend AND the TempDir guard — keep the guard alive for the
    // test's duration (dropping it deletes the Lance directory). Mirrors the
    // construction in src/memory/search.rs:577 and maintenance.rs:531.
    async fn sqlite_backend() -> (SqliteBackend, tempfile::TempDir) {
        let store = MemoryStore::connect_in_memory().await;
        let dir = tempfile::tempdir().unwrap();
        let conn = lancedb::connect(dir.path().to_str().unwrap()).execute().await.unwrap();
        let embeddings = EmbeddingTable::open_or_create(&conn).await.unwrap();
        (SqliteBackend::new(store, embeddings), dir)
    }

    #[tokio::test]
    async fn save_load_delete_roundtrips_through_trait() {
        let (be, _dir) = sqlite_backend().await;
        let be: &dyn MemoryBackend = &be;
        let m = Memory::new("the sky is blue", MemoryType::Fact);
        let emb = vec![0.1_f32; DIM];

        be.save(&m, Some(&emb)).await.unwrap();
        let loaded = be.load(&m.id).await.unwrap().expect("memory present");
        assert_eq!(loaded.content, "the sky is blue");

        be.delete(&m.id).await.unwrap();
        assert!(be.load(&m.id).await.unwrap().is_none());
        // embedding gone from Lance too: vector search returns nothing for it
        let hits = be.vector_search(&emb, 5).await.unwrap();
        assert!(hits.iter().all(|(id, _)| id != &m.id));
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p spacebot --lib memory::backend::tests::save_load_delete_roundtrips_through_trait`
Expected: FAIL — `cannot find type SqliteBackend` / module `backend` not declared.

- [ ] **Step 3: Implement the trait and `SqliteBackend`**

Write `src/memory/backend.rs` with the `MemoryBackend` trait (exact signatures from the File Structure section) and:

```rust
#[derive(Debug, Clone)]
pub struct SqliteBackend {
    store: std::sync::Arc<crate::memory::store::MemoryStore>,
    embeddings: crate::memory::lance::EmbeddingTable,
}

impl SqliteBackend {
    pub fn new(
        store: std::sync::Arc<crate::memory::store::MemoryStore>,
        embeddings: crate::memory::lance::EmbeddingTable,
    ) -> Self {
        Self { store, embeddings }
    }
}

#[async_trait::async_trait]
impl MemoryBackend for SqliteBackend {
    fn agent_id(&self) -> &str { self.store.agent_id() }

    async fn save(&self, memory: &Memory, embedding: Option<&[f32]>) -> Result<()> {
        self.store.save(memory).await?;
        if let Some(emb) = embedding {
            self.embeddings.store(&memory.id, &memory.content, emb).await?;
        }
        Ok(())
    }
    async fn set_embedding(&self, memory: &Memory, embedding: &[f32]) -> Result<()> {
        self.embeddings.store(&memory.id, &memory.content, embedding).await
    }
    async fn delete(&self, id: &str) -> Result<()> {
        self.store.delete(id).await?;       // FK ON DELETE CASCADE drops edges
        self.embeddings.delete(id).await?;  // Lance row has no FK — delete explicitly
        Ok(())
    }
    async fn load(&self, id: &str) -> Result<Option<Memory>> { self.store.load(id).await }
    async fn update(&self, m: &Memory) -> Result<()> { self.store.update(m).await }
    async fn forget(&self, id: &str) -> Result<bool> { self.store.forget(id).await }
    async fn record_access(&self, id: &str) -> Result<()> { self.store.record_access(id).await }
    async fn get_by_type(&self, t: MemoryType, limit: i64) -> Result<Vec<Memory>> { self.store.get_by_type(t, limit).await }
    async fn get_high_importance(&self, th: f32, limit: i64) -> Result<Vec<Memory>> { self.store.get_high_importance(th, limit).await }
    async fn get_sorted(&self, sort: SearchSort, limit: i64, t: Option<MemoryType>) -> Result<Vec<Memory>> { self.store.get_sorted(sort, limit, t).await }
    async fn create_association(&self, a: &Association) -> Result<()> { self.store.create_association(a).await }
    async fn get_associations(&self, id: &str) -> Result<Vec<Association>> { self.store.get_associations(id).await }
    async fn get_associations_between(&self, ids: &[String]) -> Result<Vec<Association>> { self.store.get_associations_between(ids).await }
    async fn delete_associations_for_memory(&self, id: &str) -> Result<u64> { self.store.delete_associations_for_memory(id).await }
    async fn get_neighbors(&self, id: &str, depth: u32, exclude: &[String]) -> Result<(Vec<Memory>, Vec<Association>)> { self.store.get_neighbors(id, depth, exclude).await }
    async fn vector_search(&self, q: &[f32], limit: usize) -> Result<Vec<(String, f32)>> { self.embeddings.vector_search(q, limit).await }
    async fn text_search(&self, q: &str, limit: usize) -> Result<Vec<(String, f32)>> { self.embeddings.text_search(q, limit).await }
    async fn find_similar(&self, id: &str, th: f32, limit: usize) -> Result<Vec<(String, f32)>> { self.embeddings.find_similar(id, th, limit).await }

    async fn prune_below(&self, threshold: f32, older_than: chrono::DateTime<chrono::Utc>) -> Result<u64> {
        // Mirror maintenance.rs:165-179 exactly: one unbounded SQL SELECT, then
        // delete each (self.delete also drops the Lance embedding — the
        // intentional orphan-cleanup behaviour change).
        use sqlx::Row as _;
        let rows = sqlx::query(
            "SELECT id FROM memories WHERE importance < ? AND memory_type != 'identity' AND created_at < ?",
        )
        .bind(threshold)
        .bind(older_than)
        .fetch_all(self.store.pool())
        .await
        .map_err(|e| crate::error::DbError::Query(e.to_string()))?;
        let mut n = 0;
        for row in rows {
            let id: String = row.try_get("id").map_err(|e| crate::error::DbError::Query(e.to_string()))?;
            self.delete(&id).await?;
            n += 1;
        }
        Ok(n)
    }

    async fn merge(&self, survivor_id: &str, loser_id: &str, new_content: &str, new_embedding: Option<&[f32]>) -> Result<()> {
        // merge_memories_atomic(updated_survivor, loser) internally forgets the
        // loser and rewires its associations onto the survivor — DO NOT set
        // forgotten here. We only build the updated survivor.
        let mut survivor = self.store.load(survivor_id).await?
            .ok_or_else(|| crate::error::DbError::Query(format!("merge survivor {survivor_id} not found")))?;
        let loser = self.store.load(loser_id).await?
            .ok_or_else(|| crate::error::DbError::Query(format!("merge loser {loser_id} not found")))?;
        survivor.content = new_content.to_string();
        survivor.updated_at = chrono::Utc::now();
        self.store.merge_memories_atomic(&survivor, &loser).await?;
        // Embedding fix-up, replicating maintenance.rs::merge_pair exactly:
        // EmbeddingTable::store is an append, so delete the survivor's old
        // vector before re-storing, and drop the loser's vector.
        if let Some(emb) = new_embedding {
            self.embeddings.delete(survivor_id).await?;
            self.embeddings.store(survivor_id, new_content, emb).await?;
        }
        self.embeddings.delete(loser_id).await?;
        Ok(())
    }
}
```

Add to `src/memory.rs`:

```rust
pub mod backend;
pub use backend::{MemoryBackend, SqliteBackend};
```

(No new error variant is needed: `DbError` has no `NotFound`, but it has `DbError::Query(String)` (`src/error.rs:118` is `Surreal`, the `Query(String)` variant is in the same enum) — the code above uses `DbError::Query`. Also requires `self.store.pool()` to be accessible; it is (`maintenance.rs:178` calls `memory_store.pool()`). If `pool()` is not `pub`, make it `pub(crate)` in this task.)

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p spacebot --lib memory::backend::tests::save_load_delete_roundtrips_through_trait`
Expected: PASS.

- [ ] **Step 5: Add merge + prune conformance tests, run them**

```rust
    #[tokio::test]
    async fn merge_moves_content_and_forgets_loser() {
        let (be, _dir) = sqlite_backend().await;
        let mut a = Memory::new("cats are mammals", MemoryType::Fact);
        a.importance = 0.9;
        let b = Memory::new("cats are animals", MemoryType::Fact);
        be.save(&a, Some(&vec![0.1; DIM])).await.unwrap();
        be.save(&b, Some(&vec![0.2; DIM])).await.unwrap();

        be.merge(&a.id, &b.id, "cats are mammals\n\ncats are animals", Some(&vec![0.15; DIM])).await.unwrap();

        let survivor = be.load(&a.id).await.unwrap().unwrap();
        assert!(survivor.content.contains("mammals") && survivor.content.contains("animals"));
        // merge_memories_atomic forgets the loser internally — the caller never
        // sets it. Verify the function did so.
        let loser = be.load(&b.id).await.unwrap().unwrap();
        assert!(loser.forgotten);
        // The loser's embedding must be gone (no stale vector for it).
        assert!(be.vector_search(&vec![0.2; DIM], 5).await.unwrap().iter().all(|(id, _)| id != &b.id));
    }

    #[tokio::test]
    async fn prune_below_skips_identity_and_recent() {
        let (be, _dir) = sqlite_backend().await;
        let mut low = Memory::new("trivia", MemoryType::Fact);
        low.importance = 0.1;
        low.created_at = chrono::Utc::now() - chrono::Duration::days(30);
        let mut ident = Memory::new("my name is X", MemoryType::Identity);
        ident.importance = 0.1;
        ident.created_at = low.created_at;
        be.save(&low, None).await.unwrap();
        be.save(&ident, None).await.unwrap();

        let cut = chrono::Utc::now() - chrono::Duration::days(7);
        let n = be.prune_below(0.5, cut).await.unwrap();
        assert_eq!(n, 1);
        assert!(be.load(&low.id).await.unwrap().is_none());
        assert!(be.load(&ident.id).await.unwrap().is_some()); // identity preserved
    }
```

Run: `cargo test -p spacebot --lib memory::backend::tests`
Expected: PASS (3 tests).

- [ ] **Step 6: Commit**

```bash
git add src/memory/backend.rs src/memory.rs src/error.rs
git commit -m "feat(memory): MemoryBackend trait + SqliteBackend impl"
```

---

### Task 2: Make `MemorySearch` hold `Arc<dyn MemoryBackend>`

**Files:**
- Modify: `src/memory/search.rs:40-96` (fields, `new`, accessors) and the `hybrid_search`/`metadata_search`/`traverse_graph` bodies.
- Test: `src/memory/search.rs` (existing tests + one new delegating test)

**Interfaces:**
- Consumes: `MemoryBackend`, `SqliteBackend` (Task 1).
- Produces: `MemorySearch::new(backend: Arc<dyn MemoryBackend>, embedding_model: Arc<EmbeddingModel>) -> Self`, and exactly three accessors used by Task 3 call sites:
  `backend(&self) -> &Arc<dyn MemoryBackend>`, `embedding_model_arc(&self) -> &Arc<EmbeddingModel>`, `agent_id(&self) -> &str`.
  Call sites invoke trait methods directly on the backend (`memory_search.backend().load(...).await`), so NO per-method delegators are needed — this avoids the "forgot to add a delegator" failure mode.

- [ ] **Step 1: Update the struct, `new`, and accessors**

Replace the `store`/`embedding_table` fields with `backend: Arc<dyn MemoryBackend>`; keep `embedding_model`. Update `Clone` (clone the two `Arc`s) and `Debug` (`Arc<dyn MemoryBackend>: Debug` holds because the trait requires `Debug`). Replace `store()`/`embedding_table()` with:

```rust
pub fn backend(&self) -> &Arc<dyn MemoryBackend> { &self.backend }
pub fn embedding_model_arc(&self) -> &Arc<EmbeddingModel> { &self.embedding_model }
pub fn agent_id(&self) -> &str { self.backend.agent_id() }
```

`&Arc<dyn MemoryBackend>` auto-derefs, so `memory_search.backend().load(id).await` and every other trait method work at call sites without further plumbing.

- [ ] **Step 2: Update `hybrid_search`/`metadata_search`/`traverse_graph` to call `self.backend.*`**

Replace `self.store.<m>()` with `self.backend.<m>()` and `self.embedding_table().<m>()` with `self.backend.<m>()` (the trait merges them: `vector_search`/`text_search`/`find_similar` now live on `backend`). The RRF and scoring logic is unchanged.

- [ ] **Step 3: Run the existing search tests to verify no behavioural regression**

Run: `cargo test -p spacebot --lib memory::search`
Expected: PASS (existing tests, after updating their construction to `MemorySearch::new(Arc::new(SqliteBackend::new(store, embeddings)), model)`).

- [ ] **Step 4: Commit**

```bash
git add src/memory/search.rs
git commit -m "refactor(memory): MemorySearch over Arc<dyn MemoryBackend>"
```

---

### Task 3: Migrate the 19 call sites off `.store()` / `.embedding_table()`

**Files (full list — verified via grep, the earlier draft undercounted):**
- Modify: `src/tools/memory_save.rs` (save path + embedding store + compensation deletes)
- Modify: `src/tools/memory_delete.rs` (`store()` at :88 → `forget`/`load`)
- Modify: `src/tools/memory_recall.rs` (`store()` at :232, :259 `agent_id`, :323)
- Modify: `src/api/memories.rs` (FOUR sites: :149, :235, :255 `get_associations_between`, :293)
- Modify: `src/agent/cortex.rs` (:2472 maintenance spawn, :4534 association pass — uses `find_similar` + `create_association`)
- Modify: `src/agent/maintenance.rs` (:91 maintenance spawn)

**Interfaces:**
- Consumes: `MemorySearch::backend()` / `embedding_model_arc()` / `agent_id()` from Task 2; all trait methods called on `backend()`.
- Produces: no new public surface.

- [ ] **Step 1: Find EVERY site (do not trust the list above — re-grep)**

Run: `grep -rnE "memory_search\.(store|embedding_table)\(|\.store\(\)\.|\.embedding_table\(\)" src/`
Expected: the sites in the file list. Each `memory_search.store()` → `memory_search.backend()`; each `.embedding_table()` use is folded into a `backend()` trait call. If grep shows a site not in the list, add it.

- [ ] **Step 2: Rewrite `memory_save.rs`**

Replace `let store = memory_search.store(); store.save(&memory)` + the later `embedding_table().store(id, content, emb)` with: keep the `embed_one` call (model is external), then `self.memory_search.backend().save(&memory, Some(&embedding)).await?`. Replace compensation `store().delete(...)` / `store().delete_associations_for_memory(...)` with `self.memory_search.backend().delete(...)` / `.delete_associations_for_memory(...)`. Replace `store().create_association` / `store().load` / `store().agent_id()` with `backend().create_association` / `.load` / `.agent_id()`.

- [ ] **Step 3: Rewrite `memory_delete.rs`, `memory_recall.rs`, and all four `api/memories.rs` sites**

Mechanical: `let store = memory_search.store();` → `let store = memory_search.backend();` (the variable can keep its name; `&Arc<dyn MemoryBackend>` auto-derefs, so `store.forget(id)`, `store.load(id)`, `store.record_access(id)`, `store.agent_id()`, `store.get_associations_between(ids)`, `store.get_neighbors(...)` all resolve to trait methods). Read each site and confirm the method it calls is on the trait (all of `forget`/`load`/`record_access`/`agent_id`/`get_associations_between`/`get_neighbors`/`get_sorted` are).

- [ ] **Step 4: Rewrite the maintenance/cortex spawn sites to pass `backend()`**

`run_maintenance_with_cancel(memory_search.store(), memory_search.embedding_table(), memory_search.embedding_model_arc(), ...)` → `run_maintenance_with_cancel(memory_search.backend().clone(), memory_search.embedding_model_arc().clone(), ...)` (the new 2-handle signature lands in Task 4 — execute Task 4 together with this step; see ordering note). For the cortex association pass (`cortex.rs:4534`), replace the `embedding_table().find_similar(...)` use with `deps.memory_search.backend().find_similar(...)` and keep `create_association` via `backend()`.

- [ ] **Step 5: Compile**

Run: `cargo check -p spacebot`
Expected: no errors; `grep -rnE "memory_search\.(store|embedding_table)\(" src/` returns zero hits.

- [ ] **Step 6: Commit**

```bash
git add src/tools/memory_save.rs src/tools/memory_delete.rs src/tools/memory_recall.rs src/api/memories.rs src/agent/cortex.rs src/agent/maintenance.rs
git commit -m "refactor(memory): migrate call sites to MemoryBackend"
```

---

### Task 4: Make `maintenance.rs` generic over `&dyn MemoryBackend`

**Files:**
- Modify: `src/memory/maintenance.rs` (`run_maintenance`, `run_maintenance_with_cancel`, `merge_similar_memories`, decay, prune — replace `&MemoryStore` + `&EmbeddingTable` params with `Arc<dyn MemoryBackend>`).
- Test: `src/memory/maintenance.rs` (existing maintenance tests, re-pointed at `SqliteBackend`).

**Interfaces:**
- Consumes: `MemoryBackend` trait methods. Merge scan uses `get_sorted(Recent, MERGE_SCAN_LIMIT, None)`; `find_similar`; `merge`; `prune_below`. **Decay uses `get_by_type(t, 1000)`** (NOT `get_sorted` — the current decay reads `get_by_type(mem_type, 1000)` at `maintenance.rs:114`, which orders `importance DESC, updated_at DESC`; `get_sorted(Recent, ..)` orders `created_at DESC` and would pick a different 1000-row set). `update`.
- Produces: `pub async fn run_maintenance_with_cancel(backend: Arc<dyn MemoryBackend>, embedding_model: Arc<EmbeddingModel>, config: &MaintenanceConfig, cancel: watch::Receiver<bool>) -> Result<MaintenanceReport>` (and matching `run_maintenance`).

- [ ] **Step 1: Rewrite `merge_similar_memories` to call `backend.merge(...)`**

The orchestration (scan `get_sorted(Recent, MERGE_SCAN_LIMIT, None)`, `find_similar`, `choose_merge_pair`, `merged_memory_content`) is unchanged; the final step becomes:

```rust
let content = merged_memory_content(winner.content.clone(), &loser.content);
let embedding = embedding_model.embed_one(&content).await?;
backend.merge(&winner.id, &loser.id, &content, Some(&embedding)).await?;
```

`choose_merge_pair` and `merged_memory_content` stay as free fns (this is the logic that was duplicated in `surreal_maintenance.rs`; Plan B deletes that copy).

- [ ] **Step 2: Route decay/prune through the trait**

Decay keeps its Rust formula and its current iteration: per non-identity type via `backend.get_by_type(t, 1000)` (matching `maintenance.rs:114`), apply the formula, `backend.update(&m)`. Prune calls `backend.prune_below(threshold, older_than)` (which now also drops Lance embeddings — the intentional behaviour change).

- [ ] **Step 3: Run maintenance tests**

Run: `cargo test -p spacebot --lib memory::maintenance`
Expected: PASS (existing tests, construction re-pointed at `SqliteBackend`).

- [ ] **Step 4: Commit**

```bash
git add src/memory/maintenance.rs
git commit -m "refactor(memory): maintenance over MemoryBackend"
```

---

### Task 5: Update construction sites + green gates

**Files:**
- Modify: `src/main.rs:2886-2906`, `src/api/agents.rs:833-852`.

**Interfaces:**
- Consumes: `SqliteBackend::new`, `MemorySearch::new` (new signature).

- [ ] **Step 1: Update both construction sites**

```rust
let memory_store = MemoryStore::with_agent_id(db.sqlite.clone(), &agent_config.id);
let embedding_table = EmbeddingTable::open_or_create(&db.lance).await?;
let backend: Arc<dyn MemoryBackend> = Arc::new(SqliteBackend::new(memory_store, embedding_table));
let memory_search = Arc::new(MemorySearch::new(backend, embedding_model.clone()));
```

(Mirror exactly in `api/agents.rs`.)

- [ ] **Step 2: Full build + clippy**

Run: `cargo clippy -p spacebot --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 3: Run the memory + integration test suite**

Run: `cargo test -p spacebot --lib memory && cargo test -p spacebot --test maintenance`
Expected: PASS.

- [ ] **Step 4: Delivery gates**

Run: `just preflight && just gate-pr` (or the two `scripts/*.sh`).
Expected: all green.

- [ ] **Step 5: Commit**

```bash
git add src/main.rs src/api/agents.rs
git commit -m "refactor(memory): build SqliteBackend at construction sites"
```

---

## Self-Review (completed; updated after Opus adversarial review)

- **Spec coverage:** trait (Task 1), `MemorySearch` swap (Task 2), call-site migration (Task 3), maintenance (Task 4), construction + gates (Task 5). The Lance-only `embedding_table()` leak is resolved by folding `vector/text/find_similar/set_embedding` into the trait. ✓
- **Ordering note:** Tasks 3 and 4 are mutually referencing (call sites pass `backend()` to maintenance whose signature changes in Task 4). Execute Task 4's signature change and Task 3's call-site updates together; commit boundaries stay as written by using `cargo check` (not full build) at Task 3 Step 5.
- **Type consistency:** `merge(survivor_id, loser_id, new_content, new_embedding)` is the single signature across trait def, `SqliteBackend` impl, and `maintenance.rs` caller. `find_similar(id, threshold, limit)` matches `EmbeddingTable` and `SurrealMemoryStore`. ✓

**Corrections applied from the Opus review (each verified against source):**
- 🔴 `merge` now deletes the survivor's old Lance vector before re-storing (`EmbeddingTable::store` is an append, `lance.rs:118`) and deletes the loser's vector — replicating `maintenance.rs::merge_pair:360-379`. The earlier draft duplicated/leaked vectors.
- 🔴 `merge` no longer sets `loser.forgotten`: `merge_memories_atomic` forgets the loser and rewires its edges internally (`store.rs:370-377`).
- 🔴 Test scaffold uses `tempfile::tempdir()` (the established pattern, `search.rs:577`), not the non-existent `lancedb::connect("memory://")`. The "links without ONNX" claim was false (fastembed is an unconditional dep) and has been removed.
- 🔴 Call-site list expanded from 6→ the full set: added `memory_delete.rs`, `memory_recall.rs`, and all four `api/memories.rs` sites. Task 2 drops per-method delegators in favour of `backend()` to eliminate the "missed delegator" risk.
- 🔴 `DbError::NotFound` does not exist; the code uses the existing `DbError::Query(String)`. No new variant.
- 🟠 `prune_below` mirrors the current unbounded SQL query (no 2000-row cap).
- 🟠 Decay uses `get_by_type(t, 1000)` (matching `maintenance.rs:114`), not `get_sorted`.
- 🟠 The `delete`/`prune` Lance-embedding cleanup is an intentional behaviour change (orphan-embedding fix), now documented in Global Constraints rather than claimed as "byte-for-byte equivalent".

---

## Follow-on plans (outlines — write in full once Plan A's trait shape is proven)

**Plan B — SurrealDB backend conformance + cutover.** Implement `MemoryBackend` for `SurrealMemoryStore` (it already has nearly every method; add `get_associations_between`, align `merge` to the trait signature). Add an optional per-agent `surreal: Option<Arc<SurrealMemoryStore>>` handle to the `Db` bundle (decision A = **per-agent instance** under `data_dir/surreal`, matching today's per-agent model and `SurrealMemoryStore::open`). At the two construction sites, select the backend by config/feature. **Delete `surreal_search.rs` and `surreal_maintenance.rs`** (now redundant — `MemorySearch`/`maintenance` are backend-agnostic). Decision B (cross-store atomicity: working memory stays on SQLite, so `MemorySaved` spans two engines) → document as accepted best-effort for the cutover.

**Plan C — SurrealDB quality.** Replace the hand-rolled BFS in `SurrealMemoryStore::get_neighbors` with native recursion (`$root.{..N+collect}->relates->memory` + one `SELECT ... WHERE id IN $ids` hydrate, then one edge query among the ids) — see `/opt/Kodex/docs/references/surrealdb-v3/graph-traversal.md`. Add `snowball(english)` to the FTS analyzer (followup #5). Tune the `EF=(limit*4).max(40)` heuristic (followup #6). Stand up an ONNX-runtime test env, run `tests/surreal_memory.rs` for real, and benchmark HNSW recall/latency at 384-dim on a realistic corpus (followup #7).
