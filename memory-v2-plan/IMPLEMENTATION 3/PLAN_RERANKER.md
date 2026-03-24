# Plan — Memory Injection V3 : Cross-Encoder Reranker + Pipeline Perf

**Branch cible :** PR depuis `merge/upstream-main-2026-02-24`
**Date :** 2026-02-28
**Statut :** Plan approuve, pas encore implemente

---

## Contexte

La V2 utilise un pipeline retrieve-then-filter :
1. Hybrid search (vector HNSW + FTS BM25 + RRF fusion, k=60)
2. Cosine bi-encoder post-filter avec seuils differencies par `SourceSignal`

Ce systeme fonctionne mais repose sur des **seuils magiques** calibres pour
all-MiniLM-L6-v2 (384 dims) :
- FtsOnly → 0.45, Both → 0.50, VectorOnly → dynamic (>= 0.60)
- BM25 top-50% pre-filter dans `hybrid_search`

Probleme : les metriques BM25 et cosine bi-encoder sont de nature differente.
Un changement de modele d'embedding, de langue, ou de longueur de texte
invalide tous les seuils.

De plus, le pipeline presente plusieurs goulots de performance :
- N round-trips LanceDB sequentiels pour re-fetcher des embeddings deja calcules
- FTS et vector search executees en sequence au lieu d'etre parallelisees
- N round-trips SQLite sequentiels pour charger les memories par ID

---

## Partie A : Cross-Encoder Reranker

### Solution : Retrieve → Rerank

Architecture standard RAG haute precision :

```
User message
    |
    v
hybrid_search(user_text, config)               (optimise, voir Partie B)
    |  -> FTS + embed_one en parallele (tokio::join!)
    |  -> vector search avec embeddings inclus
    |  -> batch SQLite load
    |  -> RRF fusion
    |  -> Top N candidats (N=20, high recall)
    |
    v
Cap a MAX_RERANK_CANDIDATES (20)               (garde-fou)
    |
    v
cross_encoder.rerank(user_text, candidates)    (NOUVEAU)
    |  -> score de pertinence calibre [0, 1]
    |
    v
filter: score > rerank_min_score (0.5)         (seuil unique, configurable)
    |
    v
dedup inter-tours (injection_state)            (inchange)
    |
    v
budget enforcement (max_total, pinned-first)   (inchange)
    |
    v
format [Context from memory] block             (inchange)
```

### Modele retenu

**`JINARerankerV2BaseMultilingual`** (jinaai/jina-reranker-v2-base-multilingual)

| Critere          | Valeur                                    |
|------------------|-------------------------------------------|
| Taille           | ~280 MB (ONNX)                            |
| Langues          | Multilingue (FR + EN natif)               |
| Latence          | ~5-15ms / paire sur CPU (ONNX via ort)    |
| Pour 20 candidats| ~100-300ms total                           |
| Backend          | fastembed v4 (`TextRerank`, deja en dep)  |
| Enum             | `RerankerModel::JINARerankerV2BaseMultilingual` |

Pas de nouvelle dependance — `fastembed = "4"` est deja dans Cargo.toml.

### Nouveau module : `src/memory/reranker.rs`

```rust
//! Cross-encoder reranking via fastembed.

use crate::error::{LlmError, Result};

use tokio::sync::Semaphore;

use std::path::Path;
use std::sync::Arc;

/// Hard cap on concurrent rerank operations.
///
/// Each rerank call is CPU-bound (~100-300ms for 20 candidates via ONNX).
/// Without a cap, multiple channels reranking simultaneously would saturate
/// the tokio blocking pool and spike latency for everyone.
const MAX_CONCURRENT_RERANKS: usize = 2;

/// Cross-encoder reranker for post-retrieval relevance scoring.
///
/// Jointly encodes (query, document) pairs to produce calibrated relevance
/// scores, unlike bi-encoder cosine which encodes query and document
/// independently. This makes it accurate for proper nouns and short entities
/// where bi-encoder embeddings are noisy.
///
/// `TextRerank` is Send + Sync, but reranking is CPU-bound (ONNX inference),
/// so calls are dispatched via `spawn_blocking` to avoid stalling the
/// async runtime. A semaphore caps concurrent reranks to prevent blocking
/// pool saturation under load.
pub struct Reranker {
    model: Arc<fastembed::TextRerank>,
    concurrency_limit: Arc<Semaphore>,
}

impl Reranker {
    /// Create a new reranker, downloading model files to `cache_dir` if needed.
    pub fn new(cache_dir: &Path) -> Result<Self> {
        let options = fastembed::RerankInitOptions::new(
            fastembed::RerankerModel::JINARerankerV2BaseMultilingual,
        )
        .with_cache_dir(cache_dir.to_path_buf())
        .with_show_download_progress(true);

        let model = fastembed::TextRerank::try_new(options)
            .map_err(|e| LlmError::EmbeddingFailed(e.to_string()))?;

        Ok(Self {
            model: Arc::new(model),
            concurrency_limit: Arc::new(Semaphore::new(MAX_CONCURRENT_RERANKS)),
        })
    }

    /// Rerank documents against a query.
    ///
    /// Returns `(original_index, score)` pairs sorted by descending relevance.
    /// Score is in [0, 1] — higher means more relevant to the query.
    pub async fn rerank(
        &self,
        query: &str,
        documents: &[String],
    ) -> Result<Vec<(usize, f32)>> {
        if documents.is_empty() {
            return Ok(Vec::new());
        }

        let _permit = self.concurrency_limit.acquire().await
            .map_err(|_| anyhow::anyhow!("reranker semaphore closed"))?;

        let query = query.to_string();
        let documents = documents.to_vec();
        let model = self.model.clone();

        tokio::task::spawn_blocking(move || {
            let document_refs: Vec<&str> = documents.iter().map(|s| s.as_str()).collect();
            let results = model
                .rerank(&query, document_refs, false, None)
                .map_err(|e| LlmError::EmbeddingFailed(e.to_string()))?;

            Ok(results.into_iter().map(|r| (r.index, r.score)).collect())
        })
        .await
        .map_err(|e| crate::Error::Other(anyhow::anyhow!("rerank task failed: {e}")))?
    }
}
```

### Modifications dans `src/memory.rs`

```rust
pub mod reranker;

pub use reranker::Reranker;
```

### Modifications dans `src/agent/channel.rs` — `compute_memory_injection`

#### Nouveau champ sur `ScoredCandidate`

Le score cross-encoder est semantiquement different du cosine bi-encoder.
On ajoute un champ dedie au lieu de reutiliser `cosine` :

```rust
struct ScoredCandidate {
    memory: crate::memory::Memory,
    source: InjectionSource,
    source_signal: Option<SourceSignal>,
    embedding: Vec<f32>,
    /// Cosine similarity to the user query (bi-encoder). Used for semantic
    /// dedup, NOT for relevance filtering when the reranker is active.
    cosine: Option<f32>,
    /// Cross-encoder relevance score [0, 1]. None when reranker is absent
    /// (V2 fallback) or for pinned candidates.
    rerank_score: Option<f32>,
}
```

`cosine` continue a alimenter `is_semantically_duplicate` (dedup intra-turn).
`rerank_score` alimente le filtre de pertinence quand le reranker est actif.

#### Cap explicite avant rerank

FTS + Vector + Graph peuvent produire plus de 20 candidats apres RRF.
On cap avant d'appeler le reranker pour borner le cout CPU :

```rust
const MAX_RERANK_CANDIDATES: usize = 20;

scored_candidates.sort_by(|a, b| {
    let score_a = a.cosine.unwrap_or(0.0);
    let score_b = b.cosine.unwrap_or(0.0);
    score_b.total_cmp(&score_a)
});
scored_candidates.truncate(MAX_RERANK_CANDIDATES);
```

#### Ce qui CHANGE (Pass 2 remplace par rerank)

Le Pass 2 actuel (~40 lignes : `SourceSignal`, `cosine_floor`, `dynamic_threshold`,
`ABSOLUTE_MIN_COSINE`) est **entierement remplace** par :

```rust
if let Some(reranker) = &self.deps.reranker {
    // V3: cross-encoder reranking
    let documents: Vec<String> = scored_candidates
        .iter()
        .map(|candidate| candidate.memory.content.clone())
        .collect();

    match reranker.rerank(user_text, &documents).await {
        Ok(reranked) => {
            let rerank_threshold = config.rerank_min_score;

            for (index, score) in reranked {
                scored_candidates[index].rerank_score = Some(score);
            }

            scored_candidates.retain(|candidate| {
                if candidate.source == InjectionSource::Pinned {
                    return true;
                }
                candidate.rerank_score.is_some_and(|score| score >= rerank_threshold)
            });

            tracing::debug!(
                channel_id = %self.id,
                rerank_threshold,
                remaining = scored_candidates.len(),
                "cross-encoder rerank filter"
            );
        }
        Err(error) => {
            tracing::warn!(%error, channel_id = %self.id, "reranker failed, falling back to cosine filter");
            // Fall through to V2 cosine filter below
        }
    }
} else {
    // V2 fallback: differentiated cosine floors by SourceSignal
    const ABSOLUTE_MIN_COSINE: f32 = 0.60;
    let dynamic_threshold = (max_cosine * contextual_min_score).max(ABSOLUTE_MIN_COSINE);

    scored_candidates.retain(|candidate| {
        let Some(similarity) = candidate.cosine else { return true };
        let effective_threshold: f32 = match candidate.source_signal {
            Some(SourceSignal::FtsOnly) => 0.45,
            Some(SourceSignal::Both) => 0.50,
            _ => dynamic_threshold,
        };
        similarity >= effective_threshold
    });
}

// Semantic dedup runs after both V3 and V2 paths (uses cosine, not rerank_score).
```

#### Ce qui DISPARAIT (V3 path uniquement)

| Element                    | Raison                                       |
|---------------------------|-----------------------------------------------|
| `cosine_floor` match       | Remplace par seuil unique cross-encoder       |
| `dynamic_threshold`        | Idem                                          |
| `ABSOLUTE_MIN_COSINE`      | Idem                                          |
| BM25 top-50% pre-filter    | Cross-encoder eliminera les faux positifs FTS |

Note : `SourceSignal` et `FusedMemory.in_fts/in_vector` dans `search.rs`
restent — zero cout, utiles pour le debug, le logging, et le fallback V2.
Le BM25 top-50% pre-filter dans `search.rs` reste egalement (utile pour
le fallback V2 et pour reduire le bruit dans les logs RRF).

#### Ce qui NE CHANGE PAS

| Sous-systeme                       | Raison                                    |
|------------------------------------|-------------------------------------------|
| Pass 1 (embed + cosine compute)    | Alimente la dedup semantique, pas le filtre|
| Dedup inter-tours (`injected_ids`) | Controle la frequence, pas la qualite     |
| Semantic buffer (`is_semantically_duplicate`) | Evite les redondances dans un meme turn |
| Budget enforcement (`max_total`)   | Plafond architectural                     |
| Pinned types (ambient context)     | Pas soumis au reranking                   |
| Structured output (2 sections)     | Presentation, pas ranking                 |
| Injection persistence (guard)      | Orthogonal                                |
| Block pruning (`prune_old_injection_blocks`) | Orthogonal                        |
| Compactor filter (skip blocks)     | Orthogonal                                |
| `ChannelInjectionState`            | Orthogonal                                |

### Initialisation du Reranker

Dans `AgentDeps`, a cote de `EmbeddingModel` :

```rust
pub struct AgentDeps {
    // ... existant ...
    pub reranker: Option<Arc<Reranker>>,
}
```

Construit au demarrage (`main.rs`), apres `EmbeddingModel::new()`.
`Option` pour graceful degradation si le modele n'est pas disponible :

```rust
let reranker = match Reranker::new(&cache_dir) {
    Ok(reranker) => {
        tracing::info!("cross-encoder reranker loaded (JINA v2 multilingual)");
        Some(Arc::new(reranker))
    }
    Err(error) => {
        tracing::warn!(%error, "cross-encoder reranker unavailable, using cosine fallback");
        None
    }
};
```

### Configuration

#### Nouveau champ dans `MemoryInjectionConfig`

```rust
/// Minimum cross-encoder score for a memory to be injected.
/// Only used when the reranker is available. Range: 0.0 - 1.0.
#[serde(default = "default_rerank_min_score")]
pub rerank_min_score: f32,
```

Avec `fn default_rerank_min_score() -> f32 { 0.5 }`.

#### Champs conserves pour le fallback V2

| Champ                    | Statut                                         |
|--------------------------|------------------------------------------------|
| `contextual_min_score`   | Actif en fallback V2 (ratio cosine)            |
| UI slider pour min_score | Garder, actif quand reranker absent             |

Nouveau slider UI pour `rerank_min_score`, affiche uniquement quand
le reranker est actif (l'API peut exposer un champ `reranker_available: bool`).

### Fallback / Graceful Degradation

Trois niveaux :

1. **Reranker disponible, rerank reussit** → V3 path (cross-encoder scoring)
2. **Reranker disponible, rerank echoue** → V2 fallback (cosine floors), log warn
3. **Reranker absent** (`deps.reranker == None`) → V2 path direct

Le fallback est automatique et transparent. Pas besoin de config pour
basculer — la presence du modele ONNX suffit.

---

## Partie B : Optimisations du Pipeline de Recherche

Ces optimisations s'integrent dans la meme PR car elles touchent les memes
fichiers (`lance.rs`, `search.rs`, `store.rs`, `channel.rs`) et reduisent
la latence end-to-end du pipeline d'injection.

### B.1 — `vector_search` retourne les embeddings (lance.rs)

**Probleme :** `vector_search` retourne `Vec<(String, f32)>` (id + distance).
LanceDB a deja calcule les embeddings pour le nearest-neighbor search, mais
la fonction les jette. Ensuite, Pass 1 dans `compute_memory_injection`
refait N appels sequentiels `get_embedding(id)` pour les re-fetcher un par un.

**Avant :** `lance.rs:181-231`
```rust
pub async fn vector_search(
    &self,
    query_embedding: &[f32],
    limit: usize,
) -> Result<Vec<(String, f32)>> {
    // ... execute query ...
    // Extrait seulement id + _distance, jette l'embedding
    matches.push((id, distance));
}
```

**Apres :**
```rust
/// Result from a vector similarity search: memory ID, distance, and the
/// embedding vector that LanceDB already computed for the HNSW comparison.
pub struct VectorSearchResult {
    pub memory_id: String,
    pub distance: f32,
    pub embedding: Vec<f32>,
}

pub async fn vector_search(
    &self,
    query_embedding: &[f32],
    limit: usize,
) -> Result<Vec<VectorSearchResult>> {
    // ... execute query (inchange) ...
    for batch in results {
        if let (Some(id_col), Some(dist_col), Some(embedding_col)) = (
            batch.column_by_name("id"),
            batch.column_by_name("_distance"),
            batch.column_by_name("embedding"),
        ) {
            let ids: &arrow_array::StringArray = id_col.as_string::<i32>();
            let dists: &arrow_array::PrimitiveArray<Float32Type> = dist_col.as_primitive();
            let embeddings = embedding_col
                .as_any()
                .downcast_ref::<arrow_array::FixedSizeListArray>();

            for i in 0..ids.len() {
                if ids.is_valid(i) && dists.is_valid(i) {
                    let embedding = embeddings
                        .and_then(|list| {
                            let values = list.value(i);
                            let floats = values.as_primitive::<Float32Type>();
                            Some(floats.values().to_vec())
                        })
                        .unwrap_or_default();

                    matches.push(VectorSearchResult {
                        memory_id: ids.value(i).to_string(),
                        distance: dists.value(i),
                        embedding,
                    });
                }
            }
        }
    }
    Ok(matches)
}
```

**Impact sur les consumers :** `search.rs:hybrid_search` utilise
`vector_search` — adapter le destructuring. Le nouveau `MemorySearchResult`
peut porter l'embedding en option pour que `compute_memory_injection` l'exploite.

**Gain :** Elimine N round-trips LanceDB dans Pass 1 pour les candidats
issus de vector search (la majorite). Seuls les candidats FTS-only et pinned
necessitent encore un fetch d'embedding separé.

### B.2 — Paralleliser FTS + embed_one dans hybrid_search (search.rs)

**Probleme :** `search.rs:147-233` execute sequentiellement :
1. `text_search(query, limit)` — attend
2. `embed_one(query)` — attend
3. `vector_search(&query_embedding, limit)` — attend

L'embedding du query et la recherche FTS sont independants.

**Avant :**
```rust
// 1. FTS (attend)
match self.embedding_table.text_search(query, limit).await { ... }

// 2. Embed query (attend)
let query_embedding = self.embedding_model.embed_one(query).await?;

// 3. Vector search (attend, depend de #2)
match self.embedding_table.vector_search(&query_embedding, limit).await { ... }
```

**Apres :**
```rust
// FTS et embed_one en parallele — pas de dependance entre eux.
let (fts_result, query_embedding_result) = tokio::join!(
    self.embedding_table.text_search(query, config.max_results_per_source),
    self.embedding_model.embed_one(query),
);

// FTS — traiter le resultat
match fts_result {
    Ok(fts_matches) => { /* inchange */ }
    Err(error) => { /* inchange */ }
}

// BM25 top-50% filter — inchange

// Vector search — depend de query_embedding, execute apres join!
let query_embedding = query_embedding_result?;
match self.embedding_table.vector_search(&query_embedding, config.max_results_per_source).await {
    Ok(vector_matches) => { /* adapter au nouveau VectorSearchResult */ }
    Err(error) => { /* inchange */ }
}
```

**Gain :** ~50-100ms economises (FTS et embed_one s'executent en parallele
au lieu de se bloquer mutuellement).

### B.3 — Batch SQLite load dans hybrid_search (store.rs + search.rs)

**Probleme :** Dans `hybrid_search`, chaque resultat FTS/vector declenche un
`store.load(&memory_id).await` separé — potentiellement 40+ queries SQLite
sequentielles (20 FTS + 20 vector).

**Avant :** `store.rs:95-110`
```rust
pub async fn load(&self, id: &str) -> Result<Option<Memory>> {
    sqlx::query("SELECT ... FROM memories WHERE id = ?")
        .bind(id)
        .fetch_optional(&self.pool).await
}
```

Appele en boucle dans `search.rs:175-183` et `search.rs:218-227` :
```rust
for (memory_id, score) in fts_matches {
    if let Some(memory) = self.store.load(&memory_id).await? { ... }
}
```

**Nouvelle methode sur `MemoryStore` :** `store.rs`
```rust
/// Load multiple memories by ID in a single query.
///
/// Returns only the memories that exist and are not forgotten.
/// Order is not guaranteed — caller must match by ID.
pub async fn load_batch(&self, ids: &[&str]) -> Result<Vec<Memory>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }

    // sqlx doesn't support binding a slice to IN (?), so build the
    // placeholders dynamically. IDs are UUIDs (safe for interpolation),
    // but we use bind anyway for consistency.
    let placeholders: String = ids.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
    let query_string = format!(
        r#"
        SELECT id, content, memory_type, importance, created_at, updated_at,
               last_accessed_at, access_count, source, channel_id, forgotten
        FROM memories
        WHERE id IN ({placeholders})
        "#,
    );

    let mut query = sqlx::query(&query_string);
    for id in ids {
        query = query.bind(id);
    }

    let rows = query
        .fetch_all(&self.pool)
        .await
        .context("failed to batch-load memories")?;

    Ok(rows.iter().map(|row| row_to_memory(row)).collect())
}
```

**Usage dans `search.rs` :** `hybrid_search` collecte d'abord tous les IDs,
puis fait un seul `load_batch`, puis filtre `!memory.forgotten` :

```rust
// Collect all IDs from FTS + vector results
let all_ids: Vec<&str> = fts_matches.iter().map(|(id, _)| id.as_str())
    .chain(vector_matches.iter().map(|result| result.memory_id.as_str()))
    .collect();

// Single batch load
let memories: Vec<Memory> = self.store.load_batch(&all_ids).await?;
let memory_map: HashMap<&str, Memory> = memories
    .into_iter()
    .filter(|memory| !memory.forgotten)
    .map(|memory| (memory.id.as_str(), memory))
    // NOTE: on ne peut pas emprunter memory.id et deplacer memory en meme temps.
    // Utiliser un HashMap<String, Memory> avec memory.id.clone() comme cle.
    .collect();

// Populate FTS results
for (memory_id, score) in fts_matches {
    if let Some(memory) = memory_map.get(memory_id.as_str()) {
        fts_results.push(ScoredMemory { memory: memory.clone(), score: score as f64 });
    }
}

// Populate vector results (avec embedding du B.1)
for result in vector_matches {
    if let Some(memory) = memory_map.get(result.memory_id.as_str()) {
        vector_results.push(ScoredMemory { memory: memory.clone(), score: (1.0 - result.distance) as f64 });
    }
}
```

**Gain :** 1 query SQLite au lieu de 40. Elimine la latence N × round-trip
SQLite (~1-5ms par query × 40 = 40-200ms → ~5ms).

### B.4 — Batch embedding fetch pour les candidats restants (lance.rs + channel.rs)

**Probleme :** Apres le fix B.1, les candidats vector ont deja leur embedding.
Mais les candidats FTS-only et pinned necessitent encore un `get_embedding(id)`
individuel — potentiellement 10-15 appels sequentiels.

**Nouvelle methode sur `EmbeddingTable` :** `lance.rs`
```rust
/// Fetch embeddings for multiple memory IDs in a single LanceDB query.
pub async fn get_embeddings_batch(
    &self,
    memory_ids: &[&str],
) -> Result<HashMap<String, Vec<f32>>> {
    if memory_ids.is_empty() {
        return Ok(HashMap::new());
    }

    use lancedb::query::{ExecutableQuery, QueryBase};

    // LanceDB SQL filter with IN clause.
    // IDs are UUIDs ([a-z0-9-]), safe for string interpolation.
    let id_list: String = memory_ids
        .iter()
        .map(|id| format!("'{id}'"))
        .collect::<Vec<_>>()
        .join(", ");

    let rows: Vec<arrow_array::RecordBatch> = self
        .table
        .query()
        .only_if(format!("id IN ({id_list})"))
        .execute()
        .await
        .map_err(|e| DbError::LanceDb(e.to_string()))?
        .try_collect()
        .await
        .map_err(|e| DbError::LanceDb(e.to_string()))?;

    let mut result = HashMap::new();
    for batch in rows {
        if let (Some(id_col), Some(embedding_col)) = (
            batch.column_by_name("id"),
            batch.column_by_name("embedding"),
        ) {
            let ids: &arrow_array::StringArray = id_col.as_string::<i32>();
            let list_array = embedding_col
                .as_any()
                .downcast_ref::<arrow_array::FixedSizeListArray>();

            if let Some(list_array) = list_array {
                for i in 0..ids.len() {
                    if ids.is_valid(i) {
                        let values = list_array.value(i);
                        let floats = values.as_primitive::<Float32Type>();
                        let embedding: Vec<f32> = floats.values().to_vec();
                        result.insert(ids.value(i).to_string(), embedding);
                    }
                }
            }
        }
    }

    Ok(result)
}
```

**Usage dans `channel.rs` :** `compute_memory_injection` Pass 1 collecte
les IDs des candidats qui n'ont pas d'embedding (FTS-only, pinned), puis
fait un seul `get_embeddings_batch`, puis assigne :

```rust
// Collect IDs that need embedding fetch (not already provided by vector search)
let ids_needing_embedding: Vec<&str> = candidates
    .iter()
    .filter(|candidate| candidate.embedding.is_none())
    .map(|candidate| candidate.memory.id.as_str())
    .collect();

if !ids_needing_embedding.is_empty() {
    let embedding_map = memory_search
        .embedding_table()
        .get_embeddings_batch(&ids_needing_embedding)
        .await?;

    for candidate in &mut candidates {
        if candidate.embedding.is_none() {
            if let Some(embedding) = embedding_map.get(&candidate.memory.id) {
                candidate.embedding = Some(embedding.clone());
            }
        }
    }
}
```

**Gain :** 1 LanceDB query au lieu de 10-15 sequentiels.

### B.5 — Propager les embeddings de vector_search a travers le pipeline

Pour que `compute_memory_injection` beneficie des embeddings du B.1, il faut
les propager a travers `MemorySearchResult`. Deux options :

**Option A : Champ optionnel sur MemorySearchResult** (recommandee)
```rust
pub struct MemorySearchResult {
    pub memory: Memory,
    pub score: f32,
    pub rank: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_signal: Option<SourceSignal>,
    /// Embedding vector, included when available from vector search.
    /// Avoids a second LanceDB round-trip in downstream consumers.
    #[serde(skip)]
    pub embedding: Option<Vec<f32>>,
}
```

`hybrid_search` remplit ce champ pour les candidats vector, `None` pour
FTS-only. `compute_memory_injection` l'utilise quand present, fetch
via batch pour les `None`.

**Option B : HashMap separé retourne par hybrid_search**
Plus propre architecturalement mais necessite de changer la signature.
Moins pratique pour les consumers existants.

**Choix : Option A.** Le champ est `#[serde(skip)]` donc invisible dans les
responses API. Zero impact sur les consumers existants qui ne l'utilisent pas.

---

## Recapitulatif des fichiers modifies

| Fichier | Partie A (reranker) | Partie B (perf) |
|---------|---------------------|-----------------|
| `src/memory/reranker.rs` | NOUVEAU | — |
| `src/memory.rs` | + `mod reranker` + re-export | — |
| `src/memory/lance.rs` | — | B.1 `vector_search` retourne embeddings, B.4 `get_embeddings_batch` |
| `src/memory/store.rs` | — | B.3 `load_batch` |
| `src/memory/search.rs` | — | B.2 `tokio::join!`, B.3 batch load, B.5 embedding propagation |
| `src/memory/types.rs` | — | B.5 `embedding: Option<Vec<f32>>` sur `MemorySearchResult` |
| `src/agent/channel.rs` | Rerank Pass 2, `ScoredCandidate.rerank_score` | B.4 batch fetch, B.5 utiliser embeddings pre-resolus |
| `src/config.rs` | `rerank_min_score` | — |
| `src/main.rs` (ou `lib.rs`) | Init `Reranker` dans `AgentDeps` | — |
| `interface/src/routes/Settings.tsx` | Slider `rerank_min_score` | — |

## Tests

### Unitaires reranker (reranker.rs)

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore] // Requires model download (~280 MB)
    async fn reranker_scores_relevant_higher() {
        let reranker = Reranker::new(Path::new("/tmp/fastembed-test")).unwrap();
        let results = reranker.rerank(
            "Jamie Pine",
            &[
                "Spacedrive is a VDFS created by Jamie Pine".into(),
                "I like Yirgacheffe coffee".into(),
                "Heat pump maintenance scheduled".into(),
            ],
        ).await.unwrap();

        let top = results.first().unwrap();
        assert_eq!(top.0, 0, "expected Jamie Pine doc at index 0 to rank first");
        assert!(top.1 > 0.5, "expected relevance > 0.5, got {}", top.1);
    }

    #[tokio::test]
    #[ignore]
    async fn reranker_handles_empty_documents() {
        let reranker = Reranker::new(Path::new("/tmp/fastembed-test")).unwrap();
        let results = reranker.rerank("query", &[]).await.unwrap();
        assert!(results.is_empty());
    }
}
```

### Unitaires perf (lance.rs, store.rs)

- `test_vector_search_returns_embeddings` : verifier que les embeddings ne sont pas vides
- `test_get_embeddings_batch` : fetch 3 IDs en un appel, verifier les dimensions
- `test_load_batch` : inserer 5 memories, load_batch 3, verifier le contenu
- `test_load_batch_empty` : appeler avec slice vide → Ok(vec![])

### Integration (channel.rs)

Les tests existants (`prompt_cancelled_rolls_back`, `prune_old_injection_blocks`,
etc.) ne sont pas affectes — ils testent la persistence et le pruning, pas le
scoring ni les performances.

## Estimation

| Tache                                      | Effort    |
|--------------------------------------------|-----------|
| **Partie A**                               |           |
| `src/memory/reranker.rs` + semaphore       | ~30 min   |
| Init dans AgentDeps + main.rs              | ~20 min   |
| `ScoredCandidate.rerank_score` + channel.rs| ~30 min   |
| Config (`rerank_min_score` + UI)           | ~20 min   |
| Tests reranker                             | ~15 min   |
| **Partie B**                               |           |
| B.1 `vector_search` + embeddings           | ~20 min   |
| B.2 `tokio::join!` dans hybrid_search      | ~10 min   |
| B.3 `load_batch` + integration search.rs   | ~30 min   |
| B.4 `get_embeddings_batch` + channel.rs    | ~20 min   |
| B.5 Propagation embedding MemorySearchResult| ~15 min  |
| Tests perf                                 | ~20 min   |
| **Total**                                  | **~4h**   |

## Gains de latence estimes

| Etape | Avant | Apres | Gain |
|-------|-------|-------|------|
| FTS + embed_one | ~100ms (sequentiel) | ~60ms (parallele) | -40ms |
| SQLite load × 40 | ~100ms | ~5ms (batch) | -95ms |
| Embedding fetch × 20 (vector) | ~200ms | 0ms (inclus dans vector_search) | -200ms |
| Embedding fetch × 10 (FTS-only) | ~100ms | ~10ms (batch) | -90ms |
| Cross-encoder rerank | 0ms | +150ms | +150ms |
| **Total pipeline** | **~500ms** | **~225ms** | **-275ms** |

Le reranker ajoute ~150ms mais les optimisations B.1-B.4 economisent ~425ms.
Le pipeline V3 complet est **plus rapide** que le pipeline V2 actuel malgre
l'ajout du cross-encoder.

## Risques

1. **Taille du modele JINA** (280 MB) : premier demarrage telecharge le modele.
   Mitigation : `with_show_download_progress(true)` + log info au demarrage.

2. **Latence reranker** (~100-300ms pour 20 candidats) : acceptable car la latence LLM
   domine (1-5s). Le semaphore (max 2 reranks concurrents) stabilise la
   latence sous charge.

3. **Saturation du pool blocking** : sans cap, N channels reranking
   simultanement saturent le pool tokio. Le `Semaphore(2)` dans `Reranker`
   serialise les appels au-dela de la limite. Les appels en attente sont
   suspendus (async), pas bloques.

4. **Qualite du reranker** : les cross-encoders sont generalement superieurs
   aux bi-encoders pour le scoring de pertinence, mais il faut valider
   empiriquement sur notre corpus (FR+EN, memoires courtes).
   Mitigation : fallback V2 toujours disponible.

5. **RAM** : +200-300 MB pour le modele ONNX en memoire.
   Acceptable pour un daemon serveur.

6. **`vector_search` colonne embedding** : LanceDB retourne par defaut toutes
   les colonnes dans un `query().nearest_to()`. Si un `select()` filtre les
   colonnes en amont, verifier que `embedding` est inclus. A tester.

---

## Ordre d'implementation recommande

1. **B.2** (`tokio::join!`) — 3 lignes, gain immediat, zero risque
2. **B.3** (`load_batch`) — nouvelle methode SQLite, integration simple
3. **B.1** (`vector_search` + embeddings) — changement d'API lance.rs
4. **B.5** (propagation embedding) — champ sur `MemorySearchResult`
5. **B.4** (batch embedding fetch) — utilise B.5, elimine le residu
6. **A** (reranker complet) — module isole, s'appuie sur le pipeline optimise

Chaque etape compile et passe les tests independamment.
