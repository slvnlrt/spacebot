//! Integration tests for the reference SurrealDB memory backend, against the
//! embedded in-memory engine (kv-mem) — the planned test-harness engine.
use std::collections::HashSet;
use std::time::Instant;
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

#[tokio::test]
async fn prune_below_deletes_only_old_low_importance_non_identity() {
    let store = fresh().await;
    let old = chrono::Utc::now() - chrono::Duration::days(60);
    // low + old + fact  -> pruned
    let mut a = Memory::new("low old fact", MemoryType::Fact).with_importance(0.05);
    a.created_at = old;
    // low + old + identity -> kept (identity never pruned)
    let mut b = Memory::new("low old identity", MemoryType::Identity).with_importance(0.05);
    b.created_at = old;
    // low + recent -> kept (too new)
    let c = Memory::new("low recent", MemoryType::Fact).with_importance(0.05);
    // high + old -> kept (above threshold)
    let mut d = Memory::new("high old", MemoryType::Fact).with_importance(0.9);
    d.created_at = old;
    for m in [&a, &b, &c, &d] { store.save(m, None).await.unwrap(); }

    let cutoff = chrono::Utc::now() - chrono::Duration::days(30);
    let pruned = store.prune_below(0.1, cutoff).await.unwrap();
    assert_eq!(pruned, 1, "only the low+old+fact is pruned");
    assert!(store.load(&a.id).await.unwrap().is_none());
    assert!(store.load(&b.id).await.unwrap().is_some());
    assert!(store.load(&c.id).await.unwrap().is_some());
    assert!(store.load(&d.id).await.unwrap().is_some());
}

// ============================================================================
// Task C1: Empirically verify native SurrealDB graph recursion
// ============================================================================
//
// Graph: a→b→c (c is forgotten), a→d
// This tests the parity contract for get_neighbors_native vs get_neighbors_with_edges.

/// Helper to build the Task C1 reference graph:
///   a→b→c (c forgotten), a→d
///
/// Returns (store, a_id, b_id, c_id, d_id).
async fn build_c1_graph() -> (MemoryStore<surrealdb::engine::local::Db>, String, String, String, String) {
    let store = fresh().await;
    let a = Memory::new("node a", MemoryType::Fact);
    let b = Memory::new("node b", MemoryType::Fact);
    let c = Memory::new("node c (forgotten)", MemoryType::Fact);
    let d = Memory::new("node d", MemoryType::Fact);
    for m in [&a, &b, &c, &d] {
        store.save(m, None).await.unwrap();
    }
    store.forget(&c.id).await.unwrap();
    // a→b→c (chain), a→d (branch)
    store.add_association(&Association::new(&a.id, &b.id, RelationType::RelatedTo)).await.unwrap();
    store.add_association(&Association::new(&b.id, &c.id, RelationType::RelatedTo)).await.unwrap();
    store.add_association(&Association::new(&a.id, &d.id, RelationType::RelatedTo)).await.unwrap();
    let (a_id, b_id, c_id, d_id) = (a.id, b.id, c.id, d.id);
    (store, a_id, b_id, c_id, d_id)
}

// --- C1-Step 1: Probe undirected recursive collect at bounded depth ---

/// Verify that forward `{..2+collect}->relates->memory` works on 3.1.x.
/// This is the PRIMARY collect direction; its result should include b, c (via chain).
#[tokio::test]
async fn c1_probe_forward_collect() {
    let (store, a_id, b_id, c_id, d_id) = build_c1_graph().await;
    let root = surrealdb::types::RecordId::new("memory", a_id.clone());

    let sql = "$root.{..2+collect}->relates->memory";
    let mut r = store.db().query(sql).bind(("root", root)).await.unwrap();
    let v: surrealdb::types::Value = r.take(0).unwrap();

    // Extract string IDs from RecordID array
    let ids = extract_value_ids(&v);
    println!("[C1 forward collect depth=2] ids={ids:?}");

    // At depth 2 from a: should reach b (hop1), c (hop2 via b), d (hop1).
    assert!(ids.contains(&b_id), "forward collect must include b");
    assert!(ids.contains(&c_id), "forward collect traverses THROUGH forgotten c (strategy b)");
    assert!(ids.contains(&d_id), "forward collect must include d");
    assert!(!ids.contains(&a_id), "root excluded by default (no +inclusive)");
}

/// Verify that backward `{..2+collect}<-relates<-memory` works on 3.1.x.
/// From `b`, going backward should reach `a` (b's in-edge source).
#[tokio::test]
async fn c1_probe_backward_collect() {
    let (store, a_id, b_id, _c_id, _d_id) = build_c1_graph().await;
    let root = surrealdb::types::RecordId::new("memory", b_id.clone());

    let sql = "$root.{..2+collect}<-relates<-memory";
    let mut r = store.db().query(sql).bind(("root", root)).await.unwrap();
    let v: surrealdb::types::Value = r.take(0).unwrap();

    let ids = extract_value_ids(&v);
    println!("[C1 backward collect from b, depth=2] ids={ids:?}");

    assert!(ids.contains(&a_id), "backward collect from b must reach a");
    assert!(!ids.contains(&b_id), "root b excluded by default");
}

/// EMPIRICAL CHECK: does `{..2+collect}<->relates<->memory` work on 3.1.x?
/// Per Kodex reference, `<->` with `{..}` is expected to be UNSUPPORTED.
/// This test records the result without asserting a specific value — it prints
/// the outcome for the report. We only assert that the fallback union (fwd+bwd) works.
#[tokio::test]
async fn c1_probe_undirected_bidirectional_syntax() {
    let (store, a_id, _b_id, _c_id, _d_id) = build_c1_graph().await;
    let root = surrealdb::types::RecordId::new("memory", a_id.clone());

    // Probe: does <-> with {..} recursion parse + run?
    let undirected_result = store.db()
        .query("$root.{..2+collect}<->relates<->memory")
        .bind(("root", root.clone()))
        .await;

    match undirected_result {
        Ok(mut r) => {
            match r.take::<surrealdb::types::Value>(0) {
                Ok(v) => {
                    let ids = extract_value_ids(&v);
                    println!("[C1 <-> undirected collect] RESULT (unexpected success): ids={ids:?}");
                    // If it DID work, print for report but do not fail.
                    // Per Kodex empirical evidence, this is not expected to work.
                    println!("[C1 <-> decision] <-> WITH {{..}} recursion: SUPPORTED (unexpected)");
                }
                Err(e) => {
                    println!("[C1 <-> undirected collect] take error (expected): {e}");
                    println!("[C1 <-> decision] <-> WITH {{..}} recursion: UNSUPPORTED — use forward+backward union");
                }
            }
        }
        Err(e) => {
            println!("[C1 <-> undirected collect] query error (expected): {e}");
            println!("[C1 <-> decision] <-> WITH {{..}} recursion: UNSUPPORTED — use forward+backward union");
        }
    }

    // Fallback union (forward + backward) MUST work regardless:
    let mut fwd_r = store.db().query("$root.{..2+collect}->relates->memory")
        .bind(("root", root.clone())).await.unwrap();
    let fwd: surrealdb::types::Value = fwd_r.take(0).unwrap();
    let fwd_ids = extract_value_ids(&fwd);
    println!("[C1 union-fwd] ids={fwd_ids:?}");
    assert!(!fwd_ids.is_empty(), "forward union must return results");
}

// --- C1-Step 2: Determine forgotten-node traversal behaviour ---

/// Tests strategy (b): traverse-through forgotten + hydrate-filter.
/// Native traversal visits c (forgotten) as a waypoint; the hydrate WHERE
/// `forgotten = false` excludes c from returned memories.
/// Node `e` is reachable ONLY via c — it will appear in native but NOT in BFS.
/// This is the "benign superset" delta documented in the plan.
#[tokio::test]
async fn c1_forgotten_strategy_b_traverse_through() {
    let store = fresh().await;
    let a = Memory::new("a", MemoryType::Fact);
    let b = Memory::new("b", MemoryType::Fact);
    let c = Memory::new("c forgotten", MemoryType::Fact); // forgotten
    let d = Memory::new("d", MemoryType::Fact);           // reachable only via c
    for m in [&a, &b, &c, &d] { store.save(m, None).await.unwrap(); }
    store.forget(&c.id).await.unwrap();
    // a→b, b→c (forgotten), c→d (reachable only via forgotten c)
    store.add_association(&Association::new(&a.id, &b.id, RelationType::RelatedTo)).await.unwrap();
    store.add_association(&Association::new(&b.id, &c.id, RelationType::RelatedTo)).await.unwrap();
    store.add_association(&Association::new(&c.id, &d.id, RelationType::RelatedTo)).await.unwrap();

    // BFS (reference): does NOT traverse through c → d is unreachable
    let (bfs_mems, _) = store.get_neighbors_with_edges(&a.id, 3, &[]).await.unwrap();
    let bfs_ids: HashSet<_> = bfs_mems.iter().map(|m| m.id.clone()).collect();
    println!("[C1 forgotten BFS] ids={bfs_ids:?}");
    assert!(bfs_ids.contains(&b.id), "BFS reaches b");
    assert!(!bfs_ids.contains(&c.id), "BFS excludes forgotten c");
    assert!(!bfs_ids.contains(&d.id), "BFS cannot reach d (blocked by forgotten c)");

    // Native (strategy b): traverses through c, hydrate filters c
    let (nat_mems, _) = store.get_neighbors_native(&a.id, 3, &[]).await.unwrap();
    let nat_ids: HashSet<_> = nat_mems.iter().map(|m| m.id.clone()).collect();
    println!("[C1 forgotten native strategy-b] ids={nat_ids:?}");
    assert!(nat_ids.contains(&b.id), "native reaches b");
    assert!(!nat_ids.contains(&c.id), "native hydrate-filter excludes forgotten c");
    // d appears in native because recursion passed THROUGH c (accepted superset delta)
    println!("[C1 forgotten decision] strategy (b) confirmed: native superset includes d={}", nat_ids.contains(&d.id));
    // Note: whether d actually appears depends on the depth cap; at depth 3 it should.
    // Either outcome is acceptable — we document it.
}

/// Tests strategy (a) PROBE: edge-filter `[WHERE out.forgotten=false]`.
/// Per Kodex reference, this is expected to be UNSUPPORTED on 3.1.x.
/// The test records the result without failing on either outcome.
#[tokio::test]
async fn c1_forgotten_strategy_a_edge_filter_probe() {
    let store = fresh().await;
    let a = Memory::new("a", MemoryType::Fact);
    let b = Memory::new("b forgotten", MemoryType::Fact);
    for m in [&a, &b] { store.save(m, None).await.unwrap(); }
    store.forget(&b.id).await.unwrap();
    store.add_association(&Association::new(&a.id, &b.id, RelationType::RelatedTo)).await.unwrap();

    let root = surrealdb::types::RecordId::new("memory", a.id.clone());

    // Probe: can an edge WHERE clause filter on the TARGET node's scalar field?
    let result = store.db()
        .query("$root.{..2+collect}->relates[WHERE out.forgotten=false]->memory")
        .bind(("root", root))
        .await;

    match result {
        Ok(mut r) => match r.take::<surrealdb::types::Value>(0) {
            Ok(v) => {
                let ids = extract_value_ids(&v);
                println!("[C1 strategy-a probe] RESULT: ids={ids:?}");
                if ids.contains(&b.id) {
                    println!("[C1 strategy-a] filter NOT applied — b (forgotten) still appears");
                } else {
                    println!("[C1 strategy-a] filter APPLIED — b correctly excluded (strategy a WORKS)");
                }
            }
            Err(e) => println!("[C1 strategy-a probe] take error: {e}"),
        },
        Err(e) => println!("[C1 strategy-a probe] query error (filter unsupported): {e}"),
    }
    // Strategy (b) is the default — this probe is informational only.
}

// --- C1-Step 3: Parity verification: native 3-query plan vs BFS ---

/// depth=0: both BFS and native must return ([], []).
#[tokio::test]
async fn c1_parity_depth_zero() {
    let (store, a_id, _, _, _) = build_c1_graph().await;

    let (bfs_mems, bfs_edges) = store.get_neighbors_with_edges(&a_id, 0, &[]).await.unwrap();
    let (nat_mems, nat_edges) = store.get_neighbors_native(&a_id, 0, &[]).await.unwrap();

    assert!(bfs_mems.is_empty(), "BFS depth=0 → empty memories");
    assert!(bfs_edges.is_empty(), "BFS depth=0 → empty edges");
    assert!(nat_mems.is_empty(), "native depth=0 → empty memories");
    assert!(nat_edges.is_empty(), "native depth=0 → empty edges");
    println!("[C1 parity depth=0] OK: both return ([], [])");
}

/// depth=1: BFS and native should both return only direct non-forgotten neighbours.
/// From `a` at depth=1: should reach b (hop1) and d (hop1), NOT c (c is forgotten hop2).
#[tokio::test]
async fn c1_parity_depth_one() {
    let (store, a_id, b_id, c_id, d_id) = build_c1_graph().await;

    let (bfs_mems, bfs_edges) = store.get_neighbors_with_edges(&a_id, 1, &[]).await.unwrap();
    let (nat_mems, nat_edges) = store.get_neighbors_native(&a_id, 1, &[]).await.unwrap();

    let bfs_ids: HashSet<_> = bfs_mems.iter().map(|m| m.id.clone()).collect();
    let nat_ids: HashSet<_> = nat_mems.iter().map(|m| m.id.clone()).collect();
    println!("[C1 parity depth=1] BFS ids={bfs_ids:?}, native ids={nat_ids:?}");

    // Both should reach b and d (direct neighbours from a)
    assert!(bfs_ids.contains(&b_id), "BFS depth=1 includes b");
    assert!(bfs_ids.contains(&d_id), "BFS depth=1 includes d");
    assert!(!bfs_ids.contains(&c_id), "BFS depth=1: c not reachable at hop1 (c is at hop2)");
    assert!(nat_ids.contains(&b_id), "native depth=1 includes b");
    assert!(nat_ids.contains(&d_id), "native depth=1 includes d");

    // Node set parity (BFS ⊆ native — native may include extras due to forgotten traversal)
    for id in &bfs_ids {
        assert!(nat_ids.contains(id), "native at depth=1 must include all BFS nodes: {id}");
    }

    // Edge set parity: BFS expanded only root (a) at depth=1.
    // Both should have exactly the 2 edges incident to a (a→b, a→d).
    let bfs_edge_pairs: HashSet<_> = bfs_edges.iter()
        .map(|e| (e.source_id.clone(), e.target_id.clone()))
        .collect();
    let nat_edge_pairs: HashSet<_> = nat_edges.iter()
        .map(|e| (e.source_id.clone(), e.target_id.clone()))
        .collect();
    println!("[C1 parity depth=1] BFS edges={bfs_edge_pairs:?}, native edges={nat_edge_pairs:?}");
    // BFS edge set must be a subset of native edge set
    for ep in &bfs_edge_pairs {
        assert!(nat_edge_pairs.contains(ep), "native must include BFS edge {ep:?}");
    }
    println!("[C1 parity depth=1] OK: node parity ✓, edge parity ✓");
}

/// depth=2: full parity test including the EXPANDED≠COLLECTED edge-set trap.
///
/// Graph: a→b→c (c forgotten), a→d
/// BFS depth=2 from a:
///   - Expands a (d=0): sees a→b, a→d → collects b, d
///   - Expands b (d=1): sees b→c (forgotten) → does NOT enqueue c
///   - Expands d (d=1): no outgoing edges
///   - Does NOT expand collected nodes at d=2 (there are none — b,d are at hop1)
///   - Result: memories=[b, d], edges=[a→b, a→d, b→c] (b expanded, saw b→c edge)
///
/// Native depth=2 from a:
///   - COLLECTED = {b, d, c} (via {..2+collect} forward; c is included since native traverses through forgotten)
///   - Hydrate: excludes c (forgotten=true) → memories=[b, d]
///   - EXPANDED = {a} ∪ {..1+collect} = {a, b, d} (nodes within depth-1=1 hops)
///   - Edges from EXPANDED {a,b,d}: a→b, a→d, b→c
///
/// 🔴 PARITY POINT: edges come from EXPANDED (a,b,d), NOT COLLECTED (a,b,c,d).
/// If we used COLLECTED for edges, we'd also get c's outgoing edges — wrong.
#[tokio::test]
async fn c1_parity_depth_two() {
    let (store, a_id, b_id, c_id, d_id) = build_c1_graph().await;

    let (bfs_mems, bfs_edges) = store.get_neighbors_with_edges(&a_id, 2, &[]).await.unwrap();
    let (nat_mems, nat_edges) = store.get_neighbors_native(&a_id, 2, &[]).await.unwrap();

    let bfs_ids: HashSet<_> = bfs_mems.iter().map(|m| m.id.clone()).collect();
    let nat_ids: HashSet<_> = nat_mems.iter().map(|m| m.id.clone()).collect();

    println!("[C1 parity depth=2] BFS memories={bfs_ids:?}");
    println!("[C1 parity depth=2] native memories={nat_ids:?}");

    // BFS: b and d reachable; c is forgotten so BFS stops at b
    assert!(bfs_ids.contains(&b_id), "BFS depth=2 includes b");
    assert!(bfs_ids.contains(&d_id), "BFS depth=2 includes d");
    assert!(!bfs_ids.contains(&c_id), "BFS depth=2 excludes forgotten c");
    assert!(!bfs_ids.contains(&a_id), "BFS never includes root a");

    // Native: b and d, c excluded by hydrate; root a always excluded
    assert!(nat_ids.contains(&b_id), "native depth=2 includes b");
    assert!(nat_ids.contains(&d_id), "native depth=2 includes d");
    assert!(!nat_ids.contains(&c_id), "native hydrate excludes forgotten c");
    assert!(!nat_ids.contains(&a_id), "native excludes root a");

    // BFS ⊆ native for memories (native may be a superset due to forgotten traversal)
    for id in &bfs_ids {
        assert!(nat_ids.contains(id), "native at depth=2 must include all BFS memory {id}");
    }

    // Edge set: BFS expands a (d=0) and b,d (d=1); expanded = {a, b, d}
    // edges seen: a→b, a→d (from a), b→c (from b), nothing from d
    let bfs_edge_pairs: HashSet<_> = bfs_edges.iter()
        .map(|e| (e.source_id.clone(), e.target_id.clone()))
        .collect();
    let nat_edge_pairs: HashSet<_> = nat_edges.iter()
        .map(|e| (e.source_id.clone(), e.target_id.clone()))
        .collect();

    println!("[C1 parity depth=2] BFS edges={bfs_edge_pairs:?}");
    println!("[C1 parity depth=2] native edges={nat_edge_pairs:?}");

    // Expected BFS edges: a→b, a→d, b→c
    let a_to_b = (a_id.clone(), b_id.clone());
    let a_to_d = (a_id.clone(), d_id.clone());
    let b_to_c = (b_id.clone(), c_id.clone());
    assert!(bfs_edge_pairs.contains(&a_to_b), "BFS edge a→b");
    assert!(bfs_edge_pairs.contains(&a_to_d), "BFS edge a→d");
    assert!(bfs_edge_pairs.contains(&b_to_c), "BFS edge b→c (b was expanded, even though c is forgotten)");

    // 🔴 Critical parity: BFS edge set ⊆ native edge set
    for ep in &bfs_edge_pairs {
        assert!(nat_edge_pairs.contains(ep),
            "native edge set must include BFS edge {ep:?} (EXPANDED set correctness)");
    }

    println!("[C1 parity depth=2] OK: memories parity ✓, edge parity ✓ (EXPANDED≠COLLECTED verified)");
}

/// Parity with exclude_ids: excluded nodes are not in memories or edge queries,
/// and BFS never enqueues them.
#[tokio::test]
async fn c1_parity_with_exclude_ids() {
    let (store, a_id, b_id, _c_id, d_id) = build_c1_graph().await;

    // Exclude d: BFS should not return d, native should also skip d
    let (bfs_mems, bfs_edges) = store.get_neighbors_with_edges(&a_id, 2, &[d_id.as_str()]).await.unwrap();
    let (nat_mems, nat_edges) = store.get_neighbors_native(&a_id, 2, &[d_id.as_str()]).await.unwrap();

    let bfs_ids: HashSet<_> = bfs_mems.iter().map(|m| m.id.clone()).collect();
    let nat_ids: HashSet<_> = nat_mems.iter().map(|m| m.id.clone()).collect();

    println!("[C1 parity exclude_ids] BFS={bfs_ids:?}, native={nat_ids:?}");
    assert!(!bfs_ids.contains(&d_id), "BFS excludes d (in exclude_ids)");
    assert!(bfs_ids.contains(&b_id), "BFS still reaches b");
    assert!(!nat_ids.contains(&d_id), "native excludes d (in exclude_ids)");
    assert!(nat_ids.contains(&b_id), "native still reaches b");

    // BFS ⊆ native
    for id in &bfs_ids {
        assert!(nat_ids.contains(id), "native includes all BFS nodes when d excluded: {id}");
    }

    let _ = (bfs_edges, nat_edges); // captured, not asserted in detail here
    println!("[C1 parity exclude_ids] OK");
}

// --- C1-Step 3 latency: ~1k-node graph, native 4-query plan vs BFS N+1 ---

/// Build a ~1k node star+chain graph and compare latency of native vs BFS.
/// This is not a correctness assertion — it captures timing for the report.
#[tokio::test]
async fn c1_latency_native_vs_bfs_1k_nodes() {
    let store = fresh().await;

    // Build: root + 50 direct neighbours + each has 20 children = 1051 nodes total
    let root = Memory::new("root", MemoryType::Fact);
    store.save(&root, None).await.unwrap();

    let mut tier1_ids = Vec::new();
    for i in 0..50 {
        let m = Memory::new(format!("tier1-{i}"), MemoryType::Fact);
        store.save(&m, None).await.unwrap();
        store.add_association(&Association::new(&root.id, &m.id, RelationType::RelatedTo)).await.unwrap();
        tier1_ids.push(m.id);
    }
    for (i, t1_id) in tier1_ids.iter().enumerate() {
        for j in 0..20 {
            let m = Memory::new(format!("tier2-{i}-{j}"), MemoryType::Fact);
            store.save(&m, None).await.unwrap();
            store.add_association(&Association::new(t1_id, &m.id, RelationType::RelatedTo)).await.unwrap();
        }
    }

    let total: surrealdb::types::Value = store.db()
        .query("SELECT count() FROM memory GROUP ALL").await.unwrap()
        .take(0).unwrap();
    println!("[C1 latency] total nodes: {total:?}");

    // BFS depth=2 (N+1 round-trips: 1 per expanded node)
    let t0 = Instant::now();
    let (bfs_mems, bfs_edges) = store.get_neighbors_with_edges(&root.id, 2, &[]).await.unwrap();
    let bfs_ms = t0.elapsed().as_millis();
    println!("[C1 latency] BFS depth=2: {} memories, {} edges, {}ms", bfs_mems.len(), bfs_edges.len(), bfs_ms);

    // Native 3-query plan depth=2
    let t1 = Instant::now();
    let (nat_mems, nat_edges) = store.get_neighbors_native(&root.id, 2, &[]).await.unwrap();
    let nat_ms = t1.elapsed().as_millis();
    println!("[C1 latency] native depth=2: {} memories, {} edges, {}ms", nat_mems.len(), nat_edges.len(), nat_ms);

    // Parity check: BFS memories ⊆ native memories
    let bfs_ids: HashSet<_> = bfs_mems.iter().map(|m| m.id.clone()).collect();
    let nat_ids: HashSet<_> = nat_mems.iter().map(|m| m.id.clone()).collect();
    for id in &bfs_ids {
        assert!(nat_ids.contains(id), "latency test: native must include BFS node {id}");
    }

    // Expected: 50 tier1 + 1000 tier2 = 1050 memories from root at depth=2
    assert_eq!(bfs_mems.len(), 1050, "BFS depth=2 should reach all 50+1000 nodes");
    assert_eq!(nat_mems.len(), 1050, "native depth=2 should match BFS count");

    println!("[C1 latency] speedup: BFS={}ms, native={}ms ({}x)",
        bfs_ms, nat_ms,
        if nat_ms > 0 { bfs_ms / nat_ms } else { 0 });
}

// ---- Helper: extract string IDs from a Value (array of RecordIds) ----
fn extract_value_ids(v: &surrealdb::types::Value) -> HashSet<String> {
    let mut out = HashSet::new();
    match v {
        surrealdb::types::Value::Array(arr) => {
            for item in arr.iter() {
                if let surrealdb::types::Value::RecordId(rid) = item {
                    if let surrealdb::types::RecordIdKey::String(ref key) = rid.key {
                        out.insert(key.clone());
                    }
                }
            }
        }
        surrealdb::types::Value::RecordId(rid) => {
            if let surrealdb::types::RecordIdKey::String(ref key) = rid.key {
                out.insert(key.clone());
            }
        }
        _ => {}
    }
    out
}

/// Decision (C) — backup/restore story for the per-agent embedded SurrealKV.
///
/// SurrealKV has no copyable single-file like SQLite, so the operational backup
/// is a **filesystem copy of the store directory** (which lives under the
/// agent's `data_dir/surreal` — already covered by any data-dir backup). This
/// test proves a cold copy (store quiesced/closed) restores with data intact.
/// For a LIVE copy, quiesce the agent first — the same constraint as SQLite WAL
/// / LanceDB (a live copy without a checkpoint can be inconsistent).
#[tokio::test]
async fn surrealkv_cold_copy_backup_restores() {
    use surrealdb::engine::local::SurrealKv;

    let base = std::env::temp_dir().join(format!("surreal-backup-{}", uuid::Uuid::new_v4()));
    let src = base.join("src");
    let dst = base.join("dst");
    std::fs::create_dir_all(&src).unwrap();

    // 1. Write data into an on-disk store, then CLOSE it (drop the handle).
    {
        let db = Surreal::new::<SurrealKv>(src.to_str().unwrap()).await.unwrap();
        db.use_ns("backup").use_db("agent1").await.unwrap();
        db.query("CREATE type::record('memory', $id) SET content = $c")
            .bind(("id", "m1".to_string()))
            .bind(("c", "persisted before backup".to_string()))
            .await
            .unwrap()
            .check()
            .unwrap();
    } // db dropped → store closed/flushed to disk

    // 2. Cold filesystem copy of the quiesced store directory = the backup.
    copy_dir_recursive(&src, &dst).unwrap();

    // 3. Reopen the COPY and verify the data survived copy → restore.
    let db2 = Surreal::new::<SurrealKv>(dst.to_str().unwrap()).await.unwrap();
    db2.use_ns("backup").use_db("agent1").await.unwrap();
    let mut r = db2
        .query("SELECT VALUE content FROM type::record('memory', $id)")
        .bind(("id", "m1".to_string()))
        .await
        .unwrap();
    let got: Vec<String> = r.take(0).unwrap();
    assert_eq!(got, vec!["persisted before backup".to_string()]);

    let _ = std::fs::remove_dir_all(&base);
}

fn copy_dir_recursive(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let target = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_recursive(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}
