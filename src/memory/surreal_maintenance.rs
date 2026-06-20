//! Memory maintenance for the SurrealDB backend (feature `surreal-memory`).
//!
//! Mirrors `maintenance.rs` (decay, prune, merge) over [`SurrealMemoryStore`].
//! Re-uses [`MaintenanceConfig`] and [`MaintenanceReport`]. The merge is a single
//! store operation (`store.merge`) — the embedding lives on the record, so unlike
//! the SQLite+Lance path it is not a best-effort fix-up after the transaction.
//!
//! Cancellation plumbing (the `watch::Receiver` machinery in `maintenance.rs`) is
//! intentionally omitted here for now; it can be layered on when this path is
//! wired into the running daemon.

use std::collections::HashSet;
use std::sync::Arc;

use crate::error::Result;
use crate::memory::embedding::EmbeddingModel;
use crate::memory::maintenance::{MaintenanceConfig, MaintenanceReport};
use crate::memory::surreal_store::SurrealMemoryStore;
use crate::memory::types::{Memory, MemoryType};

const MAX_MERGES_PER_PASS: usize = 500;
const MAX_SIMILAR_CANDIDATES: usize = 25;
const MAX_MERGED_MEMORY_CONTENT_BYTES: usize = 50_000;
const MAINTENANCE_SCAN_LIMIT: i64 = 2_000;

/// Run decay, prune, and merge over the SurrealDB memory store.
pub async fn run_maintenance(
    store: &SurrealMemoryStore,
    embedding_model: &Arc<EmbeddingModel>,
    config: &MaintenanceConfig,
) -> Result<MaintenanceReport> {
    let decayed = apply_decay(store, config.decay_rate).await?;
    let pruned = prune_memories(store, config).await?;
    let merged =
        merge_similar_memories(store, embedding_model, config.merge_similarity_threshold).await?;
    Ok(MaintenanceReport { decayed, pruned, merged })
}

/// Importance decay based on recency and access patterns (mirrors
/// `maintenance.rs::apply_decay`). Identity memories never decay.
pub async fn apply_decay(store: &SurrealMemoryStore, decay_rate: f32) -> Result<usize> {
    let memories = store.get_all_active(MAINTENANCE_SCAN_LIMIT).await?;
    let now = chrono::Utc::now();
    let mut decayed = 0;

    for mut memory in memories {
        if memory.memory_type == MemoryType::Identity {
            continue;
        }
        let days_old = (now - memory.updated_at).num_days();
        let days_since_access = (now - memory.last_accessed_at).num_days();

        let age_decay = 1.0 - (days_old as f32 * decay_rate).min(0.5);
        let access_boost = if days_since_access < 7 {
            1.1
        } else if days_since_access > 30 {
            0.9
        } else {
            1.0
        };
        let new_importance = memory.importance * age_decay * access_boost;

        if (new_importance - memory.importance).abs() > 0.01 {
            memory.importance = new_importance.clamp(0.0, 1.0);
            memory.updated_at = now;
            store.update(&memory).await?;
            decayed += 1;
        }
    }
    Ok(decayed)
}

/// Delete non-identity memories below the importance threshold that are older
/// than `min_age_days` (mirrors `maintenance.rs::prune_memories`).
pub async fn prune_memories(
    store: &SurrealMemoryStore,
    config: &MaintenanceConfig,
) -> Result<usize> {
    let cutoff = chrono::Utc::now() - chrono::Duration::days(config.min_age_days);
    let memories = store.get_all_active(MAINTENANCE_SCAN_LIMIT).await?;
    let mut pruned = 0;
    for memory in memories {
        if memory.memory_type != MemoryType::Identity
            && memory.importance < config.prune_threshold
            && memory.created_at < cutoff
        {
            store.delete(&memory.id).await?;
            pruned += 1;
        }
    }
    Ok(pruned)
}

/// Merge near-duplicate memories (mirrors `maintenance.rs::merge_similar_memories`).
pub async fn merge_similar_memories(
    store: &SurrealMemoryStore,
    embedding_model: &Arc<EmbeddingModel>,
    similarity_threshold: f32,
) -> Result<usize> {
    let candidates = store.get_all_active(MAINTENANCE_SCAN_LIMIT).await?;
    let mut merged_count = 0_usize;
    let mut merged_ids: HashSet<String> = HashSet::new();

    for source in candidates {
        if merged_count >= MAX_MERGES_PER_PASS {
            break;
        }
        if merged_ids.contains(&source.id) {
            continue;
        }
        // Reload in case an earlier merge changed it.
        let Some(mut active_survivor) = store.load(&source.id).await? else {
            continue;
        };
        if active_survivor.forgotten {
            continue;
        }

        let similar = store
            .find_similar(&active_survivor.id, similarity_threshold, MAX_SIMILAR_CANDIDATES)
            .await?;

        for (candidate_id, _sim) in similar {
            if merged_count >= MAX_MERGES_PER_PASS {
                break;
            }
            if merged_ids.contains(&candidate_id) || candidate_id == active_survivor.id {
                continue;
            }
            let Some(candidate) = store.load(&candidate_id).await? else {
                continue;
            };
            if candidate.forgotten {
                continue;
            }

            let (winner, loser) = choose_merge_pair(&active_survivor, &candidate);
            let content = merged_memory_content(winner.content.clone(), &loser.content);
            let embedding = embedding_model.embed_one(&content).await?;
            store.merge(&winner.id, &loser.id, &content, Some(&embedding)).await?;
            merged_ids.insert(loser.id.clone());
            merged_count += 1;

            // The winner is the survivor going forward.
            active_survivor = store.load(&winner.id).await?.unwrap_or(winner);
        }
    }
    Ok(merged_count)
}

/// Pick (winner, loser): higher importance wins; ties broken by lower id.
/// Mirrors `maintenance.rs::choose_merge_pair`.
fn choose_merge_pair(first: &Memory, second: &Memory) -> (Memory, Memory) {
    let first_wins = first.importance > second.importance
        || (first.importance == second.importance && first.id < second.id);
    if first_wins {
        (first.clone(), second.clone())
    } else {
        (second.clone(), first.clone())
    }
}

/// Combine winner + loser content, dedup-if-contained, cap bytes.
/// Mirrors `maintenance.rs::merged_memory_content`.
fn merged_memory_content(winner: String, loser: &str) -> String {
    let winner_trimmed = winner.trim_end();
    let loser_trimmed = loser.trim_end();

    if loser_trimmed.is_empty() || winner_trimmed.contains(loser_trimmed) {
        return winner_trimmed.to_string();
    }
    let merged = if winner_trimmed.is_empty() {
        loser_trimmed.to_string()
    } else {
        format!("{winner_trimmed}\n\n{loser_trimmed}")
    };
    if merged.len() <= MAX_MERGED_MEMORY_CONTENT_BYTES {
        return merged;
    }
    let boundary = merged.floor_char_boundary(MAX_MERGED_MEMORY_CONTENT_BYTES);
    merged[..boundary].to_string()
}
