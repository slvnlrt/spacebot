//! Diagnostic: open a SurrealKV store directory and list namespaces/databases
//! with per-database memory/relates counts. Used to find where orphaned memories
//! actually live (which ns/db) after an agent was recreated.
//!
//! Usage: inspect_kv <path-to-surreal-dir>

use surrealdb::Surreal;
use surrealdb::engine::local::SurrealKv;

async fn count(db: &Surreal<surrealdb::engine::local::Db>, table: &str) -> String {
    match db
        .query(format!("SELECT count() AS c FROM {table} GROUP ALL"))
        .await
    {
        Ok(mut r) => match r.take::<Option<serde_json::Value>>(0) {
            Ok(Some(v)) => v
                .get("c")
                .map(|c| c.to_string())
                .unwrap_or_else(|| "0".into()),
            Ok(None) => "0".into(),
            Err(e) => format!("take-err: {e}"),
        },
        Err(e) => format!("query-err: {e}"),
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let path = std::env::args()
        .nth(1)
        .expect("usage: inspect_kv <surreal_dir>");
    let db = Surreal::new::<SurrealKv>(path.as_str()).await?;
    println!("opened: {path}");

    let mut res = db.query("INFO FOR ROOT").await?;
    let root: Option<serde_json::Value> = res.take(0)?;
    let namespaces: Vec<String> = root
        .as_ref()
        .and_then(|v| v.get("namespaces"))
        .and_then(|v| v.as_object())
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();
    println!("namespaces: {namespaces:?}");

    for ns in &namespaces {
        db.use_ns(ns.as_str()).await?;
        let mut res = db.query("INFO FOR NS").await?;
        let nsinfo: Option<serde_json::Value> = res.take(0)?;
        let dbs: Vec<String> = nsinfo
            .as_ref()
            .and_then(|v| v.get("databases"))
            .and_then(|v| v.as_object())
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default();
        println!("\nNS '{ns}' -> databases: {dbs:?}");
        for d in &dbs {
            db.use_ns(ns.as_str()).use_db(d.as_str()).await?;
            // Optionally replicate the daemon's open: run define_schema() DDL first.
            if std::env::var("SCHEMA").is_ok() {
                let dim = 384;
                let ddl = format!(
                    "DEFINE TABLE IF NOT EXISTS memory SCHEMAFULL;\
                     DEFINE FIELD IF NOT EXISTS content ON memory TYPE string;\
                     DEFINE FIELD IF NOT EXISTS memory_type ON memory TYPE string;\
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
                     DEFINE INDEX IF NOT EXISTS memory_fts ON memory FIELDS content FULLTEXT ANALYZER memory_an BM25;"
                );
                match db.query(ddl).await {
                    Ok(r) => match r.check() { Ok(_) => println!("   [define_schema] OK"), Err(e) => println!("   [define_schema] check ERROR: {e}") },
                    Err(e) => println!("   [define_schema] query ERROR: {e}"),
                }
            }
            if std::env::var("DUMP").is_ok() {
                if let Ok(mut s) = db.query("SELECT meta::id(id) AS id, forgotten, created_at FROM memory ORDER BY created_at ASC").await {
                    if let Ok(rows)=s.take::<Vec<serde_json::Value>>(0){
                        for r in rows { println!("DUMP {} forgotten={} {}", r.get("id").and_then(|v|v.as_str()).unwrap_or("?"), r.get("forgotten").and_then(|v|v.as_bool()).unwrap_or(false), r.get("created_at").and_then(|v|v.as_str()).unwrap_or("")); }
                    }
                }
            }
            let m = count(&db, "memory").await;
            let r = count(&db, "relates").await;
            let active = count(&db, "memory WHERE forgotten = false").await;
            let forgotten = count(&db, "memory WHERE forgotten = true").await;
            println!("   db '{d}': memory={m} (active={active}, forgotten={forgotten}) relates={r}");
            // Isolate what cuts 109 -> 11: run variants of the daemon query.
            let cols = "meta::id(id) AS id, content, memory_type, importance, created_at, updated_at, last_accessed_at, access_count, source, channel_id, forgotten";
            let variants = [
                ("id only, no order", "SELECT meta::id(id) AS id FROM memory WHERE forgotten = false LIMIT 200".to_string()),
                ("id only, ORDER created_at DESC", "SELECT meta::id(id) AS id FROM memory WHERE forgotten = false ORDER BY created_at DESC LIMIT 200".to_string()),
                ("FULL daemon cols + order", format!("SELECT {cols} FROM memory WHERE forgotten = false ORDER BY created_at DESC LIMIT 200")),
            ];
            for (label, sql) in &variants {
                match db.query(sql.as_str()).await {
                    Ok(mut s) => match s.take::<Vec<serde_json::Value>>(0) {
                        Ok(rows) => println!("   [{label}] rows = {}", rows.len()),
                        Err(e) => println!("   [{label}] take ERROR: {e}"),
                    },
                    Err(e) => println!("   [{label}] query ERROR: {e}"),
                }
            }
            // Which non-optional MemoryRow field is NULL/NONE among active rows?
            for f in ["content","memory_type","importance","created_at","updated_at","last_accessed_at","access_count","forgotten","embedding"] {
                let q = format!("SELECT count() AS c FROM memory WHERE forgotten=false AND {f} IS NONE GROUP ALL");
                if let Ok(mut s)=db.query(q).await { if let Ok(Some(v))=s.take::<Option<serde_json::Value>>(0){ let c=v.get("c").map(|x|x.to_string()).unwrap_or_default(); if c!="0"&&!c.is_empty(){ println!("   active rows with {f} IS NONE = {c}"); } } }
            }
            // sample one OLD (09-13h) active row, full shape
            if let Ok(mut s) = db.query("SELECT meta::id(id) AS id, content, memory_type, importance, created_at, updated_at, last_accessed_at, access_count, source, channel_id, forgotten FROM memory WHERE forgotten=false ORDER BY created_at ASC LIMIT 2").await {
                if let Ok(rows)=s.take::<Vec<serde_json::Value>>(0){ for r in rows { println!("   OLDEST active row: {r}"); } }
            }
        }
    }
    Ok(())
}
