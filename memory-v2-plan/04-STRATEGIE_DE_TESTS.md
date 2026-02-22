# Stratégie de Tests : SpaceBot Memory V2

## 1. Tests Unitaires (Core Logic)

### A. Similarité Cosinus (`src/memory/embedding.rs`)
- **Test :** `test_cosine_similarity_identical` (doit retourner 1.0).
- **Test :** `test_cosine_similarity_orthogonal` (doit retourner 0.0).
- **Test :** `test_cosine_similarity_opposite` (doit retourner -1.0).

### B. Moteur de Déduplication (`src/agent/channel.rs` ou `src/memory/injection.rs`)
- **Test :** `test_deduplication_exact_id`
  - Injecter une mémoire ID=1.
  - Tenter de ré-injecter ID=1.
  - Vérifier qu'elle est filtrée.
- **Test :** `test_deduplication_semantic_similarity`
  - Injecter un embedding A.
  - Tenter d'injecter un embedding B (similarité > 0.85 avec A).
  - Vérifier qu'il est filtré.
  - Tenter d'injecter un embedding C (similarité < 0.85 avec A).
  - Vérifier qu'il est conservé.

## 2. Tests d'Intégration (Pipeline)

### A. Flux de Pre-hook (`tests/memory_injection.rs`)
- **Setup :** Créer un `MemoryStore` en mémoire avec des mémoires Identity, Important, et standards.
- **Test :** `test_sql_prehook_retrieval`
  - Vérifier que les mémoires Identity et Important sont toujours récupérées.
- **Test :** `test_vector_prehook_retrieval`
  - Vérifier que la recherche HNSW retourne les bons candidats pour une requête donnée.

### B. Cycle de Vie du Channel (`tests/channel_lifecycle.rs`)
- **Test :** `test_channel_injection_state_updates`
  - Envoyer un message au Channel.
  - Vérifier que l'état d'injection (`injected_ids`, `semantic_buffer`) est mis à jour.
  - Envoyer un second message similaire.
  - Vérifier que les mémoires ne sont pas ré-injectées.
- **Test :** `test_compaction_resets_injection_state`
  - Déclencher un événement de compaction.
  - Vérifier que `injected_ids` et `semantic_buffer` sont vidés.

## 3. Tests de Performance (Benchmarks)

- **Objectif :** Le pre-hook complet (SQL + Vectoriel + Déduplication) doit s'exécuter en moins de 200ms.
- **Outil :** Utiliser `criterion` ou des tests de performance simples avec `std::time::Instant`.
- **Scénario :**
  - Base de données avec 10 000 mémoires.
  - Mesurer le temps de `compute_memory_injection`.
  - S'assurer que le calcul de similarité cosinus sur le buffer sémantique (max 50-100 éléments) est négligeable (< 5ms).
