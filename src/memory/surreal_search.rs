//! Hybrid memory search over the SurrealDB store (feature `surreal-memory`).
//!
//! Mirrors `search.rs` but operates on [`SurrealMemoryStore`]. Same strategy:
//! Recent/Important/Typed are metadata queries; Hybrid fuses vector + full-text +
//! graph traversal with Reciprocal Rank Fusion. The query embedding is computed
//! by the caller (via `EmbeddingModel`) and passed in, keeping fastembed
//! decoupled from storage — identical to how the reference port is structured.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use crate::error::Result;
use crate::memory::search::{SearchConfig, SearchMode, SearchSort};
use crate::memory::surreal_store::SurrealMemoryStore;
use crate::memory::types::{Memory, MemorySearchResult, RelationType};

/// Search dependencies for the SurrealDB backend.
#[derive(Clone)]
pub struct SurrealMemorySearch {
    store: Arc<SurrealMemoryStore>,
}

impl SurrealMemorySearch {
    pub fn new(store: Arc<SurrealMemoryStore>) -> Self {
        Self { store }
    }

    pub fn store(&self) -> &SurrealMemoryStore {
        &self.store
    }

    /// Unified entry point. `query_embedding` is only used in Hybrid mode and may
    /// be empty otherwise.
    pub async fn search(
        &self,
        query: &str,
        query_embedding: &[f32],
        config: &SearchConfig,
    ) -> Result<Vec<MemorySearchResult>> {
        match config.mode {
            SearchMode::Hybrid => self.hybrid_search(query, query_embedding, config).await,
            SearchMode::Recent => self.metadata_search(SearchSort::Recent, config).await,
            SearchMode::Important => self.metadata_search(SearchSort::Importance, config).await,
            SearchMode::Typed => self.metadata_search(config.sort_by, config).await,
        }
    }

    /// Metadata-only search (no vector/FTS/RRF): Recent, Important, Typed.
    async fn metadata_search(
        &self,
        sort: SearchSort,
        config: &SearchConfig,
    ) -> Result<Vec<MemorySearchResult>> {
        let memories = self
            .store
            .get_sorted(sort, config.max_results as i64, config.memory_type)
            .await?;
        let total = memories.len();
        let results = memories
            .into_iter()
            .enumerate()
            .map(|(rank, memory)| {
                let score = if total > 1 {
                    1.0 - (rank as f32 / total as f32)
                } else {
                    1.0
                };
                MemorySearchResult {
                    memory,
                    score,
                    rank: rank + 1,
                }
            })
            .collect();
        Ok(results)
    }

    /// Hybrid: vector + full-text + graph traversal fused via RRF.
    pub async fn hybrid_search(
        &self,
        query: &str,
        query_embedding: &[f32],
        config: &SearchConfig,
    ) -> Result<Vec<MemorySearchResult>> {
        let n = config.max_results_per_source;
        let mut vector_results = Vec::new();
        let mut fts_results = Vec::new();
        let mut graph_results = Vec::new();

        // 1. Full-text (may be empty before any content is indexed).
        match self.store.text_search(query, n).await {
            Ok(hits) => {
                for (id, score) in hits {
                    if let Some(memory) = self.store.load(&id).await?
                        && !memory.forgotten
                    {
                        fts_results.push(Scored { memory, score: score as f64 });
                    }
                }
            }
            Err(error) => {
                tracing::debug!(%error, "FTS unavailable, falling back to vector + graph");
            }
        }

        // 2. Vector similarity.
        match self.store.vector_search(query_embedding, n).await {
            Ok(hits) => {
                for (id, distance) in hits {
                    if let Some(memory) = self.store.load(&id).await?
                        && !memory.forgotten
                    {
                        vector_results.push(Scored { memory, score: (1.0 - distance) as f64 });
                    }
                }
            }
            Err(error) => {
                tracing::debug!(%error, "vector search unavailable, falling back to graph only");
            }
        }

        // 3. Graph traversal from high-importance, keyword-matching seeds.
        let seeds = self.store.get_high_importance(0.8, 20).await?;
        let q_lower = query.to_lowercase();
        for seed in seeds {
            let matches = q_lower
                .split_whitespace()
                .any(|term| seed.content.to_lowercase().contains(term));
            if matches {
                graph_results.push(Scored { memory: seed.clone(), score: seed.importance as f64 });
                self.traverse_graph(&seed.id, config.max_graph_depth, &mut graph_results)
                    .await?;
            }
        }

        // 4. Reciprocal Rank Fusion.
        let fused =
            reciprocal_rank_fusion(&vector_results, &fts_results, &graph_results, config.rrf_k);
        let results: Vec<MemorySearchResult> = fused
            .into_iter()
            .filter(|s| config.memory_type.is_none_or(|t| s.memory.memory_type == t))
            .enumerate()
            .map(|(rank, s)| MemorySearchResult {
                memory: s.memory,
                score: s.score as f32,
                rank: rank + 1,
            })
            .filter(|r| r.score >= config.min_score)
            .take(config.max_results_per_source)
            .collect();
        Ok(results)
    }

    /// Iterative BFS mirroring `search.rs::traverse_graph`: visits depths
    /// `0..=max_depth`, scores every first-seen neighbour by relation-type
    /// multiplier × weight × importance, re-queues only expanding relations,
    /// and skips forgotten memories.
    async fn traverse_graph(
        &self,
        start_id: &str,
        max_depth: usize,
        results: &mut Vec<Scored>,
    ) -> Result<()> {
        let mut queue: VecDeque<(String, usize)> = VecDeque::new();
        let mut visited: HashSet<String> = HashSet::new();
        queue.push_back((start_id.to_string(), 0));
        visited.insert(start_id.to_string());

        while let Some((current_id, depth)) = queue.pop_front() {
            if depth > max_depth {
                continue;
            }
            for assoc in self.store.get_associations(&current_id).await? {
                let related_id = if assoc.source_id == current_id {
                    assoc.target_id.clone()
                } else {
                    assoc.source_id.clone()
                };
                if !visited.insert(related_id.clone()) {
                    continue;
                }
                if let Some(memory) = self.store.load(&related_id).await?
                    && !memory.forgotten
                {
                    let type_multiplier = match assoc.relation_type {
                        RelationType::Updates => 1.5,
                        RelationType::CausedBy | RelationType::ResultOf => 1.3,
                        RelationType::RelatedTo => 1.0,
                        RelationType::PartOf => 0.8,
                        RelationType::Contradicts => 0.5,
                    };
                    let score = memory.importance as f64 * assoc.weight as f64 * type_multiplier;
                    results.push(Scored { memory, score });
                    if matches!(
                        assoc.relation_type,
                        RelationType::RelatedTo | RelationType::PartOf
                    ) {
                        queue.push_back((related_id, depth + 1));
                    }
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
struct Scored {
    memory: Memory,
    score: f64,
}

/// RRF over the three sources (identical to `search.rs`).
fn reciprocal_rank_fusion(
    vector_results: &[Scored],
    fts_results: &[Scored],
    graph_results: &[Scored],
    k: f64,
) -> Vec<Scored> {
    let mut rrf_scores: HashMap<String, (f64, Memory)> = HashMap::new();
    for list in [vector_results, fts_results, graph_results] {
        for (rank, scored) in list.iter().enumerate() {
            let rrf_score = 1.0 / (k + (rank as f64 + 1.0));
            let entry = rrf_scores
                .entry(scored.memory.id.clone())
                .or_insert((0.0, scored.memory.clone()));
            entry.0 += rrf_score;
        }
    }
    let mut fused: Vec<Scored> = rrf_scores
        .into_iter()
        .map(|(_, (score, memory))| Scored { memory, score })
        .collect();
    fused.sort_by(|a, b| b.score.total_cmp(&a.score));
    fused
}
