# Guide d'Onboarding pour Agent LLM : Architecture Spacebot & Projet Memory V2

**À l'attention du prochain Agent LLM :**
Si tu lis ce document, tu as été assigné pour implémenter le projet "Memory V2" dans le dépôt Spacebot. Ce guide contient tout le contexte architectural, les emplacements de code clés, et les pièges à éviter que nous avons découverts lors de la phase de conception.

---

## 1. Philosophie Globale de Spacebot

Spacebot est un système agentique en Rust (édition 2024) basé sur le framework **Rig**.
*   **Single Binary** : Pas de microservices. Tout tourne dans un seul processus via `tokio`.
*   **Délégation stricte** : Le `Channel` (qui parle à l'utilisateur) ne fait **jamais** de travail lourd. Il délègue aux `Branch` (pour réfléchir/chercher) et aux `Worker` (pour exécuter des tâches).
*   **Trois bases de données embarquées** :
    *   `SQLite` (via `sqlx`) : Données relationnelles et graphe causal des mémoires.
    *   `LanceDB` : Recherche vectorielle (HNSW) et Full-Text Search (Tantivy).
    *   `redb` : Clé-valeur pour la configuration et les secrets.

---

## 2. Cartographie du Code (Où chercher quoi)

Voici les fichiers que tu devras absolument consulter ou modifier :

*   **`src/agent/channel.rs`** : Le cœur du projet Memory V2. Contient la struct `Channel` (boucle d'événements) et `ChannelState` (état partagé avec les outils via `Arc<RwLock>`). C'est ici que tu implémenteras le pre-hook d'injection silencieuse.
*   **`src/memory/search.rs`** : Contient `MemorySearch` et la logique de recherche hybride (Vector + FTS + Graph + RRF). Tu devras l'utiliser pour récupérer les mémoires pertinentes avant le tour de l'LLM.
*   **`src/agent/compactor.rs`** : Gère la compaction du contexte quand il devient trop grand. Modifie l'historique de manière asynchrone.
*   **`src/memory/types.rs`** : Définition des types de mémoires (`Memory`, `MemoryType`, `RelationType`).
*   **`src/agent/branch.rs` & `src/agent/worker.rs`** : Les processus de délégation (utile pour comprendre comment le Channel interagit avec eux, mais tu ne devrais pas avoir à les modifier pour la Phase 1).

---

## 3. Focus sur `src/agent/channel.rs` (Ton terrain de jeu)

C'est ici que se passe la magie. Comprends bien ces mécanismes :

1.  **`Channel` vs `ChannelState`** :
    *   `Channel` possède la boucle d'événements (`run`). Ses méthodes comme `handle_message` prennent `&mut self`. C'est l'endroit idéal pour stocker un état local (comme `ChannelInjectionState`) sans avoir besoin de locks asynchrones.
    *   `ChannelState` est cloné et partagé avec les outils. Il contient l'historique sous forme de `Arc<RwLock<Vec<Message>>>`.
2.  **Le Coalescing (`handle_message_batch`)** :
    *   Spacebot regroupe les messages rapides de l'utilisateur en un seul tour LLM. **Attention :** Ton pre-hook de mémoire doit s'exécuter sur le texte combiné du batch, pas sur chaque petit message individuel.
3.  **L'Injection Silencieuse (`run_agent_turn`)** :
    *   Avant d'appeler l'LLM, le code fait : `let mut history = self.state.history.read().await.clone();`.
    *   C'est l'opportunité parfaite : tu peux injecter tes mémoires dans ce `clone` d'historique. L'LLM les verra, mais elles ne seront pas sauvegardées dans la base de données permanente.
4.  **Les Messages Système** :
    *   Certains messages ont `source == "system"` (ex: re-trigger après la fin d'un worker). **Désactive le pre-hook** pour ces messages, car le contexte utilisateur n'a pas changé.

---

## 4. Le Projet "Memory V2" (Ce que tu dois coder)

L'objectif est d'implémenter un système inspiré de MemOS : un **Pre-hook systématique** avec **Déduplication**.

### Les 5 Phases de l'implémentation (détaillées dans `03-PLAN_IMPLEMENTATION.md`) :
1.  **Phase 1 : État d'Injection en RAM** : Créer `ChannelInjectionState` dans `src/agent/channel.rs` pour traquer les `injected_ids` (VecDeque) et le `semantic_buffer` (les derniers messages).
2.  **Phase 2 : Le Pre-hook de Recherche** : Modifier `handle_message` et `handle_message_batch` pour appeler `MemorySearch::search` avant `run_agent_turn`.
3.  **Phase 3 : Le Moteur de Déduplication** : Filtrer les résultats de recherche en utilisant les `injected_ids` (exact match) et le `semantic_buffer` (similarité cosinus > 0.85). *Note : tu devras calculer l'embedding du semantic_buffer à la volée via `self.deps.memory_search.embedding_model().embed_one()`.*
4.  **Phase 4 : L'Injection Silencieuse** : Modifier `run_agent_turn` pour accepter un `Vec<Message>` de mémoires formatées et les insérer dans l'historique cloné.
5.  **Phase 5 : Tests** : Mettre à jour ou créer des tests d'intégration.

---

## 5. Documentation de Référence (Si tu es perdu)

Si tu as besoin de plus de contexte, lis ces fichiers dans l'ordre :

1.  **`AGENTS.md`** : La bible de l'architecture de Spacebot. Explique la différence entre Channel, Branch, Worker, Compactor et Cortex.
2.  **`RUST_STYLE_GUIDE.md`** : Les conventions de code du projet (très important pour passer la CI).
3.  **`docs/content/docs/(core)/memory.mdx` & `channels.mdx`** : La documentation officielle sur le fonctionnement actuel de la mémoire et des channels. Très utile pour comprendre les concepts de base.
4.  **`docs/design-docs/`** : Ce dossier contient les documents de conception historiques (ex: `user-scoped-memories.md`, `branch-and-spawn.md`) qui expliquent *pourquoi* certaines décisions architecturales ont été prises.
5.  **`memory-v2-plan/02-ARCHITECTURE_TECHNIQUE.md`** : Les schémas et structures de données exactes prévues pour ce projet.
6.  **`memory-v2-plan/05-REVUE_PERFECTIONNISTE.md`** : Les "Gotchas" et pièges architecturaux découverts lors de l'analyse du code.

---

## 6. Règles de Style et Conventions (RUST_STYLE_GUIDE.md)

Le projet suit des règles très strictes définies dans `RUST_STYLE_GUIDE.md`. Voici les plus importantes à respecter lors de ton implémentation :

*   **Pas de `mod.rs`** : Utilise `src/memory.rs` comme racine de module, pas `src/memory/mod.rs`.
*   **Gestion des Erreurs** : Ne jamais utiliser `unwrap()` ou ignorer silencieusement une erreur (`let _ =`). Utilise `?`, `.context()` (de `anyhow`), et loggue les erreurs non-critiques avec `tracing::warn!(%error, ...)`.
*   **Imports** : Groupés en 3 tiers séparés par une ligne vide : 1. Imports locaux (`crate::`), 2. Crates externes, 3. Librairie standard (`std::`).
*   **Async & Tokio** : Utilise `tokio::spawn` pour le travail concurrent indépendant. Clone les variables avant de les déplacer dans les blocs async.
*   **Traits Async** : N'utilise **pas** `#[async_trait]`. Utilise le RPITIT natif de Rust 2024 (retourne `impl Trait` ou utilise directement `async fn` dans les traits).
*   **Commentaires** : Explique le **pourquoi**, jamais le **quoi**. Pas de commentaires de séparation de section.
*   **Lints stricts** : `dbg!`, `todo!` et `unimplemented!` sont **interdits** et feront échouer la CI. Utilise `tracing::debug!` et des commentaires `// TODO:`.
*   **Pattern Matching** : Préfère le matching exhaustif. Utilise `let-else` pour les retours anticipés (`let Some(x) = y else { return ... };`).

**Bon courage ! Tu as toutes les cartes en main pour commencer la Phase 1.**
