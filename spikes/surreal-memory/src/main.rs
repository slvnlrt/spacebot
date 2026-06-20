//! Phase 0 spike: empirically validate SurrealDB v3.1.5 (embedded) for the
//! spacebot memory backend design. Answers the open questions from the design doc.
use surrealdb::Surreal;
use surrealdb::engine::local::SurrealKv;
use surrealdb::types::Value;

async fn q(db: &Surreal<impl surrealdb::Connection>, sql: &str) -> Result<Value, surrealdb::Error> {
    let mut r = db.query(sql).await?;
    let v: Value = r.take(0)?;
    Ok(v)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // ---- 0. Embedded engines: Mem + on-disk SurrealKv ----
    let tmp = std::env::temp_dir().join("surreal-spike-db");
    let _ = std::fs::remove_dir_all(&tmp);
    let db = Surreal::new::<SurrealKv>(tmp.to_str().unwrap()).await?;
    db.use_ns("spike").use_db("mem").await?;
    println!("== [0] embedded SurrealKv on disk: connected ==");

    // ---- 1. Schema ----
    let schema = r#"
        DEFINE TABLE memory SCHEMAFULL;
        DEFINE FIELD content   ON memory TYPE string;
        DEFINE FIELD forgotten ON memory TYPE bool DEFAULT false;
        DEFINE FIELD importance ON memory TYPE float DEFAULT 0.5;
        DEFINE FIELD embedding ON memory TYPE option<array<float>>;
        DEFINE INDEX memory_hnsw ON memory FIELDS embedding HNSW DIMENSION 4 TYPE F32 DIST COSINE;
        DEFINE ANALYZER memory_an TOKENIZERS class FILTERS lowercase, ascii;
        DEFINE INDEX memory_fts ON memory FIELDS content FULLTEXT ANALYZER memory_an BM25;
        DEFINE TABLE relates SCHEMAFULL TYPE RELATION FROM memory TO memory;
        DEFINE FIELD relation_type ON relates TYPE string;
        DEFINE FIELD weight ON relates TYPE float DEFAULT 0.5;
    "#;
    match db.query(schema).await {
        Ok(mut r) => { r.check()?; println!("== [1] schema (HNSW dim4 + FULLTEXT + RELATION): OK =="); }
        Err(e) => { println!("== [1] schema FAILED: {e} =="); return Ok(()); }
    }

    // ---- 2. Record id from UUID string (type::record vs raw) ----
    let uuid = "5e69aa12-2c96-4abc-9def-0123456789ab"; // hyphenated UUID v4
    let r = db.query("CREATE type::record('memory', $id) SET content=$c, embedding=$e, forgotten=false")
        .bind(("id", uuid.to_string()))
        .bind(("c", "type thing creation".to_string()))
        .bind(("e", vec![0.1f32,0.2,0.3,0.4]))
        .await;
    match r { Ok(mut r) => match r.check() { Ok(_) => println!("== [2a] CREATE type::record('memory',$uuid): OK =="), Err(e)=>println!("== [2a] type::record check err: {e} ==") }, Err(e)=>println!("== [2a] type::record err: {e} ==") }
    // read it back by the same string form
    let back = q(&db, &format!("SELECT VALUE meta::id(id) FROM memory:`{uuid}`")).await;
    println!("   [2b] read-back id via backtick literal: {back:?}");
    let back2 = db.query("SELECT VALUE meta::id(id) FROM type::record('memory',$id)").bind(("id", uuid.to_string())).await
        .and_then(|mut r| r.take::<Value>(0));
    println!("   [2c] read-back id via type::record param: {back2:?}");

    // ---- 3. Seed dataset: 60 memories, alternating forgotten, varied embeddings ----
    for i in 0..60i64 {
        let f = i % 2 == 0; // half forgotten
        let e = vec![ (i as f32)/60.0, ((60-i) as f32)/60.0, (i as f32 % 7.0)/7.0, 0.5f32 ];
        let id = format!("m{i:03}");
        db.query("CREATE type::record('memory',$id) SET content=$c, embedding=$e, forgotten=$f, importance=$imp")
            .bind(("id", id)).bind(("c", if i % 10 == 0 { format!("memory {i} about saturn rockets orbit") } else { format!("memory {i} about coffee and tea") }))
            .bind(("e", e)).bind(("f", f)).bind(("imp", (i as f64)/60.0))
            .await?.check()?;
    }
    let cnt = q(&db, "SELECT count() FROM memory GROUP ALL").await?;
    println!("== [3] seeded. total memory rows: {cnt:?} ==");

    // ---- 4. KNN unfiltered (literal K,EF) ----
    let qe = vec![0.5f32, 0.5, 0.3, 0.5];
    let knn = db.query("SELECT meta::id(id) AS id, forgotten, vector::distance::knn() AS dist FROM memory WHERE embedding <|5,40|> $q ORDER BY dist")
        .bind(("q", qe.clone())).await?.take::<Value>(0)?;
    println!("== [4] KNN unfiltered <|5,40|> -> {} ==", summarize(&knn));

    // ---- 5. THE #6949 TEST: KNN + filter forgotten=false (embedded) ----
    let knn_f = db.query("SELECT meta::id(id) AS id, forgotten, vector::distance::knn() AS dist FROM memory WHERE embedding <|5,40|> $q AND forgotten = false ORDER BY dist")
        .bind(("q", qe.clone())).await;
    match knn_f {
        Ok(mut r) => match r.take::<Value>(0) {
            Ok(v) => println!("== [5] *** FILTERED KNN (#6949 on EMBEDDED) <|5,40|> AND forgotten=false -> {} ***", summarize(&v)),
            Err(e) => println!("== [5] filtered KNN take err: {e} =="),
        },
        Err(e) => println!("== [5] filtered KNN query err: {e} =="),
    }

    // ---- 6. K/EF as bound params? ----
    let knn_p = db.query("SELECT meta::id(id) AS id FROM memory WHERE embedding <|$k,$ef|> $q")
        .bind(("q", qe.clone())).bind(("k", 5i64)).bind(("ef", 40i64)).await;
    match knn_p { Ok(mut r) => match r.take::<Value>(0) { Ok(v)=>println!("== [6] K/EF as params <|$k,$ef|>: OK -> {} ==", summarize(&v)), Err(e)=>println!("== [6] params take err: {e} ==") }, Err(e)=>println!("== [6] K/EF params NOT allowed: {e} ==") }

    // ---- 7. FTS ----
    let fts = db.query("SELECT meta::id(id) AS id, search::score(0) AS score FROM memory WHERE content @0@ $term ORDER BY score DESC LIMIT 5")
        .bind(("term","rockets".to_string())).await;
    match fts { Ok(mut r)=>match r.take::<Value>(0){Ok(v)=>println!("== [7] FTS content @0@ 'rockets' -> {} ==", summarize(&v)), Err(e)=>println!("== [7] FTS take err: {e} ==")}, Err(e)=>println!("== [7] FTS err: {e} ==") }

    // ---- 8. RELATE + traversal ----
    db.query("RELATE memory:m001->relates->memory:m002 SET relation_type='related_to', weight=0.7").await?.check()?;
    db.query("RELATE memory:m002->relates->memory:m003 SET relation_type='part_of', weight=0.6").await?.check()?;
    let trav1 = q(&db, "SELECT VALUE ->relates->memory FROM memory:m001").await;
    println!("== [8a] 1-hop ->relates->memory FROM m001: {trav1:?} ==");
    let trav_edges = q(&db, "SELECT ->relates.{out: out, relation_type, weight} AS edges FROM memory:m001").await;
    println!("== [8b] edge projection ->relates.{{out,relation_type,weight}}: {trav_edges:?} ==");
    let trav2 = q(&db, "SELECT VALUE ->relates->memory->relates->memory FROM memory:m001").await;
    println!("== [8c] 2-hop chain: {trav2:?} ==");

    // ---- 9. find_similar: self-referential KNN ----
    let fs = db.query("LET $vec = (SELECT VALUE embedding FROM ONLY memory:m010); SELECT meta::id(id) AS id, vector::distance::knn() AS dist FROM memory WHERE embedding <|5,40|> $vec AND id != memory:m010 ORDER BY dist").await;
    match fs { Ok(mut r)=> {
        // statement 0 is LET, statement 1 is SELECT
        match r.take::<Value>(1){Ok(v)=>println!("== [9] find_similar self-referential KNN -> {} ==", summarize(&v)), Err(e)=>println!("== [9] find_similar take err: {e} ==")}
    }, Err(e)=>println!("== [9] find_similar err: {e} ==") }

    // ---- 7b. FTS with discriminative term 'saturn' (only ~6 of 60 docs) ----
    let fts2 = db.query("SELECT meta::id(id) AS id, search::score(0) AS score FROM memory WHERE content @0@ $term ORDER BY score DESC LIMIT 10")
        .bind(("term","saturn".to_string())).await;
    match fts2 { Ok(mut r)=>match r.take::<Value>(0){Ok(v)=>println!("== [7b] FTS 'saturn' (discriminative) -> {} ==", summarize(&v)), Err(e)=>println!("== [7b] err {e} ==")}, Err(e)=>println!("== [7b] err {e} ==") }

    // ---- 10. Scale test for #6949: 1000 rows, k=10, filtered ----
    for i in 100..1100i64 {
        let f = i % 3 == 0;
        let e = vec![ (i as f32 % 13.0)/13.0, (i as f32 % 5.0)/5.0, (i as f32 % 7.0)/7.0, (i as f32 % 3.0)/3.0 ];
        db.query("CREATE type::record('memory',$id) SET content=$c, embedding=$e, forgotten=$f")
            .bind(("id", format!("s{i}"))).bind(("c","scale".to_string())).bind(("e", e)).bind(("f", f)).await?.check()?;
    }
    let qe2 = vec![0.4f32,0.4,0.4,0.4];
    let big = db.query("SELECT meta::id(id) AS id, forgotten FROM memory WHERE embedding <|10,64|> $q AND forgotten = false")
        .bind(("q", qe2)).await?.take::<Value>(0)?;
    let (n, any_forgotten) = if let Value::Array(a) = &big {
        let any = a.iter().any(|r| matches!(r, Value::Object(o) if matches!(o.get("forgotten"), Some(Value::Bool(true)))));
        (a.len(), any)
    } else { (0, false) };
    println!("== [10] SCALE #6949: ~1060 rows, k=10 filtered -> returned {n} rows, any forgotten leaked? {any_forgotten} (want: 10 rows, false) ==");

    println!("\nSPIKE_DONE");
    Ok(())
}

fn summarize(v: &Value) -> String {
    // Count array length + show compact
    let s = format!("{v:?}");
    let len = if let Value::Array(a) = v { a.len().to_string() } else { "n/a".into() };
    let short: String = s.chars().take(600).collect();
    format!("[rows={len}] {short}")
}
