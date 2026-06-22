//! Memory search: hybrid (vector + FTS + RRF + graph), temporal, importance, and typed queries.

use crate::error::Result;
use crate::memory::EmbeddingModel;
use crate::memory::backend::MemoryBackend;
use crate::memory::types::{Memory, MemorySearchResult, MemoryType, RelationType};

use std::collections::HashMap;
use std::sync::Arc;

/// Which search strategy to use.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SearchMode {
    /// Full hybrid: vector + FTS + graph + RRF. Requires a query string.
    #[default]
    Hybrid,
    /// Most recent memories by creation time. No query needed.
    Recent,
    /// Highest importance memories. No query needed.
    Important,
    /// Filter by MemoryType with configurable sort. Requires `memory_type`.
    Typed,
}

/// Sort order for non-hybrid search modes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SearchSort {
    /// Most recent first (created_at DESC).
    #[default]
    Recent,
    /// Highest importance first (importance DESC).
    Importance,
    /// Most accessed first (access_count DESC).
    MostAccessed,
}

/// Bundles all memory search dependencies.
pub struct MemorySearch {
    backend: Arc<dyn MemoryBackend>,
    embedding_model: Arc<EmbeddingModel>,
}

impl Clone for MemorySearch {
    fn clone(&self) -> Self {
        Self {
            backend: Arc::clone(&self.backend),
            embedding_model: Arc::clone(&self.embedding_model),
        }
    }
}

impl std::fmt::Debug for MemorySearch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemorySearch")
            .field("backend", &self.backend)
            .finish_non_exhaustive()
    }
}

impl MemorySearch {
    /// Create a new MemorySearch instance.
    pub fn new(backend: Arc<dyn MemoryBackend>, embedding_model: Arc<EmbeddingModel>) -> Self {
        Self {
            backend,
            embedding_model,
        }
    }

    /// Get a reference to the backend.
    pub fn backend(&self) -> &Arc<dyn MemoryBackend> {
        &self.backend
    }

    /// Get a shared handle to the embedding model (for async embed_one).
    pub fn embedding_model_arc(&self) -> &Arc<EmbeddingModel> {
        &self.embedding_model
    }

    /// Get the agent ID this search instance is scoped to.
    pub fn agent_id(&self) -> &str {
        self.backend.agent_id()
    }

    /// Unified search entry point. Dispatches to the appropriate strategy
    /// based on `config.mode`.
    pub async fn search(
        &self,
        query: &str,
        config: &SearchConfig,
    ) -> Result<Vec<MemorySearchResult>> {
        let results = match config.mode {
            SearchMode::Hybrid => self.hybrid_search(query, config).await,
            SearchMode::Recent => self.metadata_search(SearchSort::Recent, config).await,
            SearchMode::Important => self.metadata_search(SearchSort::Importance, config).await,
            SearchMode::Typed => self.metadata_search(config.sort_by, config).await,
        }?;

        #[cfg(feature = "metrics")]
        {
            let agent_id = self.backend.agent_id();
            let agent_label = if agent_id.is_empty() {
                "unknown"
            } else {
                agent_id
            };
            crate::telemetry::Metrics::global()
                .memory_search_results
                .with_label_values(&[agent_label])
                .observe(results.len() as f64);
        }

        Ok(results)
    }

    /// Metadata-based search: queries SQLite directly, no vector/FTS/RRF.
    /// Used by Recent, Important, and Typed modes.
    async fn metadata_search(
        &self,
        sort: SearchSort,
        config: &SearchConfig,
    ) -> Result<Vec<MemorySearchResult>> {
        let memories = self
            .backend
            .get_sorted(sort, config.max_results as i64, config.memory_type)
            .await?;

        let total = memories.len();
        let results = memories
            .into_iter()
            .enumerate()
            .map(|(rank, memory)| {
                // Normalized positional score so output type is consistent.
                // First result gets 1.0, decays linearly.
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

    /// Perform hybrid search across all memory sources.
    pub async fn hybrid_search(
        &self,
        query: &str,
        config: &SearchConfig,
    ) -> Result<Vec<MemorySearchResult>> {
        // Collect results from different sources
        let mut vector_results = Vec::new();
        let mut fts_results = Vec::new();
        let mut graph_results = Vec::new();

        // 1. Full-text search via LanceDB
        // FTS requires an inverted index. If the index doesn't exist yet (empty
        // table, first run) this will fail — fall back to vector + graph search.
        match self
            .backend
            .text_search(query, config.max_results_per_source)
            .await
        {
            Ok(fts_matches) => {
                for (memory_id, score) in fts_matches {
                    if let Some(memory) = self.backend.load(&memory_id).await?
                        && !memory.forgotten
                    {
                        fts_results.push(ScoredMemory {
                            memory,
                            score: score as f64,
                        });
                    }
                }
            }
            Err(error) => {
                tracing::debug!(%error, "FTS search unavailable, falling back to vector + graph");
            }
        }

        // 2. Vector similarity search via LanceDB
        let query_embedding = self.embedding_model.embed_one(query).await?;
        match self
            .backend
            .vector_search(&query_embedding, config.max_results_per_source)
            .await
        {
            Ok(vector_matches) => {
                for (memory_id, distance) in vector_matches {
                    let similarity = 1.0 - distance;
                    if let Some(memory) = self.backend.load(&memory_id).await?
                        && !memory.forgotten
                    {
                        vector_results.push(ScoredMemory {
                            memory,
                            score: similarity as f64,
                        });
                    }
                }
            }
            Err(error) => {
                tracing::debug!(%error, "vector search unavailable, falling back to graph only");
            }
        }

        // 3. Graph traversal from high-importance memories
        // Get identity and high-importance memories as starting points
        let seed_memories = self.backend.get_high_importance(0.8, 20).await?;

        for seed in seed_memories {
            // Check if seed is semantically related to query via simple keyword matching
            if query
                .to_lowercase()
                .split_whitespace()
                .any(|term| seed.content.to_lowercase().contains(term))
            {
                graph_results.push(ScoredMemory {
                    memory: seed.clone(),
                    score: seed.importance as f64,
                });

                // Traverse graph to find related memories
                self.traverse_graph(&seed.id, config.max_graph_depth, &mut graph_results)
                    .await?;
            }
        }

        // 4. Merge results using Reciprocal Rank Fusion (RRF)
        let fused_results =
            reciprocal_rank_fusion(&vector_results, &fts_results, &graph_results, config.rrf_k);

        // Convert to MemorySearchResult with ranks, applying optional type filter
        let results: Vec<MemorySearchResult> = fused_results
            .into_iter()
            .filter(|scored| {
                config
                    .memory_type
                    .is_none_or(|t| scored.memory.memory_type == t)
            })
            .enumerate()
            .map(|(rank, scored)| MemorySearchResult {
                memory: scored.memory,
                score: scored.score as f32,
                rank: rank + 1,
            })
            .filter(|r| r.score >= config.min_score)
            .take(config.max_results_per_source)
            .collect();

        Ok(results)
    }

    /// Traverse the memory graph to find related memories (level-batched BFS).
    ///
    /// Uses two queries per BFS level — `get_associations_for` (all edges incident
    /// to the current frontier) and `load_many` (batch-load all new neighbours) —
    /// instead of one query per node + one per neighbour (was O(nodes) N+1, now
    /// O(depth)×2).
    ///
    /// **Ordering / determinism note.**
    /// The single collection pass below iterates `frontier` in its current order
    /// and updates `visited` inline, so the first frontier node that reaches a
    /// given neighbour wins (cross-frontier-node first-seen is deterministic).
    /// Intra-node edge order (multiple edges from the *same* frontier node) was
    /// already order-incidental in the original (`get_associations` has no ORDER BY),
    /// so that sub-case remains best-effort in both old and new implementations.
    async fn traverse_graph(
        &self,
        start_id: &str,
        max_depth: usize,
        results: &mut Vec<ScoredMemory>,
    ) -> Result<()> {
        let mut visited: std::collections::HashSet<String> = std::collections::HashSet::new();
        visited.insert(start_id.to_string());

        let mut frontier: Vec<String> = vec![start_id.to_string()];
        let mut depth = 0usize;

        while !frontier.is_empty() && depth <= max_depth {
            // One query for all edges incident to this level's frontier.
            let all_edges = self.backend.get_associations_for(&frontier).await?;

            // Group edges by the frontier node they are incident to.
            // If an edge connects two frontier nodes, assign it to the one that
            // appears first in `frontier` — the inline `visited` update in the
            // pass below ensures the correct first-seen winner regardless.
            let mut by_node: std::collections::HashMap<
                &str,
                Vec<&crate::memory::types::Association>,
            > = std::collections::HashMap::new();
            for edge in &all_edges {
                let source_in = frontier.iter().any(|f| f == &edge.source_id);
                let target_in = frontier.iter().any(|f| f == &edge.target_id);
                if source_in {
                    by_node.entry(&edge.source_id).or_default().push(edge);
                } else if target_in {
                    by_node.entry(&edge.target_id).or_default().push(edge);
                }
            }

            // SINGLE collection pass: consult AND update `visited` inline.
            // Iterating `frontier` in order ensures the first frontier node
            // that reaches a neighbour claims it (deterministic cross-node case).
            let mut new: Vec<(String, RelationType, f32)> = Vec::new();
            for fnode in &frontier {
                if let Some(edges) = by_node.get(fnode.as_str()) {
                    for edge in edges.iter() {
                        // The neighbour is the endpoint that is NOT this frontier node.
                        let neighbor_id = if edge.source_id == *fnode {
                            &edge.target_id
                        } else {
                            &edge.source_id
                        };
                        if visited.contains(neighbor_id) {
                            continue;
                        }
                        // Mark visited BEFORE load_many — mirrors the original
                        // `visited.insert` before `store.load` in the old code.
                        // Forgotten/missing neighbours are still marked visited
                        // and never reconsidered even if load_many omits them.
                        visited.insert(neighbor_id.clone());
                        new.push((neighbor_id.clone(), edge.relation_type, edge.weight));
                    }
                }
            }

            if new.is_empty() {
                break;
            }

            // One query to batch-load all new neighbours.
            // load_many returns forgotten rows — the `forgotten` check is done
            // in Rust below, AFTER marking visited (exact parity with original).
            let new_ids: Vec<String> = new.iter().map(|(id, _, _)| id.clone()).collect();
            let loaded: std::collections::HashMap<String, crate::memory::types::Memory> = self
                .backend
                .load_many(&new_ids)
                .await?
                .into_iter()
                .map(|m| (m.id.clone(), m))
                .collect();

            let mut next_frontier: Vec<String> = Vec::new();

            for (nid, rel, weight) in new {
                if let Some(memory) = loaded.get(&nid) {
                    if memory.forgotten {
                        // Visited already inserted; skip scoring and expansion.
                        continue;
                    }
                    let type_multiplier = match rel {
                        RelationType::Updates => 1.5,
                        RelationType::CausedBy | RelationType::ResultOf => 1.3,
                        RelationType::RelatedTo => 1.0,
                        RelationType::Contradicts => 0.5,
                        RelationType::PartOf => 0.8,
                    };
                    let score = memory.importance as f64 * weight as f64 * type_multiplier;
                    results.push(ScoredMemory {
                        memory: memory.clone(),
                        score,
                    });
                    // Re-expand only RelatedTo / PartOf (same rule as original).
                    if matches!(rel, RelationType::RelatedTo | RelationType::PartOf) {
                        next_frontier.push(nid);
                    }
                }
                // Missing from load_many (not in DB): visited already inserted,
                // score silently skipped — same as the original `load` returning None.
            }

            frontier = next_frontier;
            depth += 1;
        }

        Ok(())
    }
}

/// Search configuration for all modes.
#[derive(Debug, Clone)]
pub struct SearchConfig {
    /// Which search strategy to use.
    pub mode: SearchMode,
    /// Optional memory type filter. Required for `Typed` mode, optional for others.
    pub memory_type: Option<MemoryType>,
    /// Sort order for non-hybrid modes.
    pub sort_by: SearchSort,
    /// Maximum number of results to return.
    pub max_results: usize,
    /// Maximum number of results from each source (vector, fts, graph) in hybrid mode.
    pub max_results_per_source: usize,
    /// RRF k parameter (typically 60). Only used in hybrid mode.
    pub rrf_k: f64,
    /// Minimum score threshold for results. Only used in hybrid mode.
    pub min_score: f32,
    /// Maximum graph traversal depth. Only used in hybrid mode.
    pub max_graph_depth: usize,
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            mode: SearchMode::Hybrid,
            memory_type: None,
            sort_by: SearchSort::Recent,
            max_results: 10,
            max_results_per_source: 50,
            rrf_k: 60.0,
            // RRF scores are 1/(k+rank), so with k=60 the max single-source
            // score is ~0.016. Set threshold low enough to not discard everything.
            min_score: 0.0,
            max_graph_depth: 2,
        }
    }
}

/// Simple scored memory for internal use.
#[derive(Debug, Clone)]
struct ScoredMemory {
    memory: Memory,
    score: f64,
}

/// Reciprocal Rank Fusion to combine results from multiple sources.
/// RRF score = sum(1 / (k + rank)) for each list where the item appears.
fn reciprocal_rank_fusion(
    vector_results: &[ScoredMemory],
    fts_results: &[ScoredMemory],
    graph_results: &[ScoredMemory],
    k: f64,
) -> Vec<ScoredMemory> {
    // Build a map of memory ID to RRF score
    let mut rrf_scores: HashMap<String, (f64, Memory)> = HashMap::new();

    // Add vector results
    for (rank, scored) in vector_results.iter().enumerate() {
        let rrf_score = 1.0 / (k + (rank as f64 + 1.0));
        let entry = rrf_scores
            .entry(scored.memory.id.clone())
            .or_insert((0.0, scored.memory.clone()));
        entry.0 += rrf_score;
    }

    // Add FTS results
    for (rank, scored) in fts_results.iter().enumerate() {
        let rrf_score = 1.0 / (k + (rank as f64 + 1.0));
        let entry = rrf_scores
            .entry(scored.memory.id.clone())
            .or_insert((0.0, scored.memory.clone()));
        entry.0 += rrf_score;
    }

    // Add graph results
    for (rank, scored) in graph_results.iter().enumerate() {
        let rrf_score = 1.0 / (k + (rank as f64 + 1.0));
        let entry = rrf_scores
            .entry(scored.memory.id.clone())
            .or_insert((0.0, scored.memory.clone()));
        entry.0 += rrf_score;
    }

    // Convert to vec and sort by RRF score
    let mut fused: Vec<ScoredMemory> = rrf_scores
        .into_iter()
        .map(|(_, (score, memory))| ScoredMemory { memory, score })
        .collect();

    fused.sort_by(|a, b| b.score.total_cmp(&a.score));

    fused
}

/// Curate search results to return only the most relevant.
pub fn curate_results(
    results: &[MemorySearchResult],
    max_results: usize,
) -> Vec<&MemorySearchResult> {
    results.iter().take(max_results).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::backend::SqliteBackend;
    use crate::memory::lance::EmbeddingTable;
    use crate::memory::types::MemoryType;
    use chrono::{Duration, Utc};

    fn make_scored(id: &str, score: f64) -> ScoredMemory {
        let mut memory = Memory::new(format!("content for {id}"), MemoryType::Fact);
        memory.id = id.to_string();
        ScoredMemory { memory, score }
    }

    #[test]
    fn test_rrf_single_list() {
        let vector = vec![make_scored("a", 0.9), make_scored("b", 0.7)];
        let fused = reciprocal_rank_fusion(&vector, &[], &[], 60.0);

        assert_eq!(fused.len(), 2);
        assert_eq!(fused[0].memory.id, "a");
        assert_eq!(fused[1].memory.id, "b");
        // First item: 1/(60+1) ≈ 0.01639
        assert!((fused[0].score - 1.0 / 61.0).abs() < 1e-10);
    }

    #[test]
    fn test_rrf_deduplication() {
        // Same memory appears in vector and FTS — scores should sum
        let vector = vec![make_scored("a", 0.9)];
        let fts = vec![make_scored("a", 5.0)];

        let fused = reciprocal_rank_fusion(&vector, &fts, &[], 60.0);
        assert_eq!(fused.len(), 1);
        // Should be 2 * 1/(60+1)
        let expected = 2.0 / 61.0;
        assert!((fused[0].score - expected).abs() < 1e-10);
    }

    #[test]
    fn test_rrf_multi_list_ranking() {
        // "a" appears in all three lists, "b" only in vector
        let vector = vec![make_scored("a", 0.9), make_scored("b", 0.5)];
        let fts = vec![make_scored("a", 5.0)];
        let graph = vec![make_scored("a", 0.8)];

        let fused = reciprocal_rank_fusion(&vector, &fts, &graph, 60.0);
        assert_eq!(fused[0].memory.id, "a");
        assert!(fused[0].score > fused[1].score);
    }

    #[test]
    fn test_rrf_empty_lists() {
        let fused = reciprocal_rank_fusion(&[], &[], &[], 60.0);
        assert!(fused.is_empty());
    }

    #[test]
    fn test_curate_results_respects_limit() {
        let results: Vec<MemorySearchResult> = (0..10)
            .map(|i| MemorySearchResult {
                memory: Memory::new(format!("mem {i}"), MemoryType::Fact),
                score: 1.0 - (i as f32 * 0.1),
                rank: i + 1,
            })
            .collect();

        let curated = curate_results(&results, 3);
        assert_eq!(curated.len(), 3);
        assert_eq!(curated[0].rank, 1);
    }

    #[test]
    fn test_curate_results_handles_empty() {
        let curated = curate_results(&[], 5);
        assert!(curated.is_empty());
    }

    // Non-hybrid modes only need SQLite, no LanceDB/embeddings.
    // We construct a MemorySearch with dummy LanceDB/embedding fields
    // and only exercise code paths that don't touch them.

    async fn setup_search_with_memories() -> (Arc<crate::memory::MemoryStore>, Vec<Memory>) {
        let store = crate::memory::MemoryStore::connect_in_memory().await;
        let now = Utc::now();
        let mut memories = Vec::new();

        let types_and_importance = [
            (
                "user identity info",
                MemoryType::Identity,
                1.0,
                now - Duration::days(30),
            ),
            (
                "recent event",
                MemoryType::Event,
                0.4,
                now - Duration::hours(1),
            ),
            (
                "important decision",
                MemoryType::Decision,
                0.9,
                now - Duration::days(2),
            ),
            (
                "casual observation",
                MemoryType::Observation,
                0.2,
                now - Duration::days(7),
            ),
            (
                "user preference",
                MemoryType::Preference,
                0.7,
                now - Duration::days(1),
            ),
        ];

        for (content, memory_type, importance, created_at) in types_and_importance {
            let mut memory = Memory::new(content, memory_type).with_importance(importance);
            memory.created_at = created_at;
            memory.updated_at = created_at;
            memory.last_accessed_at = created_at;
            store.save(&memory).await.unwrap();
            memories.push(memory);
        }

        (store, memories)
    }

    #[tokio::test]
    async fn test_metadata_search_recent() {
        let (store, _memories) = setup_search_with_memories().await;

        // Construct MemorySearch with dummy lance/embedding (we won't use them)
        let lance_dir = tempfile::tempdir().unwrap();
        let lance_conn = lancedb::connect(lance_dir.path().to_str().unwrap())
            .execute()
            .await
            .unwrap();
        let embedding_table = EmbeddingTable::open_or_create(&lance_conn).await.unwrap();
        let embedding_model = Arc::new(EmbeddingModel::new(lance_dir.path()).unwrap());
        let backend = Arc::new(SqliteBackend::new(store, embedding_table));
        let search = MemorySearch::new(backend, embedding_model);

        let config = SearchConfig {
            mode: SearchMode::Recent,
            max_results: 3,
            ..Default::default()
        };

        let results = search.search("", &config).await.unwrap();
        assert_eq!(results.len(), 3);
        // Most recent should be first (the event from 1 hour ago)
        assert_eq!(results[0].memory.content, "recent event");
        // Scores should be descending
        assert!(results[0].score >= results[1].score);
        assert!(results[1].score >= results[2].score);
    }

    #[tokio::test]
    async fn test_metadata_search_important() {
        let (store, _memories) = setup_search_with_memories().await;

        let lance_dir = tempfile::tempdir().unwrap();
        let lance_conn = lancedb::connect(lance_dir.path().to_str().unwrap())
            .execute()
            .await
            .unwrap();
        let embedding_table = EmbeddingTable::open_or_create(&lance_conn).await.unwrap();
        let embedding_model = Arc::new(EmbeddingModel::new(lance_dir.path()).unwrap());
        let backend = Arc::new(SqliteBackend::new(store, embedding_table));
        let search = MemorySearch::new(backend, embedding_model);

        let config = SearchConfig {
            mode: SearchMode::Important,
            max_results: 5,
            ..Default::default()
        };

        let results = search.search("", &config).await.unwrap();
        // Identity (1.0) should be first, then Decision (0.9)
        assert_eq!(results[0].memory.memory_type, MemoryType::Identity);
        assert_eq!(results[1].memory.memory_type, MemoryType::Decision);
    }

    #[tokio::test]
    async fn test_metadata_search_typed() {
        let (store, _memories) = setup_search_with_memories().await;

        let lance_dir = tempfile::tempdir().unwrap();
        let lance_conn = lancedb::connect(lance_dir.path().to_str().unwrap())
            .execute()
            .await
            .unwrap();
        let embedding_table = EmbeddingTable::open_or_create(&lance_conn).await.unwrap();
        let embedding_model = Arc::new(EmbeddingModel::new(lance_dir.path()).unwrap());
        let backend = Arc::new(SqliteBackend::new(store, embedding_table));
        let search = MemorySearch::new(backend, embedding_model);

        let config = SearchConfig {
            mode: SearchMode::Typed,
            memory_type: Some(MemoryType::Decision),
            max_results: 10,
            ..Default::default()
        };

        let results = search.search("", &config).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].memory.memory_type, MemoryType::Decision);
    }

    #[tokio::test]
    async fn test_metadata_search_typed_empty() {
        let (store, _memories) = setup_search_with_memories().await;

        let lance_dir = tempfile::tempdir().unwrap();
        let lance_conn = lancedb::connect(lance_dir.path().to_str().unwrap())
            .execute()
            .await
            .unwrap();
        let embedding_table = EmbeddingTable::open_or_create(&lance_conn).await.unwrap();
        let embedding_model = Arc::new(EmbeddingModel::new(lance_dir.path()).unwrap());
        let backend = Arc::new(SqliteBackend::new(store, embedding_table));
        let search = MemorySearch::new(backend, embedding_model);

        let config = SearchConfig {
            mode: SearchMode::Typed,
            memory_type: Some(MemoryType::Goal),
            max_results: 10,
            ..Default::default()
        };

        let results = search.search("", &config).await.unwrap();
        assert!(results.is_empty());
    }

    // ── Characterization test for traverse_graph (Task D2) ────────────────────
    //
    // Builds a deterministic graph and asserts the EXACT Vec<ScoredMemory>
    // (ids + scores) that the original N+1 BFS produced, now verified against
    // the new level-batched implementation.
    //
    // Graph topology (max_depth = 1):
    //
    //   start --RelatedTo, w=0.8--> A (importance=0.8)
    //   start --RelatedTo, w=0.7--> D (importance=0.6)
    //   start --RelatedTo, w=0.5--> F (importance=0.4, FORGOTTEN)
    //
    //   A --RelatedTo,  w=0.9 --> B (importance=0.7)   re-expands
    //   A --Contradicts,w=0.6 --> C (importance=0.9)   scored, NOT re-expanded
    //   A --RelatedTo,  w=0.75--> E (importance=0.5)   A reaches E first (frontier order)
    //
    //   D --Updates,    w=0.7 --> E (importance=0.5)   E already visited from A → skip
    //
    //   B --RelatedTo,  w=0.8 --> G (importance=0.3)   NOT reached: B is depth 2 > max_depth=1
    //
    // Cases covered:
    //   ✓ RelatedTo chain re-expands (start→A→B)
    //   ✓ Contradicts scored but NOT re-expanded (A→C)
    //   ✓ Forgotten neighbour marked visited, not scored (start→F)
    //   ✓ Two DIFFERENT frontier nodes (A, D) reach E at the same level;
    //     A wins because it appears first in the frontier (deterministic cross-node)
    //   ✓ max_depth bound: B (depth 2) is not expanded → G never scored
    //
    // Golden scores ((f32_importance as f64) × (f32_weight as f64) × type_multiplier):
    //   A: 0.8f32 × 0.8f32 × 1.0  (RelatedTo)
    //   D: 0.6f32 × 0.7f32 × 1.0  (RelatedTo)
    //   B: 0.7f32 × 0.9f32 × 1.0  (RelatedTo)
    //   C: 0.9f32 × 0.6f32 × 0.5  (Contradicts)
    //   E: 0.5f32 × 0.75f32× 1.0  (RelatedTo — A reaches E first)
    //
    // Expected BFS push order: [A, D, B, C, E]

    async fn build_traverse_graph_fixture(
    ) -> (MemorySearch, String, tempfile::TempDir) {
        use crate::memory::types::Association;

        let store = crate::memory::MemoryStore::connect_in_memory().await;

        macro_rules! save_mem {
            ($content:expr, $imp:expr) => {{
                let m = Memory::new($content, MemoryType::Fact).with_importance($imp);
                store.save(&m).await.unwrap();
                m
            }};
        }

        let start_mem = save_mem!("start node", 1.0_f32);
        let a = save_mem!("node A", 0.8_f32);
        let b = save_mem!("node B", 0.7_f32);
        let c = save_mem!("node C", 0.9_f32);
        let d = save_mem!("node D", 0.6_f32);
        let e = save_mem!("node E", 0.5_f32);
        let f = save_mem!("node F forgotten", 0.4_f32);
        store.forget(&f.id).await.unwrap();
        let _g = save_mem!("node G (unreachable)", 0.3_f32);

        // Insert edges in a fixed order — SQLite rowid == insertion order, so
        // get_associations returns them in a stable sequence.
        let edge_specs: &[(&str, &str, RelationType, f32)] = &[
            (&start_mem.id, &a.id, RelationType::RelatedTo, 0.8),
            (&start_mem.id, &d.id, RelationType::RelatedTo, 0.7),
            (&start_mem.id, &f.id, RelationType::RelatedTo, 0.5),
            (&a.id, &b.id, RelationType::RelatedTo, 0.9),
            (&a.id, &c.id, RelationType::Contradicts, 0.6),
            (&a.id, &e.id, RelationType::RelatedTo, 0.75),
            (&d.id, &e.id, RelationType::Updates, 0.7),
            (&b.id, &_g.id, RelationType::RelatedTo, 0.8),
        ];
        for (src, tgt, rel, weight) in edge_specs {
            store
                .create_association(&Association::new(*src, *tgt, *rel).with_weight(*weight))
                .await
                .unwrap();
        }

        let lance_dir = tempfile::tempdir().unwrap();
        let lance_conn = lancedb::connect(lance_dir.path().to_str().unwrap())
            .execute()
            .await
            .unwrap();
        let embedding_table = EmbeddingTable::open_or_create(&lance_conn).await.unwrap();
        let embedding_model = Arc::new(EmbeddingModel::new(lance_dir.path()).unwrap());
        let backend = Arc::new(SqliteBackend::new(Arc::clone(&store), embedding_table));
        let search = MemorySearch::new(backend, embedding_model);

        (search, start_mem.id, lance_dir)
    }

    fn assert_traverse_golden(results: &[ScoredMemory]) {
        // Scores are (f32_importance as f64) * (f32_weight as f64) * type_multiplier,
        // using the same f32 intermediates as the production code to get exact values.
        fn score(imp: f32, weight: f32, mul: f64) -> f64 {
            (imp as f64) * (weight as f64) * mul
        }
        // Expected order matches BFS push order: [A, D, B, C, E]
        let expected: &[(&str, f64)] = &[
            ("node A", score(0.8, 0.8, 1.0)),  // RelatedTo
            ("node D", score(0.6, 0.7, 1.0)),  // RelatedTo
            ("node B", score(0.7, 0.9, 1.0)),  // RelatedTo
            ("node C", score(0.9, 0.6, 0.5)),  // Contradicts
            ("node E", score(0.5, 0.75, 1.0)), // RelatedTo (A reaches E first)
        ];
        assert_eq!(
            results.len(),
            expected.len(),
            "result count mismatch: got {:?}",
            results.iter().map(|r| &r.memory.content).collect::<Vec<_>>()
        );
        for (i, (r, (exp_content, exp_score))) in
            results.iter().zip(expected.iter()).enumerate()
        {
            assert_eq!(
                r.memory.content, *exp_content,
                "position {i}: expected content {exp_content:?}, got {:?}",
                r.memory.content
            );
            assert!(
                (r.score - exp_score).abs() < 1e-9,
                "position {i} ({exp_content}): expected score {exp_score}, got {}",
                r.score
            );
        }
        assert!(
            results.iter().all(|r| r.memory.content != "node F forgotten"),
            "forgotten node F must not appear in results"
        );
        assert!(
            results.iter().all(|r| r.memory.content != "node G (unreachable)"),
            "depth-bounded node G must not appear in results"
        );
    }

    #[tokio::test]
    async fn traverse_graph_batched_matches_golden() {
        let (search, start_id, _dir) = build_traverse_graph_fixture().await;
        let mut results: Vec<ScoredMemory> = Vec::new();
        search
            .traverse_graph(&start_id, 1, &mut results)
            .await
            .unwrap();
        assert_traverse_golden(&results);
    }
}
