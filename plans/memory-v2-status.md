# Memory V2 - Statut d'Implémentation

## Résumé

L'implémentation de Memory V2 (pre-hook systématique avec déduplication) est **terminée**. L'optimisation `get_embedding()` a été ajoutée pour éviter le recalcul des embeddings.

## Ce qui a été implémenté

### 1. Structure de données (`src/agent/channel.rs`)

```rust
pub struct ChannelInjectionState {
    pub injected_ids: HashMap<String, usize>,  // memory_id -> turn_number
    pub semantic_buffer: Vec<Vec<f32>>,         // embeddings injectés
}
```

Ajouté à `Channel` :
- `current_turn: usize` - compteur de tours
- `injection_state: ChannelInjectionState` - état de déduplication

### 2. Méthode `compute_memory_injection` (`src/agent/channel.rs`)

Pipeline complet :
1. SQL Pre-hook : Identity + Important (>0.8) + Recent (<1h)
2. Vector Pre-hook : recherche hybride sur le message utilisateur
3. Déduplication : filtre ID exact + similarité sémantique (cosine > 0.85)
4. Retourne un contexte formaté pour injection

### 3. Intégration dans le pipeline

- `handle_message` : appelle `compute_memory_injection` avant `run_agent_turn`
- `handle_message_batch` : appelle sur `combined_text`
- Skip pour messages système (`source == "system"`)
- `run_agent_turn` accepte `injected_context: Option<String>`
- Injection silencieuse dans l'historique cloné (non persisté)

### 4. Fonctions utilitaires ajoutées

**`src/memory/embedding.rs`** :
- `cosine_similarity(a: &[f32], b: &[f32]) -> f32`
- `is_semantically_duplicate(embedding, buffer, threshold) -> bool`

**`src/memory/store.rs`** :
- `get_recent_since(since: DateTime<Utc>, limit: i64) -> Result<Vec<Memory>>`

### 5. Optimisation `get_embedding()` ✅ NOUVEAU

**`src/memory/lance.rs`** :
```rust
/// Retrieve an embedding by memory ID.
/// Returns None if the memory is not found in the table.
pub async fn get_embedding(&self, memory_id: &str) -> Result<Option<Vec<f32>>>
```

**`src/agent/channel.rs`** - Utilisation avec fallback :
```rust
let embedding = match memory_search.embedding_table().get_embedding(&memory.id).await {
    Ok(Some(emb)) => emb,  // Récupéré depuis LanceDB
    Ok(None) => { /* fallback: calculer */ }
    Err(_) => { /* fallback: calculer */ }
};
```

## Tâches restantes

### Priorité 1 - Configuration (valeurs hardcodées)

**Fichier** : `src/agent/channel.rs:936-979`

| Variable | Valeur actuelle | Emplacement |
|----------|-----------------|-------------|
| `recent_threshold` | 1 heure | Ligne 936 |
| `identity_limit` | 10 | Ligne 941 |
| `important_limit` | 10 | Ligne 946 |
| `recent_limit` | 10 | Ligne 951 |
| `vector_search_limit` | 20 | Ligne 956 |
| `context_window_depth` | 50 | Ligne 938 |
| `semantic_threshold` | 0.85 | Ligne 979 |

### 3. Optimisation VecDeque ✅ CORRIGÉ

`semantic_buffer` utilisait `Vec::remove(0)` qui est O(n). Remplacé par `VecDeque` avec `push_back()` et `pop_front()` pour O(1).

## Configuration à brancher

### Structure proposée

Ajouter à `RuntimeConfig` et au schema de configuration :

```rust
// Dans src/config.rs
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryInjectionConfig {
    /// Hours to look back for "recent" memories.
    #[serde(default = "default_recent_threshold_hours")]
    pub recent_threshold_hours: i64,
    
    /// Maximum memories to fetch for Identity type.
    #[serde(default = "default_identity_limit")]
    pub identity_limit: i64,
    
    /// Maximum high-importance memories to fetch.
    #[serde(default = "default_important_limit")]
    pub important_limit: i64,
    
    /// Maximum recent memories to fetch.
    #[serde(default = "default_recent_limit")]
    pub recent_limit: i64,
    
    /// Maximum results from vector search.
    #[serde(default = "default_vector_search_limit")]
    pub vector_search_limit: usize,
    
    /// Number of turns before a memory can be re-injected.
    #[serde(default = "default_context_window_depth")]
    pub context_window_depth: usize,
    
    /// Cosine similarity threshold for semantic deduplication.
    #[serde(default = "default_semantic_threshold")]
    pub semantic_threshold: f32,
    
    /// Importance threshold for "high importance" memories.
    #[serde(default = "default_importance_threshold")]
    pub importance_threshold: f32,
}

fn default_recent_threshold_hours() -> i64 { 1 }
fn default_identity_limit() -> i64 { 10 }
fn default_important_limit() -> i64 { 10 }
fn default_recent_limit() -> i64 { 10 }
fn default_vector_search_limit() -> usize { 20 }
fn default_context_window_depth() -> usize { 50 }
fn default_semantic_threshold() -> f32 { 0.85 }
fn default_importance_threshold() -> f32 { 0.8 }
```

### Modification de RuntimeConfig

```rust
pub struct RuntimeConfig {
    // ... existing fields ...
    pub memory_injection: ArcSwap<MemoryInjectionConfig>,
}
```

### Modification de compute_memory_injection

```rust
async fn compute_memory_injection(&mut self, user_text: &str) -> Option<String> {
    let config = self.deps.runtime_config.memory_injection.load();
    
    let recent_threshold = chrono::Utc::now() 
        - chrono::Duration::hours(config.recent_threshold_hours);
    
    // Use config.identity_limit, config.important_limit, etc.
    // Use config.context_window_depth
    // Use config.semantic_threshold
}
```

## Tâches pour le prochain agent

### Priorité 1 - Configuration

1. **Créer `MemoryInjectionConfig`** dans `src/config.rs`
   - Définir la struct avec defaults
   - Ajouter au schema JSON (pour l'UI)

2. **Ajouter à `RuntimeConfig`**
   - Champ `memory_injection: ArcSwap<MemoryInjectionConfig>`
   - Initialisation dans `new()`
   - Reload dans `reload_config()`

3. **Modifier `compute_memory_injection`**
   - Lire la config depuis `self.deps.runtime_config.memory_injection.load()`
   - Remplacer toutes les valeurs hardcodées

### Priorité 3 - UI

1. **Ajouter les contrôles dans l'interface** (`interface/src/routes/Settings.tsx`)
   - Sliders pour les seuils (semantic_threshold, importance_threshold)
   - Number inputs pour les limites
   - Input pour recent_threshold_hours

2. **Ajouter les labels et descriptions**
   - Expliquer l'impact de chaque paramètre

### Priorité 4 - Optimisation

1. **Remplacer `Vec` par `VecDeque`** pour `semantic_buffer`
   - Modifier `ChannelInjectionState`
   - Utiliser `push_back()` et `pop_front()`

## Fichiers modifiés

| Fichier | Changements |
|---------|-------------|
| `src/agent/channel.rs` | `ChannelInjectionState`, `compute_memory_injection`, intégration pipeline, utilisation de `get_embedding()` |
| `src/memory/store.rs` | `get_recent_since()` |
| `src/memory/embedding.rs` | `cosine_similarity()`, `is_semantically_duplicate()` |
| `src/memory/lance.rs` | `get_embedding()` - nouvelle méthode pour récupérer un embedding par ID |
| `src/memory.rs` | Exports des nouvelles fonctions |

## Tests à écrire

1. `test_cosine_similarity_identical` - doit retourner 1.0
2. `test_cosine_similarity_orthogonal` - doit retourner 0.0
3. `test_deduplication_exact_id` - même ID filtré
4. `test_deduplication_semantic` - similarité > threshold filtré
5. `test_channel_injection_state_updates` - état mis à jour après injection
6. `test_get_embedding_from_lancedb` - récupération embedding existant
