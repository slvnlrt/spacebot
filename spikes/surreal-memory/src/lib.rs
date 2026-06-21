//! Reference implementation of the SurrealDB-backed memory backend.
//!
//! This is a *runnable, tested* prototype (against embedded SurrealDB) of what
//! `src/memory/` becomes under the design in
//! `docs/design-docs/surrealdb-memory-backend.md`. It mirrors spacebot's memory
//! types and store/search behaviour closely enough to prove the SurrealQL and
//! the surrealdb 3.x API usage before transplanting into the main crate.
//!
//! Differences from the eventual spacebot port (intentional, for isolation):
//! - No fastembed: callers pass query embeddings explicitly.
//! - Errors are `surrealdb::Error` / `anyhow`, not spacebot's `crate::error`.
//! - `Memory`/`Association` are local copies (spacebot's derive utoipa/serde).

use std::collections::{HashMap, HashSet, VecDeque};

use surrealdb::types::{Datetime, SurrealValue};
use surrealdb::{Connection, Surreal};

pub type Result<T> = anyhow::Result<T>;

// ----------------------------------------------------------------------------
// Types (mirror src/memory/types.rs)
// ----------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryType {
    Fact,
    Preference,
    Decision,
    Identity,
    Event,
    Observation,
    Goal,
    Todo,
}

impl MemoryType {
    pub fn as_str(self) -> &'static str {
        match self {
            MemoryType::Fact => "fact",
            MemoryType::Preference => "preference",
            MemoryType::Decision => "decision",
            MemoryType::Identity => "identity",
            MemoryType::Event => "event",
            MemoryType::Observation => "observation",
            MemoryType::Goal => "goal",
            MemoryType::Todo => "todo",
        }
    }
    pub fn from_str(s: &str) -> Option<Self> {
        Some(match s {
            "fact" => MemoryType::Fact,
            "preference" => MemoryType::Preference,
            "decision" => MemoryType::Decision,
            "identity" => MemoryType::Identity,
            "event" => MemoryType::Event,
            "observation" => MemoryType::Observation,
            "goal" => MemoryType::Goal,
            "todo" => MemoryType::Todo,
            _ => return None,
        })
    }
    pub fn default_importance(self) -> f32 {
        match self {
            MemoryType::Identity => 1.0,
            MemoryType::Goal => 0.9,
            MemoryType::Decision | MemoryType::Todo => 0.8,
            MemoryType::Preference => 0.7,
            MemoryType::Fact => 0.6,
            MemoryType::Event => 0.4,
            MemoryType::Observation => 0.3,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelationType {
    RelatedTo,
    Updates,
    Contradicts,
    CausedBy,
    ResultOf,
    PartOf,
}

impl RelationType {
    pub fn as_str(self) -> &'static str {
        match self {
            RelationType::RelatedTo => "related_to",
            RelationType::Updates => "updates",
            RelationType::Contradicts => "contradicts",
            RelationType::CausedBy => "caused_by",
            RelationType::ResultOf => "result_of",
            RelationType::PartOf => "part_of",
        }
    }
    pub fn from_str(s: &str) -> Option<Self> {
        Some(match s {
            "related_to" => RelationType::RelatedTo,
            "updates" => RelationType::Updates,
            "contradicts" => RelationType::Contradicts,
            "caused_by" => RelationType::CausedBy,
            "result_of" => RelationType::ResultOf,
            "part_of" => RelationType::PartOf,
            _ => return None,
        })
    }
    /// Traversal score multiplier (mirrors search.rs:310-316).
    pub fn multiplier(self) -> f64 {
        match self {
            RelationType::Updates => 1.5,
            RelationType::CausedBy | RelationType::ResultOf => 1.3,
            RelationType::RelatedTo => 1.0,
            RelationType::PartOf => 0.8,
            RelationType::Contradicts => 0.5,
        }
    }
    /// Whether the BFS expands deeper through this edge (search.rs:325-331).
    pub fn expands(self) -> bool {
        matches!(self, RelationType::RelatedTo | RelationType::PartOf)
    }
}

#[derive(Debug, Clone)]
pub struct Memory {
    pub id: String,
    pub content: String,
    pub memory_type: MemoryType,
    pub importance: f32,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
    pub last_accessed_at: chrono::DateTime<chrono::Utc>,
    pub access_count: i64,
    pub source: Option<String>,
    pub channel_id: Option<String>,
    pub forgotten: bool,
}

impl Memory {
    pub fn new(content: impl Into<String>, memory_type: MemoryType) -> Self {
        let now = chrono::Utc::now();
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            content: content.into(),
            memory_type,
            importance: memory_type.default_importance(),
            created_at: now,
            updated_at: now,
            last_accessed_at: now,
            access_count: 0,
            source: None,
            channel_id: None,
            forgotten: false,
        }
    }
    pub fn with_importance(mut self, importance: f32) -> Self {
        self.importance = importance.clamp(0.0, 1.0);
        self
    }
}

#[derive(Debug, Clone)]
pub struct Association {
    pub source_id: String,
    pub target_id: String,
    pub relation_type: RelationType,
    pub weight: f32,
}

impl Association {
    pub fn new(source_id: impl Into<String>, target_id: impl Into<String>, rt: RelationType) -> Self {
        Self {
            source_id: source_id.into(),
            target_id: target_id.into(),
            relation_type: rt,
            weight: 0.5,
        }
    }
    pub fn with_weight(mut self, w: f32) -> Self {
        self.weight = w.clamp(0.0, 1.0);
        self
    }
}

#[derive(Debug, Clone)]
pub struct MemorySearchResult {
    pub memory: Memory,
    pub score: f32,
    pub rank: usize,
}

// ----------------------------------------------------------------------------
// DB row structs (SurrealValue) — the bridge to/from SurrealDB
// ----------------------------------------------------------------------------

#[derive(Debug, Clone, SurrealValue)]
struct MemoryRow {
    id: String,
    content: String,
    memory_type: String,
    importance: f64,
    created_at: Datetime,
    updated_at: Datetime,
    last_accessed_at: Datetime,
    access_count: i64,
    source: Option<String>,
    channel_id: Option<String>,
    forgotten: bool,
}

impl MemoryRow {
    fn into_memory(self) -> Memory {
        Memory {
            id: self.id,
            content: self.content,
            memory_type: MemoryType::from_str(&self.memory_type).unwrap_or(MemoryType::Fact),
            importance: self.importance as f32,
            created_at: self.created_at.into(),
            updated_at: self.updated_at.into(),
            last_accessed_at: self.last_accessed_at.into(),
            access_count: self.access_count,
            source: self.source,
            channel_id: self.channel_id,
            forgotten: self.forgotten,
        }
    }
}

#[derive(Debug, Clone, SurrealValue)]
struct AssocRow {
    source: String,
    target: String,
    relation_type: String,
    weight: f64,
}

#[derive(Debug, Clone, SurrealValue)]
struct IdScore {
    id: String,
    score: f64,
}

#[derive(Debug, Clone, SurrealValue)]
struct IdDist {
    id: String,
    distance: f64,
}

// ----------------------------------------------------------------------------
// Store (mirrors src/memory/store.rs + lance.rs)
// ----------------------------------------------------------------------------

pub struct MemoryStore<C: Connection> {
    db: Surreal<C>,
}

impl<C: Connection> MemoryStore<C> {
    pub fn new(db: Surreal<C>) -> Self {
        Self { db }
    }

    pub fn db(&self) -> &Surreal<C> {
        &self.db
    }

    /// Apply the schema (idempotent via IF NOT EXISTS). `dim` is the embedding
    /// dimension (384 in spacebot; small in tests).
    pub async fn define_schema(&self, dim: usize) -> Result<()> {
        let sql = format!(
            r#"
            DEFINE TABLE IF NOT EXISTS memory SCHEMAFULL;
            DEFINE FIELD IF NOT EXISTS content   ON memory TYPE string;
            DEFINE FIELD IF NOT EXISTS memory_type ON memory TYPE string
              ASSERT $value IN ['fact','preference','decision','identity','event','observation','goal','todo'];
            DEFINE FIELD IF NOT EXISTS importance ON memory TYPE float DEFAULT 0.5;
            DEFINE FIELD IF NOT EXISTS created_at ON memory TYPE datetime DEFAULT time::now();
            DEFINE FIELD IF NOT EXISTS updated_at ON memory TYPE datetime DEFAULT time::now();
            DEFINE FIELD IF NOT EXISTS last_accessed_at ON memory TYPE datetime DEFAULT time::now();
            DEFINE FIELD IF NOT EXISTS access_count ON memory TYPE int DEFAULT 0;
            DEFINE FIELD IF NOT EXISTS source ON memory TYPE option<string>;
            DEFINE FIELD IF NOT EXISTS channel_id ON memory TYPE option<string>;
            DEFINE FIELD IF NOT EXISTS forgotten ON memory TYPE bool DEFAULT false;
            DEFINE FIELD IF NOT EXISTS embedding ON memory TYPE option<array<float>>;
            DEFINE INDEX IF NOT EXISTS memory_hnsw ON memory FIELDS embedding HNSW DIMENSION {dim} TYPE F32 DIST COSINE;
            DEFINE ANALYZER IF NOT EXISTS memory_an TOKENIZERS class FILTERS lowercase, ascii;
            DEFINE INDEX IF NOT EXISTS memory_fts ON memory FIELDS content FULLTEXT ANALYZER memory_an BM25;
            DEFINE INDEX IF NOT EXISTS memory_type_idx ON memory FIELDS memory_type;
            DEFINE INDEX IF NOT EXISTS memory_importance_idx ON memory FIELDS importance;
            DEFINE TABLE IF NOT EXISTS relates SCHEMAFULL TYPE RELATION FROM memory TO memory;
            DEFINE FIELD IF NOT EXISTS relation_type ON relates TYPE string;
            DEFINE FIELD IF NOT EXISTS weight ON relates TYPE float DEFAULT 0.5;
            DEFINE INDEX IF NOT EXISTS relates_unique ON relates FIELDS in, out, relation_type UNIQUE;
        "#
        );
        self.db.query(sql).await?.check()?;
        Ok(())
    }

    /// Insert or replace a memory, with optional embedding.
    pub async fn save(&self, m: &Memory, embedding: Option<&[f32]>) -> Result<()> {
        let emb: Option<Vec<f32>> = embedding.map(|e| e.to_vec());
        self.db
            .query(
                "CREATE type::record('memory', $id) SET \
                 content=$content, memory_type=$mt, importance=$imp, \
                 created_at=$ca, updated_at=$ua, last_accessed_at=$la, \
                 access_count=$ac, source=$src, channel_id=$cid, \
                 forgotten=$forg, embedding=$emb",
            )
            .bind(("id", m.id.clone()))
            .bind(("content", m.content.clone()))
            .bind(("mt", m.memory_type.as_str().to_string()))
            .bind(("imp", m.importance as f64))
            .bind(("ca", Datetime::from(m.created_at)))
            .bind(("ua", Datetime::from(m.updated_at)))
            .bind(("la", Datetime::from(m.last_accessed_at)))
            .bind(("ac", m.access_count))
            .bind(("src", m.source.clone()))
            .bind(("cid", m.channel_id.clone()))
            .bind(("forg", m.forgotten))
            .bind(("emb", emb))
            .await?
            .check()?;
        Ok(())
    }

    pub async fn load(&self, id: &str) -> Result<Option<Memory>> {
        let mut r = self
            .db
            .query(
                "SELECT meta::id(id) AS id, content, memory_type, importance, \
                 created_at, updated_at, last_accessed_at, access_count, source, \
                 channel_id, forgotten FROM type::record('memory', $id)",
            )
            .bind(("id", id.to_string()))
            .await?;
        let rows: Vec<MemoryRow> = r.take(0)?;
        Ok(rows.into_iter().next().map(MemoryRow::into_memory))
    }

    /// Increment access_count and bump last_accessed_at.
    pub async fn touch(&self, id: &str) -> Result<()> {
        self.db
            .query(
                "UPDATE type::record('memory', $id) SET \
                 access_count += 1, last_accessed_at = time::now()",
            )
            .bind(("id", id.to_string()))
            .await?
            .check()?;
        Ok(())
    }

    pub async fn set_importance(&self, id: &str, importance: f32) -> Result<()> {
        self.db
            .query("UPDATE type::record('memory', $id) SET importance = $imp, updated_at = time::now()")
            .bind(("id", id.to_string()))
            .bind(("imp", importance.clamp(0.0, 1.0) as f64))
            .await?
            .check()?;
        Ok(())
    }

    /// Soft delete.
    pub async fn forget(&self, id: &str) -> Result<()> {
        self.db
            .query("UPDATE type::record('memory', $id) SET forgotten = true")
            .bind(("id", id.to_string()))
            .await?
            .check()?;
        Ok(())
    }

    /// Hard delete (and its edges).
    pub async fn delete(&self, id: &str) -> Result<()> {
        self.db
            .query("DELETE type::record('memory', $id)")
            .bind(("id", id.to_string()))
            .await?
            .check()?;
        Ok(())
    }

    /// Sorted, non-forgotten memories, optionally filtered by type.
    pub async fn get_sorted(
        &self,
        sort: SearchSort,
        limit: usize,
        type_filter: Option<MemoryType>,
    ) -> Result<Vec<Memory>> {
        let order = match sort {
            SearchSort::Recent => "created_at DESC",
            SearchSort::Importance => "importance DESC",
            SearchSort::MostAccessed => "access_count DESC",
        };
        let type_clause = if type_filter.is_some() {
            "AND memory_type = $mt"
        } else {
            ""
        };
        let sql = format!(
            "SELECT meta::id(id) AS id, content, memory_type, importance, \
             created_at, updated_at, last_accessed_at, access_count, source, \
             channel_id, forgotten FROM memory \
             WHERE forgotten = false {type_clause} ORDER BY {order} LIMIT {limit}"
        );
        let mut q = self.db.query(sql);
        if let Some(t) = type_filter {
            q = q.bind(("mt", t.as_str().to_string()));
        }
        let mut r = q.await?;
        let rows: Vec<MemoryRow> = r.take(0)?;
        Ok(rows.into_iter().map(MemoryRow::into_memory).collect())
    }

    pub async fn get_high_importance(&self, threshold: f32, limit: usize) -> Result<Vec<Memory>> {
        let sql = format!(
            "SELECT meta::id(id) AS id, content, memory_type, importance, \
             created_at, updated_at, last_accessed_at, access_count, source, \
             channel_id, forgotten FROM memory \
             WHERE forgotten = false AND importance >= $th ORDER BY importance DESC LIMIT {limit}"
        );
        let mut r = self.db.query(sql).bind(("th", threshold as f64)).await?;
        let rows: Vec<MemoryRow> = r.take(0)?;
        Ok(rows.into_iter().map(MemoryRow::into_memory).collect())
    }

    // ---- associations / graph ----

    pub async fn add_association(&self, a: &Association) -> Result<()> {
        // RELATE does not accept `type::record(...)` as endpoints; bind RecordId
        // values directly and use the arrow form `$s->relates->$t`.
        let s = surrealdb::types::RecordId::new("memory", a.source_id.clone());
        let t = surrealdb::types::RecordId::new("memory", a.target_id.clone());
        self.db
            .query("RELATE $s->relates->$t SET relation_type = $rt, weight = $w")
            .bind(("s", s))
            .bind(("t", t))
            .bind(("rt", a.relation_type.as_str().to_string()))
            .bind(("w", a.weight as f64))
            .await?
            .check()?;
        Ok(())
    }

    /// All associations touching a memory (either direction).
    pub async fn get_associations(&self, memory_id: &str) -> Result<Vec<Association>> {
        let mut r = self
            .db
            .query(
                "SELECT meta::id(in) AS source, meta::id(out) AS target, relation_type, weight \
                 FROM relates WHERE in = type::record('memory', $m) OR out = type::record('memory', $m)",
            )
            .bind(("m", memory_id.to_string()))
            .await?;
        let rows: Vec<AssocRow> = r.take(0)?;
        Ok(rows
            .into_iter()
            .filter_map(|a| {
                RelationType::from_str(&a.relation_type).map(|rt| Association {
                    source_id: a.source,
                    target_id: a.target,
                    relation_type: rt,
                    weight: a.weight as f32,
                })
            })
            .collect())
    }

    /// Graph-view BFS neighbours up to `depth` (mirrors store.rs::get_neighbors).
    pub async fn get_neighbors(&self, id: &str, depth: usize) -> Result<Vec<Memory>> {
        let mut visited: HashSet<String> = HashSet::new();
        let mut queue: VecDeque<(String, usize)> = VecDeque::new();
        let mut out = Vec::new();
        visited.insert(id.to_string());
        queue.push_back((id.to_string(), 0));
        while let Some((cur, d)) = queue.pop_front() {
            if d >= depth {
                continue;
            }
            for assoc in self.get_associations(&cur).await? {
                let other = if assoc.source_id == cur {
                    assoc.target_id
                } else {
                    assoc.source_id
                };
                if !visited.insert(other.clone()) {
                    continue;
                }
                if let Some(m) = self.load(&other).await? {
                    if !m.forgotten {
                        out.push(m);
                        queue.push_back((other, d + 1));
                    }
                }
            }
        }
        Ok(out)
    }

    /// Server-side prune: delete non-identity memories below `threshold` and
    /// older than `older_than`. Returns the number deleted. Edges cascade.
    pub async fn prune_below(
        &self,
        threshold: f32,
        older_than: chrono::DateTime<chrono::Utc>,
    ) -> Result<u64> {
        let mut r = self
            .db
            .query(
                "DELETE memory WHERE importance < $t AND created_at < $cut \
                 AND memory_type != 'identity' RETURN BEFORE",
            )
            .bind(("t", threshold as f64))
            .bind(("cut", surrealdb::types::Datetime::from(older_than)))
            .await?;
        let deleted: surrealdb::types::Value = r.take(0)?;
        let n = if let surrealdb::types::Value::Array(a) = deleted {
            a.len() as u64
        } else {
            0
        };
        Ok(n)
    }

    // ---- vector + fts (mirror lance.rs) ----

    /// KNN by cosine distance. Returns (id, distance) ascending. K/EF literals.
    pub async fn vector_search(&self, embedding: &[f32], limit: usize) -> Result<Vec<(String, f32)>> {
        let ef = (limit * 4).max(40);
        let sql = format!(
            "SELECT meta::id(id) AS id, vector::distance::knn() AS distance FROM memory \
             WHERE embedding <|{limit},{ef}|> $q AND forgotten = false ORDER BY distance"
        );
        let mut r = self.db.query(sql).bind(("q", embedding.to_vec())).await?;
        let rows: Vec<IdDist> = r.take(0)?;
        Ok(rows.into_iter().map(|x| (x.id, x.distance as f32)).collect())
    }

    /// Full-text BM25 search. Returns (id, score) descending.
    pub async fn text_search(&self, query: &str, limit: usize) -> Result<Vec<(String, f32)>> {
        let sql = format!(
            "SELECT meta::id(id) AS id, search::score(0) AS score FROM memory \
             WHERE content @0@ $q AND forgotten = false ORDER BY score DESC LIMIT {limit}"
        );
        let mut r = self.db.query(sql).bind(("q", query.to_string())).await?;
        let rows: Vec<IdScore> = r.take(0)?;
        Ok(rows.into_iter().map(|x| (x.id, x.score as f32)).collect())
    }

    /// Merge `loser` into `survivor` atomically (single BEGIN/COMMIT transaction).
    /// Sets survivor content/embedding, rewires the loser's edges onto the
    /// survivor (preserving direction/type/weight; no self-loops; pre-deleting
    /// any conflicting survivor edge to satisfy the UNIQUE index), drops the
    /// loser's edges, adds `survivor->updates->loser`, and soft-deletes the loser.
    /// SurrealDB rolls the whole thing back on any failure.
    pub async fn merge(
        &self,
        survivor_id: &str,
        loser_id: &str,
        new_content: &str,
        new_embedding: Option<&[f32]>,
    ) -> Result<()> {
        use surrealdb::types::RecordId;

        // Read the loser's edges first (reads can't share the write transaction
        // cleanly), then compute the rewired set in Rust.
        let mut rewires: Vec<(String, String, RelationType, f32)> = Vec::new();
        for a in self.get_associations(loser_id).await? {
            let mut s = a.source_id;
            let mut t = a.target_id;
            if s == loser_id {
                s = survivor_id.to_string();
            }
            if t == loser_id {
                t = survivor_id.to_string();
            }
            if s == t {
                continue; // self-loop
            }
            rewires.push((s, t, a.relation_type, a.weight));
        }

        // Build one transaction with indexed params.
        let mut sql = String::from("BEGIN;\n");
        sql.push_str(
            "UPDATE $survivor SET content=$content, updated_at=time::now(), embedding=$emb;\n",
        );
        for i in 0..rewires.len() {
            sql.push_str(&format!(
                "DELETE relates WHERE in=$s{i} AND out=$t{i} AND relation_type=$rt{i};\n"
            ));
            sql.push_str(&format!(
                "RELATE $s{i}->relates->$t{i} SET relation_type=$rt{i}, weight=$w{i};\n"
            ));
        }
        sql.push_str("DELETE relates WHERE in=$loser OR out=$loser;\n");
        sql.push_str(
            "RELATE $survivor->relates->$loser SET relation_type='updates', weight=1.0;\n",
        );
        sql.push_str("UPDATE $loser SET forgotten=true;\n");
        sql.push_str("COMMIT;");

        let mut q = self
            .db
            .query(sql)
            .bind(("survivor", RecordId::new("memory", survivor_id.to_string())))
            .bind(("loser", RecordId::new("memory", loser_id.to_string())))
            .bind(("content", new_content.to_string()))
            .bind(("emb", new_embedding.map(|e| e.to_vec())));
        for (i, (s, t, rt, w)) in rewires.into_iter().enumerate() {
            q = q
                .bind((format!("s{i}"), RecordId::new("memory", s)))
                .bind((format!("t{i}"), RecordId::new("memory", t)))
                .bind((format!("rt{i}"), rt.as_str().to_string()))
                .bind((format!("w{i}"), w as f64));
        }
        q.await?.check()?;
        Ok(())
    }

    /// Memories similar to an existing memory's own embedding, excluding self.
    /// Returns (id, similarity = 1 - distance) >= threshold.
    pub async fn find_similar(&self, id: &str, threshold: f32, limit: usize) -> Result<Vec<(String, f32)>> {
        let fetch = limit + 1;
        let ef = (fetch * 4).max(40);
        let sql = format!(
            "LET $vec = (SELECT VALUE embedding FROM ONLY type::record('memory', $id)); \
             SELECT meta::id(id) AS id, vector::distance::knn() AS distance FROM memory \
             WHERE embedding <|{fetch},{ef}|> $vec AND forgotten = false ORDER BY distance"
        );
        let mut r = self.db.query(sql).bind(("id", id.to_string())).await?;
        let rows: Vec<IdDist> = r.take(1)?; // statement 1 is the SELECT
        let mut out = Vec::new();
        for x in rows {
            if x.id == id {
                continue;
            }
            let sim = 1.0 - x.distance as f32;
            if sim >= threshold {
                out.push((x.id, sim));
            }
            if out.len() >= limit {
                break;
            }
        }
        Ok(out)
    }
}

// ----------------------------------------------------------------------------
// Search (mirrors src/memory/search.rs)
// ----------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SearchMode {
    #[default]
    Hybrid,
    Recent,
    Important,
    Typed,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SearchSort {
    #[default]
    Recent,
    Importance,
    MostAccessed,
}

#[derive(Debug, Clone)]
pub struct SearchConfig {
    pub mode: SearchMode,
    pub memory_type: Option<MemoryType>,
    pub sort_by: SearchSort,
    pub max_results: usize,
    pub max_results_per_source: usize,
    pub rrf_k: f64,
    pub min_score: f32,
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
            min_score: 0.0,
            max_graph_depth: 2,
        }
    }
}

#[derive(Debug, Clone)]
struct Scored {
    memory: Memory,
    score: f64,
}

/// Hybrid search: vector + FTS + graph traversal fused with RRF.
/// `query_embedding` stands in for fastembed (computed by the caller).
pub async fn hybrid_search<C: Connection>(
    store: &MemoryStore<C>,
    query: &str,
    query_embedding: &[f32],
    config: &SearchConfig,
) -> Result<Vec<MemorySearchResult>> {
    let n = config.max_results_per_source;
    let mut vector_results = Vec::new();
    let mut fts_results = Vec::new();
    let mut graph_results = Vec::new();

    // 1. FTS
    if let Ok(hits) = store.text_search(query, n).await {
        for (id, score) in hits {
            if let Some(m) = store.load(&id).await? {
                if !m.forgotten {
                    fts_results.push(Scored { memory: m, score: score as f64 });
                }
            }
        }
    }

    // 2. Vector
    if let Ok(hits) = store.vector_search(query_embedding, n).await {
        for (id, distance) in hits {
            if let Some(m) = store.load(&id).await? {
                if !m.forgotten {
                    vector_results.push(Scored { memory: m, score: (1.0 - distance) as f64 });
                }
            }
        }
    }

    // 3. Graph from high-importance seeds keyword-matching the query
    let seeds = store.get_high_importance(0.8, 20).await?;
    let q_lower = query.to_lowercase();
    for seed in seeds {
        let matches = q_lower
            .split_whitespace()
            .any(|term| seed.content.to_lowercase().contains(term));
        if matches {
            graph_results.push(Scored { memory: seed.clone(), score: seed.importance as f64 });
            traverse_graph(store, &seed.id, config.max_graph_depth, &mut graph_results).await?;
        }
    }

    let fused = reciprocal_rank_fusion(&vector_results, &fts_results, &graph_results, config.rrf_k);
    let results = fused
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

/// Iterative BFS (mirrors search.rs::traverse_graph): depths 0..=max_depth,
/// per-relation-type scoring, re-queue only expanding relations, skip forgotten.
async fn traverse_graph<C: Connection>(
    store: &MemoryStore<C>,
    start_id: &str,
    max_depth: usize,
    results: &mut Vec<Scored>,
) -> Result<()> {
    let mut queue: VecDeque<(String, usize)> = VecDeque::new();
    let mut visited: HashSet<String> = HashSet::new();
    queue.push_back((start_id.to_string(), 0));
    visited.insert(start_id.to_string());

    while let Some((cur, depth)) = queue.pop_front() {
        if depth > max_depth {
            continue;
        }
        for assoc in store.get_associations(&cur).await? {
            let related = if assoc.source_id == cur {
                assoc.target_id.clone()
            } else {
                assoc.source_id.clone()
            };
            if !visited.insert(related.clone()) {
                continue;
            }
            if let Some(m) = store.load(&related).await? {
                if m.forgotten {
                    continue;
                }
                let score = m.importance as f64 * assoc.weight as f64 * assoc.relation_type.multiplier();
                results.push(Scored { memory: m, score });
                if assoc.relation_type.expands() {
                    queue.push_back((related, depth + 1));
                }
            }
        }
    }
    Ok(())
}

/// RRF over the three sources (verbatim from search.rs).
fn reciprocal_rank_fusion(
    vector_results: &[Scored],
    fts_results: &[Scored],
    graph_results: &[Scored],
    k: f64,
) -> Vec<Scored> {
    let mut rrf: HashMap<String, (f64, Memory)> = HashMap::new();
    for list in [vector_results, fts_results, graph_results] {
        for (rank, s) in list.iter().enumerate() {
            let contrib = 1.0 / (k + (rank as f64 + 1.0));
            let e = rrf.entry(s.memory.id.clone()).or_insert((0.0, s.memory.clone()));
            e.0 += contrib;
        }
    }
    let mut fused: Vec<Scored> = rrf
        .into_iter()
        .map(|(_, (score, memory))| Scored { memory, score })
        .collect();
    fused.sort_by(|a, b| b.score.total_cmp(&a.score));
    fused
}
