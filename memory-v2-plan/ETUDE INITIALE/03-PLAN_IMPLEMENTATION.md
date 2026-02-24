# Plan d'Implémentation : SpaceBot Memory V2

Ce document détaille les phases de développement pour intégrer le pre-hook systématique et la déduplication dans SpaceBot.

## Phase 0 : Étude Détaillée de MemOS (Spike & Analyse)
**Objectif :** Comprendre en profondeur MemOS pour valider nos choix architecturaux.
1. **Analyse de l'API MemOS :** Étudier les endpoints `/product/search` et `/product/add`.
2. **Évaluation d'Intégration :** Peser le pour et le contre d'un adaptateur MemOS Cloud vs. le moteur local.
3. **Extraction des Concepts :** Isoler les mécaniques de MemOS (confidence, relativity, invisibilité) à répliquer dans notre moteur local.
*(Voir le document `00-ETUDE_MEMOS.md` pour les détails).*

## Phase 1 : Fondations & État en Mémoire (Channel Struct)
**Objectif :** Préparer la structure `Channel` pour stocker l'état d'injection et le buffer sémantique.

1. **Modifier `src/agent/channel.rs` :**
   - Ajouter `ChannelInjectionState` (HashMap pour les IDs, Vec pour les embeddings).
   - Ajouter `current_turn` au `Channel`.
   - Créer les méthodes `update_injection_state` et `reset_injection_state`.

2. **Modifier `src/agent/compactor.rs` :**
   - S'assurer que lors d'une compaction, un événement `ProcessEvent::ResetInjectionState` est envoyé au Channel pour vider son cache.

## Phase 2 : Moteur de Recherche Pre-hook (SQL + Vectoriel)
**Objectif :** Créer la logique de récupération rapide des mémoires sans LLM.

1. **Créer `src/memory/injection.rs` (ou étendre `src/memory/search.rs`) :**
   - Implémenter `fetch_sql_prehook_memories(pool)` : Identity, Important (>0.8), Recent (<1h).
   - Implémenter `fetch_vector_prehook_memories(query_embedding, lancedb)` : Top 20 HNSW.

## Phase 3 : Moteur de Déduplication (Filtre ID + Cosinus)
**Objectif :** Filtrer les résultats des pre-hooks pour éviter la redondance.

1. **Implémenter la similarité cosinus :**
   - Ajouter une fonction utilitaire `cosine_similarity(a: &[f32], b: &[f32]) -> f32` dans `src/memory/embedding.rs` ou `src/memory/search.rs`.

2. **Créer la logique de filtrage :**
   - Dans `Channel::compute_memory_injection`, appliquer le filtre ID exact (`injected_ids`).
   - Appliquer le filtre sémantique (`semantic_buffer`) avec un seuil de `0.85`.

## Phase 4 : Intégration Pipeline (Pre-hook, Post-hook)
**Objectif :** Brancher le tout dans le flux de traitement des messages du Channel.

1. **Modifier `Channel::handle_message` (`src/agent/channel.rs`) :**
   - Avant `agent.prompt()`, appeler `compute_memory_injection`.
   - Formater les mémoires retenues en un bloc de contexte (ex: `SystemMessage`).
   - Injecter ce bloc dans l'historique temporaire ou permanent du LLM.
   - Après la réponse du LLM, appeler `update_injection_state` avec les mémoires injectées.
   - Incrémenter `current_turn`.

## Phase 5 : Tests & Optimisation
**Objectif :** Valider la latence (< 200ms) et la pertinence.

1. **Tests Unitaires :**
   - Tester la fonction `cosine_similarity`.
   - Tester la logique de déduplication (ID et Sémantique).

2. **Tests d'Intégration :**
   - Simuler un flux de messages et vérifier que les mémoires ne sont pas ré-injectées inutilement.
   - Vérifier le reset lors de la compaction.

3. **Optimisation :**
   - Profiler le temps d'exécution du pre-hook (objectif < 200ms).
