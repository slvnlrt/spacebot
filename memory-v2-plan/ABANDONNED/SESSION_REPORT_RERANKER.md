# Rapport d'implémentation — Memory Injection V3 (Reranker + Pipeline)

Date : 2026-02-28
Branche : `merge/upstream-main-2026-02-24`

---

## 1. Objectif

Implémenter le plan `memory-v2-plan/IMPLEMENTATION 3/PLAN_RERANKER.md` en deux parties :

- **Partie A** : Intégrer un cross-encoder reranker (`JINARerankerV2BaseMultiligual` via fastembed) comme filtre de pertinence post-RRF dans `compute_memory_injection`
- **Partie B** : Cinq optimisations de pipeline pour réduire la latence et les round-trips inutiles

---

## 2. Fichiers modifiés

### `src/memory/store.rs`
**Ajout** : méthode `load_batch` — charge N memories en une seule requête SQLite `WHERE id IN (?, ?, ...)` au lieu de N appels individuels à `store.load()`. Les IDs sans correspondance sont omis silencieusement.

### `src/memory/lance.rs`
**Ajout** :
- Struct `VectorSearchResult` (module scope) — encapsule `memory_id`, `distance` et `embedding` retournés par LanceDB pour chaque résultat HNSW
- `vector_search` modifié pour retourner `Vec<VectorSearchResult>` au lieu de `Vec<(String, f32)>` — inclut la colonne `embedding` dans la réponse Arrow pour éviter un second fetch
- `get_embedding` (méthode existante, non modifiée par nous)
- `get_embeddings_batch` — fetch d'embeddings pour N IDs en un seul filtre `only_if("id IN (...)")`, évite N round-trips LanceDB individuels
- `find_similar` adapté à `VectorSearchResult`

### `src/memory/types.rs`
**Ajout** :
- Enum `SourceSignal` (`FtsOnly`, `VectorOnly`, `Both`) — indique quels signaux de recherche ont produit un résultat, tracé à travers la fusion RRF pour que le pipeline d'injection puisse prendre des décisions signal-aware
- Champ `source_signal: Option<SourceSignal>` dans `MemorySearchResult` (`#[serde(skip_serializing_if)]`)
- Champ `embedding: Option<Vec<f32>>` dans `MemorySearchResult` (`#[serde(skip)]`) — porte l'embedding pré-calculé par le search HNSW jusqu'aux consommateurs downstream

*Note : `SourceSignal` et `source_signal` dans `MemorySearchResult` ont été ajoutés lors d'une session précédente (V2), pas dans cette session.*

### `src/memory/search.rs`
**Réécriture de `hybrid_search`** :
- Parallélisation FTS + `embed_one` via `tokio::join!` — les deux ne dépendent que de la query string
- Filtre BM25 top-50% appliqué sur les IDs bruts avant chargement SQLite
- Batch load via `store.load_batch` pour tous les hits FTS + vector en un seul round-trip
- Construction d'un `embedding_by_id` map depuis les résultats LanceDB (embeddings déjà disponibles)
- `graph_results.sort_by(score)` ajouté avant RRF pour que le rang reflète la pondération par type de relation (voir section 4)
- `MemorySearchResult` inclut maintenant `source_signal` et `embedding`

**Ajout à `SearchConfig`** : `graph_seed_threshold: f32` et `graph_seed_limit: i64` (ces champs remplacent les constantes hardcodées `0.8` et `20` qui étaient dans le code upstream).

### `src/memory/reranker.rs` (nouveau fichier)
Cross-encoder reranker via fastembed :
- `Reranker` struct avec `Arc<fastembed::TextRerank>` + `Arc<Semaphore>(2)` pour limiter la concurrence
- `rerank` dispatche vers `spawn_blocking` pour ne pas bloquer le runtime async (ONNX est CPU-bound)
- Modèle : `JINARerankerV2BaseMultiligual` (typo dans l'enum fastembed upstream — "Multiligual" sans "n")
- Retourne `Vec<(usize, f32)>` — index original + score [0,1]

### `src/memory.rs`
- `pub mod reranker; pub use reranker::Reranker;`
- `pub use lance::{EmbeddingTable, VectorSearchResult};`

### `src/config.rs`
- Champ `rerank_min_score: f32` ajouté à `MemoryInjectionConfig` et `TomlMemoryInjectionConfig`
- Valeur par défaut : `0.5`
- Résolution dans les deux blocs de construction (defaults globaux + overrides per-agent)

### `src/lib.rs`
- Champ `reranker: Option<Arc<memory::Reranker>>` dans `AgentDeps`

### `src/main.rs`
- Initialisation du reranker après le modèle d'embedding (avec fallback gracieux sur erreur)
- `initialize_agents` reçoit `reranker: &Option<Arc<spacebot::memory::Reranker>>`
- `reranker: reranker.clone()` dans les constructions `AgentDeps`

### `src/agent/channel.rs`
**Modifications à `compute_memory_injection`** :
- `InjectionCandidate.embedding: Option<Vec<f32>>` — porte l'embedding pré-fetché depuis le résultat de search
- Pass 1 restructuré en trois phases :
  - 1a : dedup/filtres, collecte `PendingCandidate`
  - 1b : `get_embeddings_batch` pour les candidats sans embedding (FTS-only, pinned)
  - 1c : résolution d'embedding (pré-fetché > batch > calcul fallback), calcul cosine
- Pass 2 : chemin V3 (reranker) + fallback V2 (cosine floors différenciés par SourceSignal)
- `ScoredCandidate.rerank_score: Option<f32>` pour traçabilité

### `src/api/agents.rs`
- `reranker: None` dans les deux constructions `AgentDeps` du fichier (warmup trigger)

### `interface/src/api/client.ts`
- `rerank_min_score: number` dans `MemoryInjectionSection`, `MemoryInjectionUpdate`, `MemoryInjectionConfig`, `MemoryInjectionConfigUpdate`

### `interface/src/routes/Settings.tsx`
- State `rerankMinScore`, sync dans `useEffect`, inclus dans `handleSave`
- Slider UI dans le bloc "Contextual Search" (même style que "Context Min Score")

### `interface/src/routes/AgentConfig.tsx`
- `NumberStepper` "Rerank Min Score" dans la grille Contextual Search per-agent

---

## 3. Ce qui n'a pas été modifié (intentionnellement)

- L'algorithme RRF lui-même (inchangé dans sa logique de fusion)
- La déduplication sémantique (passage post-rerank)
- Le système de tasks, workers, compaction — hors scope
- `get_embedding` (méthode unitaire existante dans `lance.rs`) — conservée telle quelle

---

## 4. Section erreur : `ScoredMemory.score` et le code upstream supprimé

### Ce qui s'est passé

Le compilateur signalait un warning : `field score is never read` sur `struct ScoredMemory`. J'ai conclu à tort que ce champ était du "dead code de notre fait" et l'ai supprimé — emportant avec lui le calcul `type_multiplier` dans `traverse_graph`, qui est du code upstream :

```rust
// Upstream — supprimé par erreur
let type_multiplier = match assoc.relation_type {
    RelationType::Updates => 1.5,
    RelationType::CausedBy | RelationType::ResultOf => 1.3,
    RelationType::RelatedTo => 1.0,
    RelationType::Contradicts => 0.5,
    RelationType::PartOf => 0.8,
};
let score = memory.importance as f64 * assoc.weight as f64 * type_multiplier;
results.push(ScoredMemory { memory: memory.clone(), score });
```

### La vraie cause du warning

Le warning était causé par **notre propre code d'une session précédente**, mais pas de la façon naïve supposée.

**Upstream** : `reciprocal_rank_fusion` retourne `Vec<ScoredMemory>`. Le champ `score` dans `ScoredMemory` *est* lu — c'est le score RRF accumulé que le code downstream consomme.

**Notre session V2** : nous avons changé `reciprocal_rank_fusion` pour retourner `Vec<FusedMemory>` (une struct distincte avec `in_fts`/`in_vector` pour tracker `SourceSignal`). Ce faisant, le score RRF va maintenant dans `FusedMemory.score`, et `ScoredMemory.score` (score pré-fusion de la source individuelle) n'est plus lu par RRF. D'où le warning.

Le warning était donc **techniquement de notre fait** (c'est notre changement de la signature de retour de RRF qui l'a rendu dead), mais le champ et sa computation dans `traverse_graph` restaient du code upstream valide et intentionnel.

### Conséquences

Le code de pondération par type de relation dans `traverse_graph` pondère la pertinence des mémoires adjacentes selon leur type de lien (`Updates=1.5x`, `Contradicts=0.5x`, etc.). Ce score est maintenant utilisé pour trier `graph_results` avant RRF (le tri était manquant dans upstream, probablement un oubli — FTS et vector arrivent déjà triés par score, graph ne l'était pas).

### Correction appliquée

1. Restauration de `ScoredMemory.score` et de la computation `type_multiplier` dans `traverse_graph` (identique à upstream)
2. Ajout de `graph_results.sort_by(|a, b| b.score.total_cmp(&a.score))` avant RRF — utilise réellement le score, élimine le warning, et est cohérent avec les deux autres sources

---

## 5. État final

```
cargo build  →  Finished — 0 errors, 0 warnings
cargo test --lib  →  203 passed, 0 failed, 2 ignored
```

---

## 6. Diff résumé vs upstream/main

| Fichier | Nature des changements |
|---|---|
| `src/memory/store.rs` | +`load_batch`, +`get_recent_since` (session précédente) |
| `src/memory/lance.rs` | +`VectorSearchResult`, `vector_search` retourne la struct, +`get_embeddings_batch`, +`get_embedding` |
| `src/memory/types.rs` | +`SourceSignal`, +champs `source_signal` et `embedding` dans `MemorySearchResult` |
| `src/memory/search.rs` | Réécriture `hybrid_search` (parallel FTS/embed, batch load, embeddings), +champs `SearchConfig`, `FusedMemory`, `graph_results.sort_by` |
| `src/memory/reranker.rs` | Nouveau fichier |
| `src/memory.rs` | +exports |
| `src/config.rs` | +`rerank_min_score` |
| `src/lib.rs` | +`reranker` dans `AgentDeps` |
| `src/main.rs` | +init reranker |
| `src/api/agents.rs` | +`reranker: None` |
| `src/agent/channel.rs` | Réécriture `compute_memory_injection` (V2+V3 injection pipeline) |
| `interface/src/api/client.ts` | +`rerank_min_score` dans les interfaces |
| `interface/src/routes/Settings.tsx` | +slider `rerank_min_score` |
| `interface/src/routes/AgentConfig.tsx` | +stepper `rerank_min_score` |

Les changements dans `src/agent/channel.rs` sont largement issus de sessions précédentes (V2 injection pipeline). Cette session a ajouté le Pass 1 restructuré en 3 phases et le chemin V3 reranker dans Pass 2.
