//! One-time migration from the SQLite + LanceDB memory store into the embedded
//! SurrealDB store (feature `surreal-memory`).
//!
//! Reads memories and associations from the existing [`MemoryStore`] (SQLite) and
//! writes them into a [`SurrealMemoryStore`], regenerating embeddings from
//! content via [`EmbeddingModel`]. Regenerating (rather than copying vectors out
//! of LanceDB) keeps the migration independent of Lance internals and sidesteps
//! the empty-table-recovery gap noted in the design doc; it is the simplest
//! correct first cut. If a LanceDB embedding getter is later exposed, this can be
//! optimised to copy existing vectors and only re-embed the missing ones.
//!
//! Idempotency: writing a memory uses the same UUID id, so re-running upserts the
//! same records. Associations use a UNIQUE edge index, so duplicates collapse.
//!
//! Scope: active (non-forgotten) memories, enumerated per `MemoryType`. Soft
//! deleted memories are not carried over.

use std::collections::HashSet;
use std::sync::Arc;

use crate::error::Result;
use crate::memory::embedding::EmbeddingModel;
use crate::memory::store::MemoryStore;
use crate::memory::surreal_store::SurrealMemoryStore;
use crate::memory::types::MemoryType;

/// Per-type enumeration cap. Large enough for a full per-agent store.
const MIGRATION_SCAN_LIMIT: i64 = 100_000;

#[derive(Debug, Default, Clone, Copy)]
pub struct MigrationReport {
    pub memories: usize,
    pub associations: usize,
}

/// Migrate everything reachable from `source` into `target`.
pub async fn migrate_from_sqlite(
    source: &MemoryStore,
    embedding_model: &Arc<EmbeddingModel>,
    target: &SurrealMemoryStore,
) -> Result<MigrationReport> {
    let mut report = MigrationReport::default();
    let mut seen_edges: HashSet<(String, String, String)> = HashSet::new();
    let mut memory_ids: Vec<String> = Vec::new();

    // 1. Memories, enumerated per type (the SQLite store has no "all" accessor).
    for &mem_type in MemoryType::ALL {
        let memories = source.get_by_type(mem_type, MIGRATION_SCAN_LIMIT).await?;
        for memory in memories {
            let embedding = embedding_model.embed_one(&memory.content).await?;
            target.save(&memory, Some(&embedding)).await?;
            memory_ids.push(memory.id.clone());
            report.memories += 1;
        }
    }

    // 2. Associations, collected per memory and de-duplicated.
    for id in &memory_ids {
        for assoc in source.get_associations(id).await? {
            let key = (
                assoc.source_id.clone(),
                assoc.target_id.clone(),
                assoc.relation_type.to_string(),
            );
            if !seen_edges.insert(key) {
                continue;
            }
            target.create_association(&assoc).await?;
            report.associations += 1;
        }
    }

    Ok(report)
}
