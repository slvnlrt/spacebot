//! Integration tests for the reference SurrealDB memory backend, against the
//! embedded in-memory engine (kv-mem) — the planned test-harness engine.
use surrealdb::engine::local::Mem;
use surrealdb::Surreal;
use surreal_memory_spike::*;

const DIM: usize = 4;

async fn fresh() -> MemoryStore<surrealdb::engine::local::Db> {
    let db = Surreal::new::<Mem>(()).await.unwrap();
    db.use_ns("t").use_db("t").await.unwrap();
    let store = MemoryStore::new(db);
    store.define_schema(DIM).await.unwrap();
    store
}

fn emb(a: f32, b: f32, c: f32, d: f32) -> Vec<f32> {
    vec![a, b, c, d]
}

#[tokio::test]
async fn save_load_roundtrip_with_uuid_ids() {
    let store = fresh().await;
    let mut m = Memory::new("the user prefers dark mode", MemoryType::Preference)
        .with_importance(0.7);
    m.source = Some("chat".into());
    m.channel_id = Some("c1".into());
    store.save(&m, Some(&emb(0.1, 0.2, 0.3, 0.4))).await.unwrap();

    let got = store.load(&m.id).await.unwrap().expect("memory exists");
    assert_eq!(got.id, m.id);
    assert_eq!(got.content, "the user prefers dark mode");
    assert_eq!(got.memory_type, MemoryType::Preference);
    assert!((got.importance - 0.7).abs() < 1e-6);
    assert_eq!(got.source.as_deref(), Some("chat"));
    assert_eq!(got.channel_id.as_deref(), Some("c1"));
    assert!(!got.forgotten);
}

#[tokio::test]
async fn touch_increments_access_count() {
    let store = fresh().await;
    let m = Memory::new("x", MemoryType::Fact);
    store.save(&m, None).await.unwrap();
    store.touch(&m.id).await.unwrap();
    store.touch(&m.id).await.unwrap();
    let got = store.load(&m.id).await.unwrap().unwrap();
    assert_eq!(got.access_count, 2);
}

#[tokio::test]
async fn forget_excludes_from_sorted_and_vector() {
    let store = fresh().await;
    let m = Memory::new("forgettable rockets", MemoryType::Event);
    store.save(&m, Some(&emb(0.9, 0.9, 0.9, 0.9))).await.unwrap();
    store.forget(&m.id).await.unwrap();

    // still loadable (retained) but flagged
    assert!(store.load(&m.id).await.unwrap().unwrap().forgotten);
    // excluded from sorted
    let recent = store.get_sorted(SearchSort::Recent, 10, None).await.unwrap();
    assert!(recent.iter().all(|x| x.id != m.id));
    // excluded from vector search
    let v = store.vector_search(&emb(0.9, 0.9, 0.9, 0.9), 5).await.unwrap();
    assert!(v.iter().all(|(id, _)| *id != m.id));
}

#[tokio::test]
async fn get_sorted_modes_and_type_filter() {
    let store = fresh().await;
    let specs = [
        ("identity", MemoryType::Identity, 1.0),
        ("a decision", MemoryType::Decision, 0.9),
        ("a pref", MemoryType::Preference, 0.7),
        ("an event", MemoryType::Event, 0.3),
    ];
    for (c, t, imp) in specs {
        let m = Memory::new(c, t).with_importance(imp);
        store.save(&m, None).await.unwrap();
    }
    let by_imp = store.get_sorted(SearchSort::Importance, 10, None).await.unwrap();
    assert_eq!(by_imp[0].memory_type, MemoryType::Identity);
    assert_eq!(by_imp[1].memory_type, MemoryType::Decision);

    let typed = store
        .get_sorted(SearchSort::Recent, 10, Some(MemoryType::Decision))
        .await
        .unwrap();
    assert_eq!(typed.len(), 1);
    assert_eq!(typed[0].memory_type, MemoryType::Decision);
}

#[tokio::test]
async fn associations_and_get_associations_both_directions() {
    let store = fresh().await;
    let a = Memory::new("a", MemoryType::Fact);
    let b = Memory::new("b", MemoryType::Fact);
    store.save(&a, None).await.unwrap();
    store.save(&b, None).await.unwrap();
    store
        .add_association(&Association::new(&a.id, &b.id, RelationType::RelatedTo).with_weight(0.8))
        .await
        .unwrap();

    let from_a = store.get_associations(&a.id).await.unwrap();
    assert_eq!(from_a.len(), 1);
    assert_eq!(from_a[0].relation_type, RelationType::RelatedTo);
    assert!((from_a[0].weight - 0.8).abs() < 1e-6);
    // reverse direction is found too
    let from_b = store.get_associations(&b.id).await.unwrap();
    assert_eq!(from_b.len(), 1);
}

#[tokio::test]
async fn get_neighbors_bfs() {
    let store = fresh().await;
    let ids: Vec<Memory> = (0..4).map(|i| Memory::new(format!("n{i}"), MemoryType::Fact)).collect();
    for m in &ids {
        store.save(m, None).await.unwrap();
    }
    // chain n0 -> n1 -> n2 -> n3
    for w in ids.windows(2) {
        store
            .add_association(&Association::new(&w[0].id, &w[1].id, RelationType::RelatedTo))
            .await
            .unwrap();
    }
    let depth1 = store.get_neighbors(&ids[0].id, 1).await.unwrap();
    assert_eq!(depth1.len(), 1, "depth 1 reaches n1 only");
    let depth2 = store.get_neighbors(&ids[0].id, 2).await.unwrap();
    assert_eq!(depth2.len(), 2, "depth 2 reaches n1, n2");
}

#[tokio::test]
async fn vector_search_orders_by_distance_and_respects_k() {
    let store = fresh().await;
    for i in 0..20 {
        let m = Memory::new(format!("m{i}"), MemoryType::Fact);
        let f = i as f32 / 20.0;
        store.save(&m, Some(&emb(f, 1.0 - f, 0.5, 0.5))).await.unwrap();
    }
    let res = store.vector_search(&emb(0.5, 0.5, 0.5, 0.5), 5).await.unwrap();
    assert_eq!(res.len(), 5);
    // distances ascending
    for w in res.windows(2) {
        assert!(w[0].1 <= w[1].1 + 1e-6);
    }
}

#[tokio::test]
async fn text_search_bm25_ranks_discriminative_terms() {
    let store = fresh().await;
    for i in 0..20 {
        let content = if i % 5 == 0 {
            format!("memory {i} about saturn rockets")
        } else {
            format!("memory {i} about coffee")
        };
        store.save(&Memory::new(content, MemoryType::Fact), None).await.unwrap();
    }
    let hits = store.text_search("saturn", 10).await.unwrap();
    assert_eq!(hits.len(), 4, "saturn appears in 4 of 20");
    assert!(hits.iter().all(|(_, s)| *s > 0.0));
}

#[tokio::test]
async fn find_similar_excludes_self() {
    let store = fresh().await;
    let mut ids = Vec::new();
    for i in 0..10 {
        let m = Memory::new(format!("m{i}"), MemoryType::Fact);
        let f = i as f32 / 10.0;
        store.save(&m, Some(&emb(f, f, f, f))).await.unwrap();
        ids.push(m.id);
    }
    let sim = store.find_similar(&ids[5], 0.0, 3).await.unwrap();
    assert!(sim.len() <= 3);
    assert!(sim.iter().all(|(id, _)| *id != ids[5]), "self excluded");
}

#[tokio::test]
async fn hybrid_search_fuses_and_filters_forgotten() {
    let store = fresh().await;
    // vector + fts hit
    let target = Memory::new("saturn rockets orbit mission", MemoryType::Fact);
    store.save(&target, Some(&emb(0.5, 0.5, 0.5, 0.5))).await.unwrap();
    // noise
    for i in 0..15 {
        let m = Memory::new(format!("coffee note {i}"), MemoryType::Observation);
        let f = i as f32 / 15.0;
        store.save(&m, Some(&emb(f, 0.1, 0.9, f))).await.unwrap();
    }
    // a forgotten near-duplicate must not appear
    let ghost = Memory::new("saturn rockets ghost", MemoryType::Fact);
    store.save(&ghost, Some(&emb(0.5, 0.5, 0.5, 0.5))).await.unwrap();
    store.forget(&ghost.id).await.unwrap();

    let cfg = SearchConfig::default();
    let res = hybrid_search(&store, "saturn rockets", &emb(0.5, 0.5, 0.5, 0.5), &cfg)
        .await
        .unwrap();
    assert!(!res.is_empty());
    assert!(res.iter().any(|r| r.memory.id == target.id), "target found");
    assert!(res.iter().all(|r| r.memory.id != ghost.id), "forgotten excluded");
    // ranks are 1-based and ascending
    assert_eq!(res[0].rank, 1);
}

#[tokio::test]
async fn merge_rewires_edges_and_soft_deletes_loser() {
    let store = fresh().await;
    // survivor S, loser L, neighbour X. L is connected to X (L -> X related_to).
    let s = Memory::new("survivor about saturn", MemoryType::Fact).with_importance(0.9);
    let l = Memory::new("loser about saturn rockets", MemoryType::Fact).with_importance(0.5);
    let x = Memory::new("neighbour planet", MemoryType::Fact);
    store.save(&s, Some(&emb(0.5, 0.5, 0.5, 0.5))).await.unwrap();
    store.save(&l, Some(&emb(0.5, 0.5, 0.5, 0.51))).await.unwrap();
    store.save(&x, Some(&emb(0.1, 0.2, 0.3, 0.4))).await.unwrap();
    store.add_association(&Association::new(&l.id, &x.id, RelationType::PartOf).with_weight(0.6)).await.unwrap();

    store.merge(&s.id, &l.id, "survivor about saturn\n\nloser about saturn rockets", Some(&emb(0.5, 0.5, 0.5, 0.5))).await.unwrap();

    // survivor content updated
    let s_after = store.load(&s.id).await.unwrap().unwrap();
    assert!(s_after.content.contains("rockets"));
    // loser soft-deleted
    assert!(store.load(&l.id).await.unwrap().unwrap().forgotten);
    // X is now connected to survivor (edge rewired), with the same PartOf type
    let s_assocs = store.get_associations(&s.id).await.unwrap();
    assert!(
        s_assocs.iter().any(|a| (a.source_id == s.id && a.target_id == x.id) || (a.source_id == x.id && a.target_id == s.id)),
        "survivor connected to X after rewire: {s_assocs:?}"
    );
    // survivor ->updates-> loser exists
    assert!(s_assocs.iter().any(|a| a.relation_type == RelationType::Updates && a.target_id == l.id));
    // loser has no live edges left except the incoming updates edge
    let l_assocs = store.get_associations(&l.id).await.unwrap();
    assert!(l_assocs.iter().all(|a| a.relation_type == RelationType::Updates));
}
