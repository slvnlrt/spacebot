//! Experimental SurrealDB-backed memory store (feature `surreal-memory`).
//!
//! A single embedded SurrealDB instance that unifies what `store.rs` (SQLite)
//! and `lance.rs` (LanceDB) do today: memory records, graph associations
//! (`RELATE` edges), vector KNN, and full-text search live on one `memory`
//! table. Embeddings are still generated externally (`embedding.rs` / fastembed)
//! and passed in.
//!
//! Design + rationale: `docs/design-docs/surrealdb-memory/design.md`. The query
//! shapes here were validated against embedded SurrealDB 3.1.5 in
//! `spikes/surreal-memory/` (a runnable, tested reference port).
//!
//! Phase 1 scope: the store + vector/FTS primitives. Hybrid RRF search (Phase 2)
//! still lives in `search.rs`; this type exposes the same primitives it needs.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

use surrealdb::Surreal;
use surrealdb::engine::local::{Db, SurrealKv};
use surrealdb::types::{Datetime, RecordId, SurrealValue};

use crate::error::{DbError, Result};
use crate::memory::search::SearchSort;
use crate::memory::types::{Association, Memory, MemoryType, RelationType};

fn err<E: std::fmt::Display>(e: E) -> crate::error::Error {
    DbError::Surreal(e.to_string()).into()
}

/// Map a stored `memory_type` string back to the enum (mirrors
/// `store.rs::parse_memory_type`; unknown defaults to `Fact`).
fn mt_from_str(s: &str) -> MemoryType {
    match s {
        "preference" => MemoryType::Preference,
        "decision" => MemoryType::Decision,
        "identity" => MemoryType::Identity,
        "event" => MemoryType::Event,
        "observation" => MemoryType::Observation,
        "goal" => MemoryType::Goal,
        "todo" => MemoryType::Todo,
        _ => MemoryType::Fact,
    }
}

/// Map a stored `relation_type` string back to the enum (unknown -> RelatedTo).
fn rt_from_str(s: &str) -> RelationType {
    match s {
        "updates" => RelationType::Updates,
        "contradicts" => RelationType::Contradicts,
        "caused_by" => RelationType::CausedBy,
        "result_of" => RelationType::ResultOf,
        "part_of" => RelationType::PartOf,
        _ => RelationType::RelatedTo,
    }
}

// --- DB row structs (the SurrealValue bridge) ---

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

impl From<MemoryRow> for Memory {
    fn from(r: MemoryRow) -> Self {
        Memory {
            id: r.id,
            content: r.content,
            memory_type: mt_from_str(&r.memory_type),
            importance: r.importance as f32,
            created_at: r.created_at.into(),
            updated_at: r.updated_at.into(),
            last_accessed_at: r.last_accessed_at.into(),
            access_count: r.access_count,
            source: r.source,
            channel_id: r.channel_id,
            forgotten: r.forgotten,
        }
    }
}

#[derive(Debug, Clone, SurrealValue)]
struct AssocRow {
    source: String,
    target: String,
    relation_type: String,
    weight: f64,
    created_at: Datetime,
}

impl From<AssocRow> for Association {
    fn from(r: AssocRow) -> Self {
        Association {
            // Edges have no separate uuid; synthesize a stable-ish id for the API.
            id: format!("{}:{}:{}", r.source, r.relation_type, r.target),
            source_id: r.source,
            target_id: r.target,
            relation_type: rt_from_str(&r.relation_type),
            weight: r.weight as f32,
            created_at: r.created_at.into(),
        }
    }
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

/// Columns selected for every full `Memory` read.
const MEMORY_COLS: &str = "meta::id(id) AS id, content, memory_type, importance, \
     created_at, updated_at, last_accessed_at, access_count, source, channel_id, forgotten";

/// Extract `RecordId`s from a `{..N+collect}` traversal result, deduplicating
/// into `seen` (string keys) and appending new ones to `out`.
///
/// SurrealDB 3.1.x returns the collect result as a `Value::Array` of
/// `Value::RecordId`s (possibly nested when the graph has no edges — the
/// array may be empty or contain a single NONE). UUID-keyed memory records
/// have `RecordIdKey::String` keys.
fn extract_rids_into(
    v: &surrealdb::types::Value,
    seen: &mut std::collections::HashSet<String>,
    out: &mut Vec<RecordId>,
) {
    match v {
        surrealdb::types::Value::Array(arr) => {
            for item in arr.iter() {
                extract_rids_into(item, seen, out);
            }
        }
        surrealdb::types::Value::RecordId(rid) => {
            if let surrealdb::types::RecordIdKey::String(ref key) = rid.key
                && seen.insert(key.clone())
            {
                out.push(RecordId::new(rid.table.as_str(), key.clone()));
            }
        }
        _ => {}
    }
}

/// Embedded SurrealDB memory store.
pub struct SurrealMemoryStore {
    db: Surreal<Db>,
    agent_id: String,
    dim: usize,
}

impl SurrealMemoryStore {
    /// Open (or create) an embedded SurrealKV store under `data_dir/surreal`,
    /// scoped to namespace `spacebot` and database `agent_id`, and apply the
    /// schema. `dim` is the embedding dimension (384 for all-MiniLM-L6-v2).
    pub async fn open(
        data_dir: &Path,
        agent_id: impl Into<String>,
        dim: usize,
    ) -> Result<Arc<Self>> {
        let path = data_dir.join("surreal");
        std::fs::create_dir_all(&path).map_err(|e| err(format!("create surreal dir: {e}")))?;
        let db = Surreal::new::<SurrealKv>(path.to_string_lossy().as_ref())
            .await
            .map_err(err)?;
        let agent_id = agent_id.into();
        db.use_ns("spacebot")
            .use_db(agent_id.as_str())
            .await
            .map_err(err)?;
        let store = Self { db, agent_id, dim };
        store.define_schema().await?;
        Ok(Arc::new(store))
    }

    /// Build from an existing handle (e.g. a shared instance or a `kv-mem`
    /// test database). Caller has already selected ns/db.
    pub async fn from_handle(
        db: Surreal<Db>,
        agent_id: impl Into<String>,
        dim: usize,
    ) -> Result<Arc<Self>> {
        let store = Self {
            db,
            agent_id: agent_id.into(),
            dim,
        };
        store.define_schema().await?;
        Ok(Arc::new(store))
    }

    pub fn agent_id(&self) -> &str {
        &self.agent_id
    }

    /// Apply the schema. Idempotent (`IF NOT EXISTS`).
    ///
    /// # Analyzer `IF NOT EXISTS` caveat
    ///
    /// The `memory_an` analyzer now includes `snowball(english)` stemming.
    /// `IF NOT EXISTS` is sufficient here because **no on-disk SurrealKV store
    /// exists yet** (the backend isn't production-enabled — decision C open) —
    /// fresh stores (tests, new agents) will always get this analyzer.
    ///
    /// **IMPORTANT — live migration:** if an on-disk Surreal store already
    /// exists, `IF NOT EXISTS` will silently leave the old analyzer in place
    /// (without stemming). To apply this change to a live store you must:
    ///
    /// 1. `DEFINE ANALYZER OVERWRITE memory_an TOKENIZERS class FILTERS lowercase, ascii, snowball(english);`
    /// 2. Rebuild the FTS index (re-index `memory_fts`) so it is tokenised with
    ///    the new analyzer — otherwise searches will silently mismatch.
    ///
    /// See followups.md for the live-migration item.
    pub async fn define_schema(&self) -> Result<()> {
        let dim = self.dim;
        let sql = format!(
            "DEFINE TABLE IF NOT EXISTS memory SCHEMAFULL;\
             DEFINE FIELD IF NOT EXISTS content ON memory TYPE string;\
             DEFINE FIELD IF NOT EXISTS memory_type ON memory TYPE string \
               ASSERT $value IN ['fact','preference','decision','identity','event','observation','goal','todo'];\
             DEFINE FIELD IF NOT EXISTS importance ON memory TYPE float DEFAULT 0.5;\
             DEFINE FIELD IF NOT EXISTS created_at ON memory TYPE datetime DEFAULT time::now();\
             DEFINE FIELD IF NOT EXISTS updated_at ON memory TYPE datetime DEFAULT time::now();\
             DEFINE FIELD IF NOT EXISTS last_accessed_at ON memory TYPE datetime DEFAULT time::now();\
             DEFINE FIELD IF NOT EXISTS access_count ON memory TYPE int DEFAULT 0;\
             DEFINE FIELD IF NOT EXISTS source ON memory TYPE option<string>;\
             DEFINE FIELD IF NOT EXISTS channel_id ON memory TYPE option<string>;\
             DEFINE FIELD IF NOT EXISTS forgotten ON memory TYPE bool DEFAULT false;\
             DEFINE FIELD IF NOT EXISTS embedding ON memory TYPE option<array<float>>;\
             DEFINE INDEX IF NOT EXISTS memory_hnsw ON memory FIELDS embedding HNSW DIMENSION {dim} TYPE F32 DIST COSINE;\
             DEFINE ANALYZER IF NOT EXISTS memory_an TOKENIZERS class FILTERS lowercase, ascii, snowball(english);\
             DEFINE INDEX IF NOT EXISTS memory_fts ON memory FIELDS content FULLTEXT ANALYZER memory_an BM25;\
             DEFINE INDEX IF NOT EXISTS memory_type_idx ON memory FIELDS memory_type;\
             DEFINE INDEX IF NOT EXISTS memory_importance_idx ON memory FIELDS importance;\
             DEFINE TABLE IF NOT EXISTS relates SCHEMAFULL TYPE RELATION FROM memory TO memory;\
             DEFINE FIELD IF NOT EXISTS relation_type ON relates TYPE string;\
             DEFINE FIELD IF NOT EXISTS weight ON relates TYPE float DEFAULT 0.5;\
             DEFINE FIELD IF NOT EXISTS created_at ON relates TYPE datetime DEFAULT time::now();\
             DEFINE INDEX IF NOT EXISTS relates_unique ON relates FIELDS in, out, relation_type UNIQUE;"
        );
        self.db
            .query(sql)
            .await
            .map_err(err)?
            .check()
            .map_err(err)?;
        Ok(())
    }

    // ---- CRUD ----

    /// Insert (or replace) a memory, with an optional embedding.
    pub async fn save(&self, memory: &Memory, embedding: Option<&[f32]>) -> Result<()> {
        let emb: Option<Vec<f32>> = embedding.map(<[f32]>::to_vec);
        self.db
            .query(
                "CREATE type::record('memory', $id) SET \
                 content=$content, memory_type=$mt, importance=$imp, \
                 created_at=$ca, updated_at=$ua, last_accessed_at=$la, \
                 access_count=$ac, source=$src, channel_id=$cid, \
                 forgotten=$forg, embedding=$emb",
            )
            .bind(("id", memory.id.clone()))
            .bind(("content", memory.content.clone()))
            .bind(("mt", memory.memory_type.to_string()))
            .bind(("imp", memory.importance as f64))
            .bind(("ca", Datetime::from(memory.created_at)))
            .bind(("ua", Datetime::from(memory.updated_at)))
            .bind(("la", Datetime::from(memory.last_accessed_at)))
            .bind(("ac", memory.access_count))
            .bind(("src", memory.source.clone()))
            .bind(("cid", memory.channel_id.clone()))
            .bind(("forg", memory.forgotten))
            .bind(("emb", emb))
            .await
            .map_err(err)?
            .check()
            .map_err(err)?;
        Ok(())
    }

    pub async fn load(&self, id: &str) -> Result<Option<Memory>> {
        let sql = format!("SELECT {MEMORY_COLS} FROM type::record('memory', $id)");
        let mut r = self
            .db
            .query(sql)
            .bind(("id", id.to_string()))
            .await
            .map_err(err)?;
        let rows: Vec<MemoryRow> = r.take(0).map_err(err)?;
        Ok(rows.into_iter().next().map(Memory::from))
    }

    /// Update mutable fields of an existing memory (content, type, importance,
    /// channel/source, forgotten) and bump `updated_at`.
    pub async fn update(&self, memory: &Memory) -> Result<()> {
        self.db
            .query(
                "UPDATE type::record('memory', $id) SET \
                 content=$content, memory_type=$mt, importance=$imp, \
                 source=$src, channel_id=$cid, forgotten=$forg, updated_at=time::now()",
            )
            .bind(("id", memory.id.clone()))
            .bind(("content", memory.content.clone()))
            .bind(("mt", memory.memory_type.to_string()))
            .bind(("imp", memory.importance as f64))
            .bind(("src", memory.source.clone()))
            .bind(("cid", memory.channel_id.clone()))
            .bind(("forg", memory.forgotten))
            .await
            .map_err(err)?
            .check()
            .map_err(err)?;
        Ok(())
    }

    /// Update only the embedding for a memory.
    pub async fn set_embedding_by_id(&self, id: &str, embedding: &[f32]) -> Result<()> {
        self.db
            .query("UPDATE type::record('memory', $id) SET embedding = $emb")
            .bind(("id", id.to_string()))
            .bind(("emb", embedding.to_vec()))
            .await
            .map_err(err)?
            .check()
            .map_err(err)?;
        Ok(())
    }

    /// Hard delete a memory and its edges.
    pub async fn delete(&self, id: &str) -> Result<()> {
        // Deleting the record also drops incident graph edges in SurrealDB.
        self.db
            .query("DELETE type::record('memory', $id)")
            .bind(("id", id.to_string()))
            .await
            .map_err(err)?
            .check()
            .map_err(err)?;
        Ok(())
    }

    /// Soft delete. Returns whether the row existed.
    pub async fn forget(&self, id: &str) -> Result<bool> {
        let existed = self.load(id).await?.is_some();
        self.db
            .query("UPDATE type::record('memory', $id) SET forgotten = true")
            .bind(("id", id.to_string()))
            .await
            .map_err(err)?
            .check()
            .map_err(err)?;
        Ok(existed)
    }

    /// Increment access count and bump `last_accessed_at`.
    pub async fn record_access(&self, id: &str) -> Result<()> {
        self.db
            .query(
                "UPDATE type::record('memory', $id) SET \
                 access_count += 1, last_accessed_at = time::now()",
            )
            .bind(("id", id.to_string()))
            .await
            .map_err(err)?
            .check()
            .map_err(err)?;
        Ok(())
    }

    // ---- associations / graph ----

    pub async fn create_association(&self, association: &Association) -> Result<()> {
        // RELATE rejects type::record(...) endpoints — bind RecordId values.
        let source = RecordId::new("memory", association.source_id.clone());
        let target = RecordId::new("memory", association.target_id.clone());
        self.db
            .query("RELATE $s->relates->$t SET relation_type = $rt, weight = $w")
            .bind(("s", source))
            .bind(("t", target))
            .bind(("rt", association.relation_type.to_string()))
            .bind(("w", association.weight as f64))
            .await
            .map_err(err)?
            .check()
            .map_err(err)?;
        Ok(())
    }

    /// All associations touching a memory (either direction).
    pub async fn get_associations(&self, memory_id: &str) -> Result<Vec<Association>> {
        let mut r = self
            .db
            .query(
                "SELECT meta::id(in) AS source, meta::id(out) AS target, relation_type, weight, \
                 created_at FROM relates \
                 WHERE in = type::record('memory', $m) OR out = type::record('memory', $m)",
            )
            .bind(("m", memory_id.to_string()))
            .await
            .map_err(err)?;
        let rows: Vec<AssocRow> = r.take(0).map_err(err)?;
        Ok(rows.into_iter().map(Association::from).collect())
    }

    /// Associations where both endpoints are within `ids` (internal edges only).
    pub async fn get_associations_between(&self, ids: &[String]) -> Result<Vec<Association>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let recs: Vec<RecordId> = ids
            .iter()
            .map(|s| RecordId::new("memory", s.clone()))
            .collect();
        let mut r = self
            .db
            .query(
                "SELECT meta::id(in) AS source, meta::id(out) AS target, relation_type, weight, \
                 created_at FROM relates WHERE in IN $ids AND out IN $ids",
            )
            .bind(("ids", recs))
            .await
            .map_err(err)?;
        let rows: Vec<AssocRow> = r.take(0).map_err(err)?;
        Ok(rows.into_iter().map(Association::from).collect())
    }

    /// Delete all edges incident to a memory. Returns the number removed.
    pub async fn delete_associations_for_memory(&self, memory_id: &str) -> Result<u64> {
        let before = self.get_associations(memory_id).await?.len() as u64;
        self.db
            .query(
                "DELETE relates WHERE in = type::record('memory', $m) \
                 OR out = type::record('memory', $m)",
            )
            .bind(("m", memory_id.to_string()))
            .await
            .map_err(err)?
            .check()
            .map_err(err)?;
        Ok(before)
    }

    /// Graph-view neighbours up to `depth`, excluding `exclude_ids`.
    /// Returns `(memories, edges)` where:
    /// - `memories`: all non-forgotten nodes within `depth` hops (both directions),
    ///   excluding the start node and `exclude_ids`.
    /// - `edges`: all associations incident to the EXPANDED set
    ///   (`{root} ∪ nodes within depth−1 hops`), each appearing exactly once.
    ///
    /// Implemented via native SurrealDB `{..N+collect}` graph recursion (4 fixed
    /// queries regardless of graph size), replacing the previous N+1 BFS.
    ///
    /// # Forgotten-node strategy (b) — accepted behavioural delta
    ///
    /// The traversal is unconditional: `{..N+collect}` traverses through forgotten
    /// nodes. Forgotten nodes are excluded from returned `memories` only at the
    /// hydrate step (`WHERE forgotten = false`). This means nodes reachable
    /// exclusively via a forgotten intermediate node WILL appear in `memories` (they
    /// were unreachable in the old BFS, which skipped forgotten nodes). This is a
    /// benign superset for the graph-view API and is explicitly tested. If exact
    /// no-traverse-through-forgotten parity is ever required, strategy (a) —
    /// `->relates[WHERE out.forgotten=false]->memory` — was also verified as
    /// working on SurrealDB 3.1.x (see task-c1-report.md).
    ///
    /// # depth == 0
    ///
    /// Returns `([], [])` immediately. Do NOT clamp to 1; `{..0}` is illegal
    /// SurrealQL ("Found 0 for bound but expected at least 1"). The upper bound
    /// is clamped to 256 (SurrealDB 3.1.x limit).
    pub async fn get_neighbors(
        &self,
        memory_id: &str,
        depth: u32,
        exclude_ids: &[String],
    ) -> Result<(Vec<Memory>, Vec<Association>)> {
        // Guard: depth 0 → nothing to expand; {..0} is illegal SurrealQL.
        if depth == 0 {
            return Ok((vec![], vec![]));
        }
        let depth_clamped = depth.min(256) as usize;

        let root = RecordId::new("memory", memory_id.to_string());

        // ── Step 1: COLLECTED set ───────────────────────────────────────────
        // Forward and backward `{..depth+collect}` each return a flat array of
        // RecordIDs (not hydrated records). We union + dedup + exclude start and
        // exclude_ids.
        let fwd_sql = format!("$root.{{..{depth_clamped}+collect}}->relates->memory");
        let bwd_sql = format!("$root.{{..{depth_clamped}+collect}}<-relates<-memory");

        let mut fwd_r = self
            .db
            .query(&fwd_sql)
            .bind(("root", root.clone()))
            .await
            .map_err(err)?;
        let fwd_val: surrealdb::types::Value = fwd_r.take(0).map_err(err)?;

        let mut bwd_r = self
            .db
            .query(&bwd_sql)
            .bind(("root", root.clone()))
            .await
            .map_err(err)?;
        let bwd_val: surrealdb::types::Value = bwd_r.take(0).map_err(err)?;

        // Seed the seen-set with start + excludes so they are never added to
        // collected_rids.
        let mut seen: HashSet<String> = exclude_ids.iter().cloned().collect();
        seen.insert(memory_id.to_string());
        let mut collected_rids: Vec<RecordId> = Vec::new();
        extract_rids_into(&fwd_val, &mut seen, &mut collected_rids);
        extract_rids_into(&bwd_val, &mut seen, &mut collected_rids);

        // ── Step 2: Hydrate memories (non-forgotten) ───────────────────────
        let memories = if collected_rids.is_empty() {
            vec![]
        } else {
            let sql = format!(
                "SELECT {MEMORY_COLS} FROM memory WHERE id IN $ids AND forgotten = false"
            );
            let mut r = self
                .db
                .query(sql)
                .bind(("ids", collected_rids))
                .await
                .map_err(err)?;
            let rows: Vec<MemoryRow> = r.take(0).map_err(err)?;
            rows.into_iter().map(Memory::from).collect()
        };

        // ── Step 3: EXPANDED set ───────────────────────────────────────────
        // EXPANDED = {root} ∪ nodes within (depth−1) hops (both directions).
        // For depth == 1, EXPANDED = {root} only (skip inner collect;
        // {..0} is illegal).
        let mut expanded_rids: Vec<RecordId> = vec![root.clone()];
        if depth_clamped >= 2 {
            let exp_depth = depth_clamped - 1;
            let ef_sql = format!("$root.{{..{exp_depth}+collect}}->relates->memory");
            let eb_sql = format!("$root.{{..{exp_depth}+collect}}<-relates<-memory");

            let mut ef_r = self
                .db
                .query(&ef_sql)
                .bind(("root", root.clone()))
                .await
                .map_err(err)?;
            let ef_val: surrealdb::types::Value = ef_r.take(0).map_err(err)?;

            let mut eb_r = self
                .db
                .query(&eb_sql)
                .bind(("root", root.clone()))
                .await
                .map_err(err)?;
            let eb_val: surrealdb::types::Value = eb_r.take(0).map_err(err)?;

            // Use a separate seen-set for expanded — root is pre-seeded.
            let mut exp_seen: HashSet<String> = HashSet::new();
            exp_seen.insert(memory_id.to_string());
            extract_rids_into(&ef_val, &mut exp_seen, &mut expanded_rids);
            extract_rids_into(&eb_val, &mut exp_seen, &mut expanded_rids);
        }

        // ── Step 4: Edges incident to EXPANDED set ─────────────────────────
        // Each edge appears exactly once (SELECT is idempotent; old BFS could
        // push duplicates when the same edge was seen from both endpoints).
        let edges = {
            let mut r = self
                .db
                .query(
                    "SELECT meta::id(in) AS source, meta::id(out) AS target, \
                     relation_type, weight, created_at \
                     FROM relates WHERE in IN $expanded OR out IN $expanded",
                )
                .bind(("expanded", expanded_rids))
                .await
                .map_err(err)?;
            let rows: Vec<AssocRow> = r.take(0).map_err(err)?;
            rows.into_iter().map(Association::from).collect()
        };

        Ok((memories, edges))
    }

    // ---- sorted / typed reads ----

    pub async fn get_by_type(&self, memory_type: MemoryType, limit: i64) -> Result<Vec<Memory>> {
        self.get_sorted(SearchSort::Recent, limit, Some(memory_type))
            .await
    }

    pub async fn get_high_importance(&self, threshold: f32, limit: i64) -> Result<Vec<Memory>> {
        let sql = format!(
            "SELECT {MEMORY_COLS} FROM memory \
             WHERE forgotten = false AND importance >= $th \
             ORDER BY importance DESC LIMIT {}",
            limit.max(0)
        );
        let mut r = self
            .db
            .query(sql)
            .bind(("th", threshold as f64))
            .await
            .map_err(err)?;
        let rows: Vec<MemoryRow> = r.take(0).map_err(err)?;
        Ok(rows.into_iter().map(Memory::from).collect())
    }

    pub async fn get_sorted(
        &self,
        sort: SearchSort,
        limit: i64,
        memory_type: Option<MemoryType>,
    ) -> Result<Vec<Memory>> {
        let order = match sort {
            SearchSort::Recent => "ORDER BY created_at DESC",
            SearchSort::Importance => "ORDER BY importance DESC, created_at DESC",
            SearchSort::MostAccessed => "ORDER BY access_count DESC, created_at DESC",
        };
        let type_clause = if memory_type.is_some() {
            "AND memory_type = $mt"
        } else {
            ""
        };
        let sql = format!(
            "SELECT {MEMORY_COLS} FROM memory \
             WHERE forgotten = false {type_clause} {order} LIMIT {}",
            limit.max(0)
        );
        let mut q = self.db.query(sql);
        if let Some(t) = memory_type {
            q = q.bind(("mt", t.to_string()));
        }
        let mut r = q.await.map_err(err)?;
        let rows: Vec<MemoryRow> = r.take(0).map_err(err)?;
        Ok(rows.into_iter().map(Memory::from).collect())
    }

    /// All non-forgotten memories, most-recent first, up to `limit`.
    /// Used by maintenance (merge candidate scan).
    pub async fn get_all_active(&self, limit: i64) -> Result<Vec<Memory>> {
        self.get_sorted(SearchSort::Recent, limit, None).await
    }

    /// Server-side prune: delete non-identity memories below `threshold` that are
    /// older than `older_than`. Returns the number deleted. Edges cascade with
    /// the record. Replaces a client-side scan-and-filter.
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
            .bind(("cut", Datetime::from(older_than)))
            .await
            .map_err(err)?;
        let deleted: surrealdb::types::Value = r.take(0).map_err(err)?;
        let n = if let surrealdb::types::Value::Array(a) = deleted {
            a.len() as u64
        } else {
            0
        };
        Ok(n)
    }

    /// Merge `loser` into `survivor` **atomically** (one BEGIN/COMMIT
    /// transaction — SurrealDB rolls it all back on any failure). Sets the
    /// survivor's content/embedding, rewires the loser's edges onto the survivor
    /// (preserving direction/type/weight; no self-loops; pre-deleting any
    /// conflicting survivor edge to satisfy the UNIQUE index), drops the loser's
    /// edges, adds `survivor->updates->loser`, and soft-deletes the loser.
    ///
    /// This is the SurrealDB equivalent of `store.rs::merge_memories_atomic` plus
    /// the embedding fix-up that `maintenance.rs::merge_pair` does *outside* the
    /// SQLite transaction — here the embedding lives on the record, so it is part
    /// of the same atomic operation.
    pub async fn merge(
        &self,
        survivor_id: &str,
        loser_id: &str,
        new_content: &str,
        new_embedding: Option<&[f32]>,
    ) -> Result<()> {
        // Read the loser's edges first, then compute the rewired set in Rust.
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
            .bind(("emb", new_embedding.map(<[f32]>::to_vec)));
        for (i, (s, t, rt, w)) in rewires.into_iter().enumerate() {
            q = q
                .bind((format!("s{i}"), RecordId::new("memory", s)))
                .bind((format!("t{i}"), RecordId::new("memory", t)))
                .bind((format!("rt{i}"), rt.to_string()))
                .bind((format!("w{i}"), w as f64));
        }
        q.await.map_err(err)?.check().map_err(err)?;
        Ok(())
    }

    // ---- vector + full-text (replaces lance.rs) ----

    /// KNN by cosine distance. Returns (memory_id, distance) ascending.
    /// K and EF are integer literals (bound params are not allowed in `<|..|>`).
    pub async fn vector_search(
        &self,
        query_embedding: &[f32],
        limit: usize,
    ) -> Result<Vec<(String, f32)>> {
        if query_embedding.len() != self.dim {
            return Err(err(format!(
                "embedding dim mismatch: expected {}, got {}",
                self.dim,
                query_embedding.len()
            )));
        }
        // EF≥80 required: benchmark (C5) shows EF=40 has a severe tail-latency
        // pathology (~14s p99 vs ~9ms p50) at 10k-vector 384-dim corpus, even
        // though recall@10 = 1.0. EF=80 eliminates the tail entirely (p95≈p50).
        // Recommended formula: (limit*4).max(80).
        let ef = (limit * 4).max(80);
        let sql = format!(
            "SELECT meta::id(id) AS id, vector::distance::knn() AS distance FROM memory \
             WHERE embedding <|{limit},{ef}|> $q AND forgotten = false ORDER BY distance"
        );
        let mut r = self
            .db
            .query(sql)
            .bind(("q", query_embedding.to_vec()))
            .await
            .map_err(err)?;
        let rows: Vec<IdDist> = r.take(0).map_err(err)?;
        Ok(rows
            .into_iter()
            .map(|x| (x.id, x.distance as f32))
            .collect())
    }

    /// Full-text BM25 search on `content`. Returns (memory_id, score) desc.
    pub async fn text_search(&self, query: &str, limit: usize) -> Result<Vec<(String, f32)>> {
        let sql = format!(
            "SELECT meta::id(id) AS id, search::score(0) AS score FROM memory \
             WHERE content @0@ $q AND forgotten = false ORDER BY score DESC LIMIT {limit}"
        );
        let mut r = self
            .db
            .query(sql)
            .bind(("q", query.to_string()))
            .await
            .map_err(err)?;
        let rows: Vec<IdScore> = r.take(0).map_err(err)?;
        Ok(rows.into_iter().map(|x| (x.id, x.score as f32)).collect())
    }

    /// Memories similar to an existing memory's own embedding, excluding self.
    /// Returns (memory_id, similarity = 1 - distance) >= threshold.
    pub async fn find_similar(
        &self,
        memory_id: &str,
        threshold: f32,
        limit: usize,
    ) -> Result<Vec<(String, f32)>> {
        let fetch = limit + 1;
        // EF≥80 minimum — see vector_search comment (C5 benchmark result).
        let ef = (fetch * 4).max(80);
        let sql = format!(
            "LET $vec = (SELECT VALUE embedding FROM ONLY type::record('memory', $id));\
             SELECT meta::id(id) AS id, vector::distance::knn() AS distance FROM memory \
             WHERE embedding <|{fetch},{ef}|> $vec AND forgotten = false ORDER BY distance"
        );
        let mut r = self
            .db
            .query(sql)
            .bind(("id", memory_id.to_string()))
            .await
            .map_err(err)?;
        // Statement 0 is the LET; statement 1 is the SELECT.
        let rows: Vec<IdDist> = r.take(1).map_err(err)?;
        let mut out = Vec::new();
        for x in rows {
            if x.id == memory_id {
                continue;
            }
            let similarity = 1.0 - x.distance as f32;
            if similarity >= threshold {
                out.push((x.id, similarity));
            }
            if out.len() >= limit {
                break;
            }
        }
        Ok(out)
    }
}

impl std::fmt::Debug for SurrealMemoryStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SurrealMemoryStore")
            .field("agent_id", &self.agent_id)
            .field("dim", &self.dim)
            .finish_non_exhaustive()
    }
}

#[async_trait::async_trait]
impl crate::memory::backend::MemoryBackend for SurrealMemoryStore {
    fn agent_id(&self) -> &str {
        self.agent_id()
    }
    async fn save(&self, m: &Memory, e: Option<&[f32]>) -> Result<()> {
        self.save(m, e).await
    }
    async fn set_embedding(&self, memory: &Memory, e: &[f32]) -> Result<()> {
        self.set_embedding_by_id(&memory.id, e).await
    }
    async fn delete(&self, id: &str) -> Result<()> {
        self.delete(id).await
    }
    async fn load(&self, id: &str) -> Result<Option<Memory>> {
        self.load(id).await
    }
    async fn update(&self, m: &Memory) -> Result<()> {
        self.update(m).await
    }
    async fn forget(&self, id: &str) -> Result<bool> {
        self.forget(id).await
    }
    async fn record_access(&self, id: &str) -> Result<()> {
        self.record_access(id).await
    }
    async fn get_by_type(&self, t: MemoryType, l: i64) -> Result<Vec<Memory>> {
        self.get_by_type(t, l).await
    }
    async fn get_high_importance(&self, th: f32, l: i64) -> Result<Vec<Memory>> {
        self.get_high_importance(th, l).await
    }
    async fn get_sorted(
        &self,
        s: SearchSort,
        l: i64,
        t: Option<MemoryType>,
    ) -> Result<Vec<Memory>> {
        self.get_sorted(s, l, t).await
    }
    async fn create_association(&self, a: &Association) -> Result<()> {
        self.create_association(a).await
    }
    async fn get_associations(&self, id: &str) -> Result<Vec<Association>> {
        self.get_associations(id).await
    }
    async fn get_associations_between(&self, ids: &[String]) -> Result<Vec<Association>> {
        self.get_associations_between(ids).await
    }
    async fn delete_associations_for_memory(&self, id: &str) -> Result<u64> {
        self.delete_associations_for_memory(id).await
    }
    async fn get_neighbors(
        &self,
        id: &str,
        d: u32,
        ex: &[String],
    ) -> Result<(Vec<Memory>, Vec<Association>)> {
        self.get_neighbors(id, d, ex).await
    }
    async fn vector_search(&self, q: &[f32], l: usize) -> Result<Vec<(String, f32)>> {
        self.vector_search(q, l).await
    }
    async fn text_search(&self, q: &str, l: usize) -> Result<Vec<(String, f32)>> {
        self.text_search(q, l).await
    }
    async fn find_similar(&self, id: &str, th: f32, l: usize) -> Result<Vec<(String, f32)>> {
        self.find_similar(id, th, l).await
    }
    async fn prune_below(&self, th: f32, older: chrono::DateTime<chrono::Utc>) -> Result<u64> {
        self.prune_below(th, older).await
    }
    async fn merge(&self, s: &str, l: &str, c: &str, e: Option<&[f32]>) -> Result<()> {
        self.merge(s, l, c, e).await
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use surrealdb::Surreal;
    use surrealdb::engine::local::Mem;

    const DIM: usize = 4;

    async fn mem_store() -> Arc<SurrealMemoryStore> {
        let db = Surreal::new::<Mem>(()).await.unwrap();
        db.use_ns("test").use_db("test").await.unwrap();
        SurrealMemoryStore::from_handle(db, "test-agent", DIM)
            .await
            .unwrap()
    }

    fn mem(id: &str) -> Memory {
        Memory {
            id: id.to_string(),
            content: format!("content of {id}"),
            memory_type: MemoryType::Fact,
            importance: 0.5,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            last_accessed_at: chrono::Utc::now(),
            access_count: 0,
            source: None,
            channel_id: None,
            forgotten: false,
        }
    }

    fn mem_forgotten(id: &str) -> Memory {
        Memory { forgotten: true, ..mem(id) }
    }

    /// Collect memory ids from the result, sorted for deterministic comparison.
    fn mem_ids(memories: &[Memory]) -> Vec<String> {
        let mut ids: Vec<_> = memories.iter().map(|m| m.id.clone()).collect();
        ids.sort();
        ids
    }

    /// Collect edge (source, target) pairs, sorted.
    fn edge_pairs(edges: &[Association]) -> Vec<(String, String)> {
        let mut pairs: Vec<_> = edges
            .iter()
            .map(|e| (e.source_id.clone(), e.target_id.clone()))
            .collect();
        pairs.sort();
        pairs
    }

    #[tokio::test]
    async fn associations_between_returns_only_internal_edges() {
        let store = mem_store().await;
        let a = mem("a");
        let b = mem("b");
        let c = mem("c");
        for m in [&a, &b, &c] {
            store.save(m, None).await.unwrap();
        }
        store
            .create_association(&Association::new(&a.id, &b.id, RelationType::RelatedTo))
            .await
            .unwrap();
        store
            .create_association(&Association::new(&b.id, &c.id, RelationType::RelatedTo))
            .await
            .unwrap();
        let within = store
            .get_associations_between(&[a.id.clone(), b.id.clone()])
            .await
            .unwrap();
        assert_eq!(within.len(), 1); // a->b only; b->c excluded (c not in set)
    }

    // ── get_neighbors parity tests (C2 — native {..+collect} recursion) ────────

    /// depth 0 → always returns empty regardless of graph.
    /// ({..0} is illegal SurrealQL; must guard before issuing any query.)
    #[tokio::test]
    async fn get_neighbors_depth_zero_returns_empty() {
        let store = mem_store().await;
        let a = mem("gn-d0-a");
        let b = mem("gn-d0-b");
        for m in [&a, &b] {
            store.save(m, None).await.unwrap();
        }
        store
            .create_association(&Association::new(&a.id, &b.id, RelationType::RelatedTo))
            .await
            .unwrap();

        let (mems, edges) = store.get_neighbors(&a.id, 0, &[]).await.unwrap();
        assert!(mems.is_empty(), "depth=0 must return no memories");
        assert!(edges.is_empty(), "depth=0 must return no edges");
    }

    /// depth 1 — only immediate neighbours of root (both directions), no multi-hop.
    /// Graph: a→b, a→c (root = a, depth 1 → should collect {b, c}).
    /// Edges: incident to expanded={a} → {a→b, a→c}.
    #[tokio::test]
    async fn get_neighbors_depth_one_collects_direct_neighbors() {
        let store = mem_store().await;
        let a = mem("gn-d1-a");
        let b = mem("gn-d1-b");
        let c = mem("gn-d1-c");
        let d = mem("gn-d1-d"); // b→d is depth-2; must NOT appear at depth=1
        for m in [&a, &b, &c, &d] {
            store.save(m, None).await.unwrap();
        }
        store
            .create_association(&Association::new(&a.id, &b.id, RelationType::RelatedTo))
            .await
            .unwrap();
        store
            .create_association(&Association::new(&a.id, &c.id, RelationType::RelatedTo))
            .await
            .unwrap();
        store
            .create_association(&Association::new(&b.id, &d.id, RelationType::RelatedTo))
            .await
            .unwrap();

        let (mems, edges) = store.get_neighbors(&a.id, 1, &[]).await.unwrap();
        let returned_ids = mem_ids(&mems);
        assert_eq!(returned_ids, vec!["gn-d1-b", "gn-d1-c"],
            "depth=1 must collect exactly b and c (direct neighbours of a)");
        assert!(!returned_ids.contains(&"gn-d1-a".to_string()),
            "start node must not appear in returned memories");
        assert!(!returned_ids.contains(&"gn-d1-d".to_string()),
            "d is 2 hops away; must not appear at depth=1");

        // Edges from EXPANDED = {a}: a→b and a→c.
        let ep = edge_pairs(&edges);
        assert_eq!(ep.len(), 2, "depth=1 edges: only the 2 edges incident to root");
        assert!(ep.contains(&("gn-d1-a".to_string(), "gn-d1-b".to_string())));
        assert!(ep.contains(&("gn-d1-a".to_string(), "gn-d1-c".to_string())));
    }

    /// depth 2, multi-hop + both directions.
    /// Graph: a→b→c, d→a (backward from a). Root=a, depth=2.
    /// COLLECTED: forward {b,c}, backward {d} → {b,c,d}.
    /// EXPANDED: {a} ∪ {b,d} (depth-1 collect) → {a,b,d}.
    /// Edges incident to {a,b,d}: a→b, b→c, d→a.
    #[tokio::test]
    async fn get_neighbors_depth_two_multi_hop_and_bidirectional() {
        let store = mem_store().await;
        let a = mem("gn-d2-a");
        let b = mem("gn-d2-b");
        let c = mem("gn-d2-c");
        let d = mem("gn-d2-d");
        for m in [&a, &b, &c, &d] {
            store.save(m, None).await.unwrap();
        }
        store
            .create_association(&Association::new(&a.id, &b.id, RelationType::RelatedTo))
            .await
            .unwrap();
        store
            .create_association(&Association::new(&b.id, &c.id, RelationType::RelatedTo))
            .await
            .unwrap();
        store
            .create_association(&Association::new(&d.id, &a.id, RelationType::RelatedTo))
            .await
            .unwrap();

        let (mems, edges) = store.get_neighbors(&a.id, 2, &[]).await.unwrap();
        let returned_ids = mem_ids(&mems);
        assert_eq!(returned_ids, vec!["gn-d2-b", "gn-d2-c", "gn-d2-d"],
            "depth=2 must collect b (forward-1), c (forward-2), d (backward-1)");
        assert!(!returned_ids.contains(&"gn-d2-a".to_string()),
            "start node must be excluded from memories");

        let ep = edge_pairs(&edges);
        // Expanded = {a, b, d}: edges incident to those nodes.
        // a→b (a expanded), b→c (b expanded), d→a (d expanded).
        assert_eq!(ep.len(), 3, "3 distinct edges incident to expanded set {{a,b,d}}");
        assert!(ep.contains(&("gn-d2-a".to_string(), "gn-d2-b".to_string())));
        assert!(ep.contains(&("gn-d2-b".to_string(), "gn-d2-c".to_string())));
        assert!(ep.contains(&("gn-d2-d".to_string(), "gn-d2-a".to_string())));

        // Verify no duplicate edges (native SELECT is idempotent; old BFS could duplicate).
        let mut deduped = ep.clone();
        deduped.dedup();
        assert_eq!(ep.len(), deduped.len(), "edges must not be duplicated");
    }

    /// Excluded node: memories in exclude_ids must not appear in results.
    /// Graph: a→b→c, a→excl→e. Root=a, depth=2, exclude=[excl].
    /// COLLECTED candidates include excl and e, but excl is excluded by dedup/seed;
    /// e remains reachable only via the excluded excl node's traversal path — however
    /// under strategy (b) (unconditional traversal) e IS reachable via the native
    /// traversal through excl. Under BFS, excl is pre-seeded in `visited` so e is
    /// never enqueued. We test the native rule: exclude_ids are excluded from the
    /// RETURNED memories; `e` reachability depends on traversal strategy.
    /// Core assertion: excl must never appear in returned memories.
    #[tokio::test]
    async fn get_neighbors_excludes_specified_nodes() {
        let store = mem_store().await;
        let a = mem("gn-ex-a");
        let b = mem("gn-ex-b");
        let c = mem("gn-ex-c");
        let excl = mem("gn-ex-excl");
        for m in [&a, &b, &c, &excl] {
            store.save(m, None).await.unwrap();
        }
        store
            .create_association(&Association::new(&a.id, &b.id, RelationType::RelatedTo))
            .await
            .unwrap();
        store
            .create_association(&Association::new(&b.id, &c.id, RelationType::RelatedTo))
            .await
            .unwrap();
        store
            .create_association(&Association::new(&a.id, &excl.id, RelationType::RelatedTo))
            .await
            .unwrap();

        let (mems, _edges) = store
            .get_neighbors(&a.id, 2, std::slice::from_ref(&excl.id))
            .await
            .unwrap();
        let returned_ids = mem_ids(&mems);
        assert!(
            !returned_ids.contains(&excl.id),
            "excluded node must never appear in returned memories"
        );
        assert!(
            !returned_ids.contains(&a.id),
            "start node must never appear in returned memories"
        );
        // b and c are reachable without going through excl.
        assert!(returned_ids.contains(&b.id), "b must be returned (not excluded)");
        assert!(returned_ids.contains(&c.id), "c must be returned (not excluded)");
    }

    /// Forgotten node — strategy (b) documented delta.
    ///
    /// Graph: a→b→forg→d. `forg` is forgotten. Root=a, depth=3.
    ///
    /// BFS behaviour: forg is never enqueued (forgotten check), so d is unreachable.
    /// BFS returns: memories={b}, edges={a→b, b→forg}.
    ///
    /// Native strategy (b): traversal is unconditional — `{..3+collect}` goes
    /// through forg; d IS included in the raw COLLECTED set. The hydrate
    /// `WHERE forgotten = false` excludes forg from returned memories, but d
    /// (non-forgotten) IS returned. This is the documented accepted delta:
    /// the native reachable set is a SUPERSET of the BFS reachable set when
    /// forgotten nodes exist on paths.
    ///
    /// Asserted: forg NOT in returned memories (excluded by hydrate).
    /// Asserted: d IS in returned memories (native superset — not present in old BFS).
    /// Asserted: edges include b→forg (forg is in EXPANDED at depth 2).
    #[tokio::test]
    async fn get_neighbors_forgotten_strategy_b_superset() {
        let store = mem_store().await;
        let a = mem("gn-fg-a");
        let b = mem("gn-fg-b");
        let forg = mem_forgotten("gn-fg-forg"); // forgotten
        let d = mem("gn-fg-d");
        for m in [&a, &b, &forg, &d] {
            store.save(m, None).await.unwrap();
        }
        store
            .create_association(&Association::new(&a.id, &b.id, RelationType::RelatedTo))
            .await
            .unwrap();
        store
            .create_association(&Association::new(&b.id, &forg.id, RelationType::RelatedTo))
            .await
            .unwrap();
        store
            .create_association(&Association::new(&forg.id, &d.id, RelationType::RelatedTo))
            .await
            .unwrap();

        let (mems, edges) = store.get_neighbors(&a.id, 3, &[]).await.unwrap();
        let returned_ids = mem_ids(&mems);

        // forg must be excluded by hydrate (WHERE forgotten = false).
        assert!(
            !returned_ids.contains(&forg.id),
            "forgotten node must not appear in returned memories"
        );
        // b is returned normally.
        assert!(returned_ids.contains(&b.id), "b must be returned");
        // d IS returned under strategy (b): native traverses through forg.
        // This is the documented delta vs old BFS (which would NOT return d).
        assert!(
            returned_ids.contains(&d.id),
            "strategy (b): d must be returned — native traverses through forgotten forg"
        );

        // Edges: b→forg should be present (forg is in expanded set at depth 2).
        let ep = edge_pairs(&edges);
        assert!(
            ep.contains(&(b.id.clone(), forg.id.clone())),
            "b→forg edge must be present (b is in expanded set)"
        );
    }

    // ── C3: snowball(english) stemming tests ─────────────────────────────────

    /// Verify that `snowball(english)` stemming is active: a memory whose content
    /// contains "running" must be matched by the stem query "run".
    ///
    /// An un-stemmed analyzer (lowercase + ascii only) would NOT match "run"
    /// against "running" because they are distinct tokens after tokenisation.
    /// With snowball(english) both are reduced to the stem "run", so BM25 finds
    /// the match.
    ///
    /// This test will fail (0 results) against the old analyzer without stemming,
    /// confirming the regression guard.
    #[tokio::test]
    async fn fts_snowball_stem_running_matches_run() {
        let store = mem_store().await;
        // Save a memory with the inflected form "running".
        let mut m = mem("stem-running");
        m.content = "running quickly through the park".to_string();
        store.save(&m, None).await.unwrap();

        // Query with the bare stem "run" — must match due to snowball(english).
        let results = store.text_search("run", 10).await.unwrap();
        let ids: Vec<_> = results.iter().map(|(id, _)| id.as_str()).collect();
        assert!(
            ids.contains(&"stem-running"),
            "snowball(english): stem query 'run' must match memory with content 'running' \
             (would miss with un-stemmed analyzer)"
        );
    }

    /// Verify plural-to-singular stemming: "databases" is stemmed to "databas"
    /// (the Porter stem) so searching "database" should also match.
    ///
    /// Note: Porter stem of "database" and "databases" converges to "databas",
    /// so both queries hit the same indexed token.
    #[tokio::test]
    async fn fts_snowball_stem_databases_matches_database() {
        let store = mem_store().await;
        let mut m = mem("stem-databases");
        m.content = "relational databases store structured data".to_string();
        store.save(&m, None).await.unwrap();

        // Query with the singular form — both share the Porter stem "databas".
        let results = store.text_search("database", 10).await.unwrap();
        let ids: Vec<_> = results.iter().map(|(id, _)| id.as_str()).collect();
        assert!(
            ids.contains(&"stem-databases"),
            "snowball(english): 'database' query must match memory with 'databases' \
             (would miss with un-stemmed analyzer)"
        );
    }
}
