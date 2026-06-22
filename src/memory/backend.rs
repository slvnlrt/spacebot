//! Pluggable memory storage backend trait and implementations.

use std::sync::Arc;

use async_trait::async_trait;

use crate::error::Result;
use crate::memory::search::SearchSort;
use crate::memory::types::{Association, Memory, MemoryType};

/// Trait for pluggable memory storage backends.
///
/// A backend encapsulates both structured memory storage (SQLite, SurrealDB,
/// etc.) and vector/FTS search (LanceDB, etc.) behind a single async interface.
/// All provided methods delegate to the underlying stores.
#[async_trait]
pub trait MemoryBackend: Send + Sync + std::fmt::Debug {
    /// The agent ID this backend is scoped to (empty string if none).
    fn agent_id(&self) -> &str;

    // ── CRUD ─────────────────────────────────────────────────────────────────

    /// Persist a new memory and optionally its embedding.
    async fn save(&self, memory: &Memory, embedding: Option<&[f32]>) -> Result<()>;

    /// Store or replace the embedding for an existing memory.
    async fn set_embedding(&self, memory: &Memory, embedding: &[f32]) -> Result<()>;

    /// Hard-delete a memory and its embedding.
    async fn delete(&self, id: &str) -> Result<()>;

    /// Load a memory by ID; returns `None` if not found.
    async fn load(&self, id: &str) -> Result<Option<Memory>>;

    /// Update an existing memory's metadata/content.
    async fn update(&self, memory: &Memory) -> Result<()>;

    /// Soft-delete: mark as forgotten. Returns `true` if the record was modified.
    async fn forget(&self, id: &str) -> Result<bool>;

    /// Record an access, updating `last_accessed_at` and `access_count`.
    async fn record_access(&self, id: &str) -> Result<()>;

    // ── Filtered retrieval ────────────────────────────────────────────────────

    /// Get up to `limit` memories of the given type, ordered by importance.
    async fn get_by_type(&self, t: MemoryType, limit: i64) -> Result<Vec<Memory>>;

    /// Get up to `limit` memories with importance ≥ `threshold`.
    async fn get_high_importance(&self, threshold: f32, limit: i64) -> Result<Vec<Memory>>;

    /// Get up to `limit` memories sorted by `sort`, optionally filtered by type.
    async fn get_sorted(
        &self,
        sort: SearchSort,
        limit: i64,
        t: Option<MemoryType>,
    ) -> Result<Vec<Memory>>;

    // ── Associations ──────────────────────────────────────────────────────────

    /// Create (or upsert) an association between two memories.
    async fn create_association(&self, a: &Association) -> Result<()>;

    /// Get all associations for a memory (incoming + outgoing).
    async fn get_associations(&self, id: &str) -> Result<Vec<Association>>;

    /// Get associations where both endpoints are within `ids`.
    async fn get_associations_between(&self, ids: &[String]) -> Result<Vec<Association>>;

    /// Delete all associations referencing `id`. Returns the number deleted.
    async fn delete_associations_for_memory(&self, id: &str) -> Result<u64>;

    /// Traverse the graph up to `depth` hops from `id`, excluding `exclude`.
    ///
    /// Returns `(neighbor_memories, edges)`.
    async fn get_neighbors(
        &self,
        id: &str,
        depth: u32,
        exclude: &[String],
    ) -> Result<(Vec<Memory>, Vec<Association>)>;

    // ── Search ────────────────────────────────────────────────────────────────

    /// Approximate nearest-neighbour search. Returns `(memory_id, distance)`.
    async fn vector_search(&self, q: &[f32], limit: usize) -> Result<Vec<(String, f32)>>;

    /// Full-text search. Returns `(memory_id, score)`.
    async fn text_search(&self, q: &str, limit: usize) -> Result<Vec<(String, f32)>>;

    /// Find memories similar to the one at `id`. Returns `(memory_id, similarity)`.
    async fn find_similar(
        &self,
        id: &str,
        threshold: f32,
        limit: usize,
    ) -> Result<Vec<(String, f32)>>;

    // ── Maintenance ───────────────────────────────────────────────────────────

    /// Delete all non-identity memories with importance below `threshold` that
    /// were created before `older_than`. Returns the number of memories deleted.
    async fn prune_below(
        &self,
        threshold: f32,
        older_than: chrono::DateTime<chrono::Utc>,
    ) -> Result<u64>;

    /// Merge `loser_id` into `survivor_id`, updating the survivor's content and
    /// optionally its embedding. The loser is soft-deleted (forgotten) and its
    /// associations are rewired to the survivor.
    async fn merge(
        &self,
        survivor_id: &str,
        loser_id: &str,
        new_content: &str,
        new_embedding: Option<&[f32]>,
    ) -> Result<()>;
}

// ── SqliteBackend ─────────────────────────────────────────────────────────────

/// `MemoryBackend` implementation backed by SQLite (via `MemoryStore`) and
/// LanceDB (via `EmbeddingTable`). This is the production default.
#[derive(Clone)]
pub struct SqliteBackend {
    store: Arc<crate::memory::store::MemoryStore>,
    embeddings: crate::memory::lance::EmbeddingTable,
}

impl std::fmt::Debug for SqliteBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SqliteBackend")
            .field("store", &self.store)
            .field("embeddings", &"<EmbeddingTable>")
            .finish()
    }
}

impl SqliteBackend {
    /// Create a new `SqliteBackend` wrapping the given store and embedding table.
    pub fn new(
        store: Arc<crate::memory::store::MemoryStore>,
        embeddings: crate::memory::lance::EmbeddingTable,
    ) -> Self {
        Self { store, embeddings }
    }
}

#[async_trait]
impl MemoryBackend for SqliteBackend {
    fn agent_id(&self) -> &str {
        self.store.agent_id()
    }

    async fn save(&self, memory: &Memory, embedding: Option<&[f32]>) -> Result<()> {
        self.store.save(memory).await?;
        if let Some(emb) = embedding {
            self.embeddings
                .store(&memory.id, &memory.content, emb)
                .await?;
        }
        Ok(())
    }

    async fn set_embedding(&self, memory: &Memory, embedding: &[f32]) -> Result<()> {
        // EmbeddingTable::store is an append, not an upsert — delete any
        // existing vector first so "set" replaces rather than duplicates.
        self.embeddings.delete(&memory.id).await?;
        self.embeddings
            .store(&memory.id, &memory.content, embedding)
            .await
    }

    async fn delete(&self, id: &str) -> Result<()> {
        self.store.delete(id).await?; // FK ON DELETE CASCADE drops edges
        self.embeddings.delete(id).await?; // Lance row has no FK — delete explicitly
        Ok(())
    }

    async fn load(&self, id: &str) -> Result<Option<Memory>> {
        self.store.load(id).await
    }

    async fn update(&self, m: &Memory) -> Result<()> {
        self.store.update(m).await
    }

    async fn forget(&self, id: &str) -> Result<bool> {
        self.store.forget(id).await
    }

    async fn record_access(&self, id: &str) -> Result<()> {
        self.store.record_access(id).await
    }

    async fn get_by_type(&self, t: MemoryType, limit: i64) -> Result<Vec<Memory>> {
        self.store.get_by_type(t, limit).await
    }

    async fn get_high_importance(&self, th: f32, limit: i64) -> Result<Vec<Memory>> {
        self.store.get_high_importance(th, limit).await
    }

    async fn get_sorted(
        &self,
        sort: SearchSort,
        limit: i64,
        t: Option<MemoryType>,
    ) -> Result<Vec<Memory>> {
        self.store.get_sorted(sort, limit, t).await
    }

    async fn create_association(&self, a: &Association) -> Result<()> {
        self.store.create_association(a).await
    }

    async fn get_associations(&self, id: &str) -> Result<Vec<Association>> {
        self.store.get_associations(id).await
    }

    async fn get_associations_between(&self, ids: &[String]) -> Result<Vec<Association>> {
        self.store.get_associations_between(ids).await
    }

    async fn delete_associations_for_memory(&self, id: &str) -> Result<u64> {
        self.store.delete_associations_for_memory(id).await
    }

    async fn get_neighbors(
        &self,
        id: &str,
        depth: u32,
        exclude: &[String],
    ) -> Result<(Vec<Memory>, Vec<Association>)> {
        self.store.get_neighbors(id, depth, exclude).await
    }

    async fn vector_search(&self, q: &[f32], limit: usize) -> Result<Vec<(String, f32)>> {
        self.embeddings.vector_search(q, limit).await
    }

    async fn text_search(&self, q: &str, limit: usize) -> Result<Vec<(String, f32)>> {
        self.embeddings.text_search(q, limit).await
    }

    async fn find_similar(
        &self,
        id: &str,
        th: f32,
        limit: usize,
    ) -> Result<Vec<(String, f32)>> {
        self.embeddings.find_similar(id, th, limit).await
    }

    async fn prune_below(
        &self,
        threshold: f32,
        older_than: chrono::DateTime<chrono::Utc>,
    ) -> Result<u64> {
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

        let mut n = 0u64;
        for row in rows {
            let id: String = row
                .try_get("id")
                .map_err(|e| crate::error::DbError::Query(e.to_string()))?;
            self.delete(&id).await?;
            n += 1;
        }
        Ok(n)
    }

    async fn merge(
        &self,
        survivor_id: &str,
        loser_id: &str,
        new_content: &str,
        new_embedding: Option<&[f32]>,
    ) -> Result<()> {
        // merge_memories_atomic(updated_survivor, loser) internally forgets the
        // loser and rewires its associations onto the survivor — DO NOT set
        // forgotten here. We only build the updated survivor.
        let mut survivor = self
            .store
            .load(survivor_id)
            .await?
            .ok_or_else(|| {
                crate::error::DbError::Query(format!(
                    "merge survivor {survivor_id} not found"
                ))
            })?;
        let loser = self
            .store
            .load(loser_id)
            .await?
            .ok_or_else(|| {
                crate::error::DbError::Query(format!(
                    "merge loser {loser_id} not found"
                ))
            })?;

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

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::lance::EmbeddingTable;
    use crate::memory::store::MemoryStore;
    use crate::memory::types::{Memory, MemoryType};

    const DIM: usize = 384;

    // Returns the backend AND the TempDir guard — keep the guard alive for the
    // test's duration (dropping it deletes the Lance directory). Mirrors the
    // construction in src/memory/search.rs:577 and maintenance.rs:531.
    async fn sqlite_backend() -> (SqliteBackend, tempfile::TempDir) {
        let store = MemoryStore::connect_in_memory().await;
        let dir = tempfile::tempdir().unwrap();
        let conn = lancedb::connect(dir.path().to_str().unwrap())
            .execute()
            .await
            .unwrap();
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

    #[tokio::test]
    async fn merge_moves_content_and_forgets_loser() {
        let (be, _dir) = sqlite_backend().await;
        let mut a = Memory::new("cats are mammals", MemoryType::Fact);
        a.importance = 0.9;
        let b = Memory::new("cats are animals", MemoryType::Fact);
        be.save(&a, Some(&vec![0.1; DIM])).await.unwrap();
        be.save(&b, Some(&vec![0.2; DIM])).await.unwrap();

        be.merge(
            &a.id,
            &b.id,
            "cats are mammals\n\ncats are animals",
            Some(&vec![0.15; DIM]),
        )
        .await
        .unwrap();

        let survivor = be.load(&a.id).await.unwrap().unwrap();
        assert!(survivor.content.contains("mammals") && survivor.content.contains("animals"));
        // merge_memories_atomic forgets the loser internally — the caller never
        // sets it. Verify the function did so.
        let loser = be.load(&b.id).await.unwrap().unwrap();
        assert!(loser.forgotten);
        // The loser's embedding must be gone (no stale vector for it).
        assert!(be
            .vector_search(&vec![0.2; DIM], 5)
            .await
            .unwrap()
            .iter()
            .all(|(id, _)| id != &b.id));
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

    #[tokio::test]
    async fn set_embedding_replaces_does_not_duplicate() {
        let (be, _dir) = sqlite_backend().await;
        let m = Memory::new("the sky is blue", MemoryType::Fact);
        be.save(&m, Some(&vec![0.1_f32; DIM])).await.unwrap();

        // Re-embed the same memory; store is an append, so set_embedding must
        // delete the old vector first or KNN would surface two rows for one id.
        be.set_embedding(&m, &vec![0.9_f32; DIM]).await.unwrap();

        let hits = be.vector_search(&vec![0.9_f32; DIM], 10).await.unwrap();
        let occurrences = hits.iter().filter(|(id, _)| id == &m.id).count();
        assert_eq!(occurrences, 1, "set_embedding must replace, not duplicate");
    }
}
