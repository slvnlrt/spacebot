# Architecture Technique : Pre-hooks & Déduplication

## 1. Gestion de l'État d'Injection (Déduplication)

Contrairement à une base de données globale, l'état d'injection (`last_injected_turn`) est **spécifique à chaque canal de conversation** (Channel). Si on l'ajoute directement dans la table `memories`, on crée des conflits entre les différents channels (Discord, Slack, etc.).

**Décision Architecturale :** L'état de déduplication vivra **en mémoire vive (RAM)** au sein de la structure `Channel` (`src/agent/channel.rs`), puisqu'il doit de toute façon être réinitialisé à chaque compaction.

```rust
// Dans src/agent/channel.rs
pub struct ChannelInjectionState {
    /// memory_id -> turn_number
    pub injected_ids: HashMap<Uuid, usize>,
    /// Buffer des embeddings injectés dans la session courante pour le filtre sémantique
    pub semantic_buffer: Vec<Vec<f32>>, 
}

pub struct Channel {
    // ... champs existants ...
    pub(crate) current_turn: usize,
    pub(crate) injection_state: ChannelInjectionState,
}
```

## 2. Le Moteur de Pre-hook (0 LLM)

Le pre-hook sera une nouvelle méthode dans `Channel` ou un module dédié `src/memory/injection.rs` qui s'exécute avant `agent.prompt()`.

### A. SQL Pre-hook (SQLite)
Requête ultra-rapide (< 5ms) pour récupérer le fond permanent :
- `MemoryType::Identity` (toujours)
- `importance > 0.8` (Important)
- `created_at > NOW() - 1h` (Recent)

### B. Vector Pre-hook (LanceDB)
1. Génération de l'embedding du message utilisateur via `FastEmbed` (déjà présent dans `src/memory/embedding.rs`).
2. Recherche HNSW dans LanceDB (Top 20).

## 3. Le Moteur de Déduplication

Avant d'injecter les résultats des pre-hooks dans le contexte du LLM, ils passent par deux filtres :

1. **Filtre ID Exact :** 
   `if injection_state.injected_ids.contains_key(&memory.id) { continue; }`
   *(Note : La gestion du flag `is_dirty` nécessitera d'écouter les événements de mise à jour du graphe pour invalider le cache du channel).*

2. **Filtre Sémantique (Cosinus) :**
   Calcul de la similarité cosinus entre l'embedding candidat et ceux du `semantic_buffer`.
   `if max_cosine_similarity > 0.85 { continue; }`

## 4. Intégration au Pipeline Existant

Dans `src/agent/channel.rs` (`handle_message`) :

```rust
// 1. Pre-hooks & Déduplication
let context_delta = self.compute_memory_injection(&user_message).await?;

// 2. Injection dans l'historique (en tant que System Message ou Context Block)
if !context_delta.is_empty() {
    self.history.push(Message::system(&format!("Context: {}", context_delta)));
}

// 3. Appel LLM standard
let response = agent.prompt(&user_message).with_history(&mut self.history).await?;

// 4. Post-hook
self.update_injection_state(context_delta_memories);
self.current_turn += 1;
```

Lors de la compaction (`src/agent/compactor.rs`), un événement sera envoyé au Channel pour vider son `injection_state`.
