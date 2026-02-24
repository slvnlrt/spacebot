# Phase 0 : Étude Détaillée de MemOS et Faisabilité d'Intégration

Avant de coder notre propre solution de pre-hook, il est crucial d'analyser en profondeur le fonctionnement de MemOS (openmem.net) pour comprendre ses mécanismes internes, ce que nous pouvons lui emprunter, et évaluer la pertinence d'une intégration directe (MemOS Cloud) dans SpaceBot.

## 1. Analyse du Fonctionnement Interne de MemOS

MemOS repose sur une architecture hybride (Vecteur + Graphe) gérée par un LLM interne (le MemReader).

### A. Le Pipeline d'Ingestion (Post-hook)
- **Capture :** À la fin de chaque interaction, l'échange complet est envoyé à l'API `/product/add`.
- **MemReader (LLM Interne) :** Un LLM dédié analyse la conversation pour extraire les "faits mémorables".
- **Stockage Dual :**
  - **Qdrant (Vecteur) :** Stocke les embeddings pour la recherche sémantique rapide.
  - **Neo4j (Graphe) :** Stocke les relations entre les entités extraites pour comprendre le contexte global.
- **Gestion des Conflits :** MemOS gère automatiquement la déduplication et la fusion des contradictions (ex: si l'utilisateur change de préférence).

### B. Le Pipeline de Restitution (Pre-hook)
- **Interception :** Avant que le LLM principal ne réponde, le message utilisateur est envoyé à `/product/search`.
- **Recherche Hybride :** MemOS interroge Qdrant et Neo4j pour ramener un contexte riche.
- **Métadonnées :** Les résultats incluent `memory_key`, `memory_value`, `create_time`, `confidence`, et `relativity`.
- **Injection :** Ces résultats sont formatés et injectés de manière invisible dans le prompt système du LLM principal.

## 2. Évaluation : Brancher MemOS Cloud à SpaceBot ?

Faut-il remplacer le système de mémoire local de SpaceBot par des appels à l'API MemOS Cloud ?

### Avantages d'une intégration MemOS Cloud :
1. **Zéro maintenance de base de données :** Plus besoin de gérer LanceDB ou SQLite pour la mémoire.
2. **Graphe managé :** Neo4j est géré par MemOS, offrant des relations complexes "out-of-the-box".
3. **Curation automatique :** Le MemReader de MemOS gère l'extraction et la fusion des contradictions sans que nous ayons à coder cette logique.
4. **Performances impressionnantes :** Malgré les appels réseau, l'API MemOS est extrêmement optimisée. Les tests sur OpenClaw montrent que la requête cloud + recherche + retour se fait en une fraction de seconde, rendant l'injection imperceptible pour l'utilisateur.

### Le Défi : Combiner MemOS Cloud et les forces de SpaceBot
Si l'on intègre MemOS Cloud, le défi principal est de ne pas perdre ce qui fait la force de SpaceBot :
1. **Le Typage Fort :** SpaceBot possède 8 types de mémoires (Identity, Preference, Decision, etc.). MemOS stocke du texte générique.
   *Solution possible :* Utiliser les métadonnées de MemOS (si l'API le permet) pour stocker le type SpaceBot, ou pré-fixer les `memory_value` (ex: `[DECISION] On utilise JWT`).
2. **Le Graphe Causal Typé :** SpaceBot a des arêtes spécifiques (`Updates`, `Contradicts`, `CausedBy`). MemOS a son propre graphe Neo4j, mais ses relations sont gérées en boîte noire par leur LLM.
   *Solution possible :* Accepter de déléguer la gestion du graphe à MemOS, en perdant le contrôle fin sur les types d'arêtes, mais en gagnant la curation automatique.
3. **Philosophie "Single Binary" :** SpaceBot est conçu pour tourner sans dépendances externes.
   *Solution possible :* Ne pas imposer MemOS, mais l'implémenter via un pattern `MemoryProvider` (trait Rust). L'utilisateur pourrait choisir dans sa config : `provider = "local"` (SQLite+LanceDB) ou `provider = "memos_cloud"`.

**Conclusion sur l'intégration Cloud :** Les performances de MemOS Cloud sont suffisamment bluffantes pour justifier une intégration. Cependant, pour la V1 de cette refonte, la priorité est d'améliorer le moteur local de SpaceBot en lui empruntant les concepts de pre-hook et de déduplication. L'abstraction `MemoryProvider` et l'intégration de MemOS Cloud seront repoussées à une V2, afin de se concentrer sur la livraison d'un système local robuste et performant.

## 3. Ce que SpaceBot DOIT emprunter à MemOS (Implémentation Locale)

Puisque nous gardons notre moteur local (SQLite + LanceDB), voici les concepts brillants de MemOS que nous allons répliquer :

1. **L'Injection Systématique (Pre-hook) :**
   - Ne plus laisser le LLM "décider" d'appeler l'outil `memory_recall`.
   - Faire une recherche vectorielle (LanceDB) ultra-rapide en tâche de fond avant chaque prompt.

2. **Le Scoring de Confiance et de Relativité :**
   - MemOS retourne un score de `confidence` et de `relativity`.
   - *Adaptation SpaceBot :* Utiliser notre score d'`importance` (SQLite) combiné au score de similarité cosinus (LanceDB) pour filtrer ce qui est injecté.

3. **L'Invisibilité pour le LLM :**
   - Le LLM ne doit pas voir les rouages de la recherche. Il reçoit simplement un bloc de contexte propre : `[System: Relevant past context: ...]`.

4. **La Déduplication Intelligente :**
   - MemOS évite de répéter les mêmes choses.
   - *Adaptation SpaceBot :* Le système de `last_injected_turn` et le buffer sémantique (détaillés dans l'architecture technique).

## 4. Prochaines Étapes de cette Phase d'Étude

1. **Spike Technique (Optionnel) :** Créer un script Python ou Rust rapide pour tester l'API MemOS Cloud (`/product/add` et `/product/search`) afin d'observer exactement le format de retour et la latence réelle.
2. **Validation de l'Architecture :** Confirmer que notre implémentation locale (LanceDB + SQLite pre-hook) peut atteindre des performances similaires ou supérieures à l'API MemOS.
3. **Décision Finale :** Acter si nous développons uniquement le pre-hook local, ou si nous créons également une interface `MemoryProvider` pour permettre de brancher MemOS Cloud via la configuration.