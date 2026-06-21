//! Experimental SurrealDB-backed memory store (feature `surreal-memory`).
//!
//! A single embedded SurrealDB instance that unifies what `store.rs` (SQLite)
//! and `lance.rs` (LanceDB) do today: memory records, graph associations
//! (`RELATE` edges), vector KNN, and full-text search live on one `memory`
//! table. Embeddings are still generated externally (`embedding.rs` / fastembed)
//! and passed in.
//!
//! Design + rationale: `docs/design-docs/surrealdb-memory-backend.md`. The query
//! shapes here were validated against embedded SurrealDB 3.1.5 in
//! `spikes/surreal-memory/` (a runnable, tested reference port).
//!
//! Phase 1 scope: the store + vector/FTS primitives. Hybrid RRF search (Phase 2)
//! still lives in `search.rs`; this type exposes the same primitives it needs.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::Arc;

use surrealdb::engine::local::{Db, SurrealKv};
use surrealdb::types::{Datetime, RecordId, SurrealValue};
use surrealdb::Surreal;

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
        db.use_ns("spacebot").use_db(agent_id.as_str()).await.map_err(err)?;
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
        let store = Self { db, agent_id: agent_id.into(), dim };
        store.define_schema().await?;
        Ok(Arc::new(store))
    }

    pub fn agent_id(&self) -> &str {
        &self.agent_id
    }

    /// Apply the schema. Idempotent (`IF NOT EXISTS`).
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
             DEFINE ANALYZER IF NOT EXISTS memory_an TOKENIZERS class FILTERS lowercase, ascii;\
             DEFINE INDEX IF NOT EXISTS memory_fts ON memory FIELDS content FULLTEXT ANALYZER memory_an BM25;\
             DEFINE INDEX IF NOT EXISTS memory_type_idx ON memory FIELDS memory_type;\
             DEFINE INDEX IF NOT EXISTS memory_importance_idx ON memory FIELDS importance;\
             DEFINE TABLE IF NOT EXISTS relates SCHEMAFULL TYPE RELATION FROM memory TO memory;\
             DEFINE FIELD IF NOT EXISTS relation_type ON relates TYPE string;\
             DEFINE FIELD IF NOT EXISTS weight ON relates TYPE float DEFAULT 0.5;\
             DEFINE FIELD IF NOT EXISTS created_at ON relates TYPE datetime DEFAULT time::now();\
             DEFINE INDEX IF NOT EXISTS relates_unique ON relates FIELDS in, out, relation_type UNIQUE;"
        );
        self.db.query(sql).await.map_err(err)?.check().map_err(err)?;
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
        let mut r = self.db.query(sql).bind(("id", id.to_string())).await.map_err(err)?;
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
    pub async fn set_embedding(&self, id: &str, embedding: &[f32]) -> Result<()> {
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

    /// Graph-view BFS neighbours up to `depth`, excluding `exclude_ids`.
    /// Mirrors `store.rs::get_neighbors` — returns (memories, edges traversed).
    pub async fn get_neighbors(
        &self,
        memory_id: &str,
        depth: u32,
        exclude_ids: &[String],
    ) -> Result<(Vec<Memory>, Vec<Association>)> {
        let mut visited: std::collections::HashSet<String> =
            exclude_ids.iter().cloned().collect();
        visited.insert(memory_id.to_string());

        let mut memories = Vec::new();
        let mut edges = Vec::new();
        let mut queue: VecDeque<(String, u32)> = VecDeque::new();
        queue.push_back((memory_id.to_string(), 0));

        while let Some((current, d)) = queue.pop_front() {
            if d >= depth {
                continue;
            }
            for assoc in self.get_associations(&current).await? {
                let other = if assoc.source_id == current {
                    assoc.target_id.clone()
                } else {
                    assoc.source_id.clone()
                };
                edges.push(assoc);
                if !visited.insert(other.clone()) {
                    continue;
                }
                if let Some(m) = self.load(&other).await?
                    && !m.forgotten
                {
                    memories.push(m);
                    queue.push_back((other, d + 1));
                }
            }
        }
        Ok((memories, edges))
    }

    // ---- sorted / typed reads ----

    pub async fn get_by_type(&self, memory_type: MemoryType, limit: i64) -> Result<Vec<Memory>> {
        self.get_sorted(SearchSort::Recent, limit, Some(memory_type)).await
    }

    pub async fn get_high_importance(&self, threshold: f32, limit: i64) -> Result<Vec<Memory>> {
        let sql = format!(
            "SELECT {MEMORY_COLS} FROM memory \
             WHERE forgotten = false AND importance >= $th \
             ORDER BY importance DESC LIMIT {}",
            limit.max(0)
        );
        let mut r = self.db.query(sql).bind(("th", threshold as f64)).await.map_err(err)?;
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
        let ef = (limit * 4).max(40);
        let sql = format!(
            "SELECT meta::id(id) AS id, vector::distance::knn() AS distance FROM memory \
             WHERE embedding <|{limit},{ef}|> $q AND forgotten = false ORDER BY distance"
        );
        let mut r = self.db.query(sql).bind(("q", query_embedding.to_vec())).await.map_err(err)?;
        let rows: Vec<IdDist> = r.take(0).map_err(err)?;
        Ok(rows.into_iter().map(|x| (x.id, x.distance as f32)).collect())
    }

    /// Full-text BM25 search on `content`. Returns (memory_id, score) desc.
    pub async fn text_search(&self, query: &str, limit: usize) -> Result<Vec<(String, f32)>> {
        let sql = format!(
            "SELECT meta::id(id) AS id, search::score(0) AS score FROM memory \
             WHERE content @0@ $q AND forgotten = false ORDER BY score DESC LIMIT {limit}"
        );
        let mut r = self.db.query(sql).bind(("q", query.to_string())).await.map_err(err)?;
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
        let ef = (fetch * 4).max(40);
        let sql = format!(
            "LET $vec = (SELECT VALUE embedding FROM ONLY type::record('memory', $id));\
             SELECT meta::id(id) AS id, vector::distance::knn() AS distance FROM memory \
             WHERE embedding <|{fetch},{ef}|> $vec AND forgotten = false ORDER BY distance"
        );
        let mut r = self.db.query(sql).bind(("id", memory_id.to_string())).await.map_err(err)?;
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
