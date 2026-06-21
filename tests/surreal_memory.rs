//! Integration tests for the SurrealDB memory backend (feature `surreal-memory`).
//!
//! These run against the embedded in-memory engine (`kv-mem`) and pass explicit
//! embeddings, so they do not need fastembed at runtime. They mirror the proven
//! reference suite in `spikes/surreal-memory/tests/backend.rs`, but exercise the
//! real in-crate `SurrealMemoryStore` / `SurrealMemorySearch`.
//!
//! NB: the test binary still links the spacebot lib (which pulls fastembed/ort),
//! so running them needs a working ONNX Runtime; in environments where that is
//! unavailable, `cargo check --tests --features surreal-memory` still typechecks
//! them.
#![cfg(feature = "surreal-memory")]

use std::sync::Arc;

use spacebot::memory::search::{SearchConfig, SearchMode};
use spacebot::memory::surreal_store::SurrealMemoryStore;
use spacebot::memory::surreal_search::SurrealMemorySearch;
use spacebot::memory::types::{Association, Memory, MemoryType, RelationType};
use surrealdb::engine::local::{Db, Mem};
use surrealdb::Surreal;

const DIM: usize = 4;

async fn fresh() -> Arc<SurrealMemoryStore> {
    let db: Surreal<Db> = Surreal::new::<Mem>(()).await.unwrap();
    db.use_ns("test").use_db("test").await.unwrap();
    SurrealMemoryStore::from_handle(db, "test-agent", DIM)
        .await
        .unwrap()
}

fn emb(a: f32, b: f32, c: f32, d: f32) -> Vec<f32> {
    vec![a, b, c, d]
}

#[tokio::test]
async fn save_load_roundtrip() {
    let store = fresh().await;
    let mut m = Memory::new("user prefers dark mode", MemoryType::Preference).with_importance(0.7);
    m.source = Some("chat".into());
    store.save(&m, Some(&emb(0.1, 0.2, 0.3, 0.4))).await.unwrap();

    let got = store.load(&m.id).await.unwrap().expect("exists");
    assert_eq!(got.id, m.id);
    assert_eq!(got.content, "user prefers dark mode");
    assert_eq!(got.memory_type, MemoryType::Preference);
    assert!((got.importance - 0.7).abs() < 1e-6);
    assert_eq!(got.source.as_deref(), Some("chat"));
    assert!(!got.forgotten);
}

#[tokio::test]
async fn record_access_increments() {
    let store = fresh().await;
    let m = Memory::new("x", MemoryType::Fact);
    store.save(&m, None).await.unwrap();
    store.record_access(&m.id).await.unwrap();
    store.record_access(&m.id).await.unwrap();
    assert_eq!(store.load(&m.id).await.unwrap().unwrap().access_count, 2);
}

#[tokio::test]
async fn forget_excludes_from_reads() {
    let store = fresh().await;
    let m = Memory::new("forgettable", MemoryType::Event);
    store.save(&m, Some(&emb(0.9, 0.9, 0.9, 0.9))).await.unwrap();
    assert!(store.forget(&m.id).await.unwrap());

    assert!(store.load(&m.id).await.unwrap().unwrap().forgotten);
    let recent = store
        .get_sorted(spacebot::memory::SearchSort::Recent, 10, None)
        .await
        .unwrap();
    assert!(recent.iter().all(|x| x.id != m.id));
    let v = store.vector_search(&emb(0.9, 0.9, 0.9, 0.9), 5).await.unwrap();
    assert!(v.iter().all(|(id, _)| *id != m.id));
}

#[tokio::test]
async fn associations_both_directions_and_neighbors() {
    let store = fresh().await;
    let a = Memory::new("a", MemoryType::Fact);
    let b = Memory::new("b", MemoryType::Fact);
    let c = Memory::new("c", MemoryType::Fact);
    for m in [&a, &b, &c] {
        store.save(m, None).await.unwrap();
    }
    store
        .create_association(&Association::new(&a.id, &b.id, RelationType::RelatedTo).with_weight(0.8))
        .await
        .unwrap();
    store
        .create_association(&Association::new(&b.id, &c.id, RelationType::RelatedTo))
        .await
        .unwrap();

    assert_eq!(store.get_associations(&a.id).await.unwrap().len(), 1);
    assert_eq!(store.get_associations(&b.id).await.unwrap().len(), 2);

    let (depth1, _) = store.get_neighbors(&a.id, 1, &[]).await.unwrap();
    assert_eq!(depth1.len(), 1);
    let (depth2, _) = store.get_neighbors(&a.id, 2, &[]).await.unwrap();
    assert_eq!(depth2.len(), 2);
}

#[tokio::test]
async fn vector_and_fts_and_find_similar() {
    let store = fresh().await;
    for i in 0..20 {
        let content = if i % 5 == 0 {
            format!("memory {i} about saturn rockets")
        } else {
            format!("memory {i} about coffee")
        };
        let f = i as f32 / 20.0;
        store
            .save(&Memory::new(content, MemoryType::Fact), Some(&emb(f, 1.0 - f, 0.5, 0.5)))
            .await
            .unwrap();
    }
    let v = store.vector_search(&emb(0.5, 0.5, 0.5, 0.5), 5).await.unwrap();
    assert_eq!(v.len(), 5);
    for w in v.windows(2) {
        assert!(w[0].1 <= w[1].1 + 1e-6);
    }
    let fts = store.text_search("saturn", 10).await.unwrap();
    assert_eq!(fts.len(), 4);
    assert!(fts.iter().all(|(_, s)| *s > 0.0));
}

#[tokio::test]
async fn merge_rewires_and_soft_deletes() {
    let store = fresh().await;
    let s = Memory::new("survivor saturn", MemoryType::Fact).with_importance(0.9);
    let l = Memory::new("loser saturn rockets", MemoryType::Fact).with_importance(0.5);
    let x = Memory::new("neighbour", MemoryType::Fact);
    store.save(&s, Some(&emb(0.5, 0.5, 0.5, 0.5))).await.unwrap();
    store.save(&l, Some(&emb(0.5, 0.5, 0.5, 0.51))).await.unwrap();
    store.save(&x, Some(&emb(0.1, 0.2, 0.3, 0.4))).await.unwrap();
    store
        .create_association(&Association::new(&l.id, &x.id, RelationType::PartOf).with_weight(0.6))
        .await
        .unwrap();

    store
        .merge(&s.id, &l.id, "survivor saturn\n\nloser saturn rockets", Some(&emb(0.5, 0.5, 0.5, 0.5)))
        .await
        .unwrap();

    assert!(store.load(&s.id).await.unwrap().unwrap().content.contains("rockets"));
    assert!(store.load(&l.id).await.unwrap().unwrap().forgotten);
    let s_assocs = store.get_associations(&s.id).await.unwrap();
    assert!(s_assocs.iter().any(|a| a.target_id == x.id || a.source_id == x.id));
    assert!(s_assocs
        .iter()
        .any(|a| a.relation_type == RelationType::Updates && a.target_id == l.id));
}

#[tokio::test]
async fn hybrid_search_fuses_and_excludes_forgotten() {
    let store = fresh().await;
    let target = Memory::new("saturn rockets orbit mission", MemoryType::Fact);
    store.save(&target, Some(&emb(0.5, 0.5, 0.5, 0.5))).await.unwrap();
    for i in 0..15 {
        let f = i as f32 / 15.0;
        store
            .save(&Memory::new(format!("coffee note {i}"), MemoryType::Observation), Some(&emb(f, 0.1, 0.9, f)))
            .await
            .unwrap();
    }
    let ghost = Memory::new("saturn rockets ghost", MemoryType::Fact);
    store.save(&ghost, Some(&emb(0.5, 0.5, 0.5, 0.5))).await.unwrap();
    store.forget(&ghost.id).await.unwrap();

    let search = SurrealMemorySearch::new(store);
    let cfg = SearchConfig {
        mode: SearchMode::Hybrid,
        ..Default::default()
    };
    let res = search
        .search("saturn rockets", &emb(0.5, 0.5, 0.5, 0.5), &cfg)
        .await
        .unwrap();
    assert!(res.iter().any(|r| r.memory.id == target.id));
    assert!(res.iter().all(|r| r.memory.id != ghost.id));
    assert_eq!(res[0].rank, 1);
}

#[tokio::test]
async fn prune_below_server_side() {
    let store = fresh().await;
    let old = chrono::Utc::now() - chrono::Duration::days(60);
    let mut a = Memory::new("low old fact", MemoryType::Fact).with_importance(0.05);
    a.created_at = old;
    let mut id = Memory::new("low old identity", MemoryType::Identity).with_importance(0.05);
    id.created_at = old;
    let recent = Memory::new("low recent", MemoryType::Fact).with_importance(0.05);
    for m in [&a, &id, &recent] { store.save(m, None).await.unwrap(); }
    let cutoff = chrono::Utc::now() - chrono::Duration::days(30);
    let pruned = store.prune_below(0.1, cutoff).await.unwrap();
    assert_eq!(pruned, 1);
    assert!(store.load(&a.id).await.unwrap().is_none());
    assert!(store.load(&id.id).await.unwrap().is_some());
    assert!(store.load(&recent.id).await.unwrap().is_some());
}
