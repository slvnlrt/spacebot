# Revue Perfectionniste : Architecture Memory V2

Cette revue a été effectuée après une analyse approfondie des fichiers `src/agent/channel.rs`, `src/agent/compactor.rs` et `src/memory/search.rs`.

## 1. Validation des Concepts

### L'Injection Silencieuse (Validé 🟢)
Le plan propose d'injecter les mémoires de manière invisible pour l'utilisateur et de ne pas les sauvegarder dans l'historique permanent. 
**Analyse du code :** C'est parfaitement aligné avec l'architecture actuelle. Dans `channel.rs`, la méthode `run_agent_turn` clone l'historique (`self.state.history.read().await.clone()`) avant de l'envoyer à Rig. Il suffira de passer un paramètre `injected_messages: Vec<Message>` à `run_agent_turn` et de les ajouter au clone. Ainsi, l'LLM les verra, mais lors de la sauvegarde post-tour, ils seront ignorés. C'est extrêmement élégant.

### L'État d'Injection en RAM (Validé 🟢)
Le plan propose de stocker `ChannelInjectionState` en RAM.
**Analyse du code :** Au lieu de le mettre dans `ChannelState` (qui est un `Arc<RwLock>` partagé avec les outils), nous pouvons le mettre directement dans la struct `Channel`. Les méthodes `handle_message` et `handle_message_batch` prennent `&mut self`, ce qui permet de muter cet état sans aucun lock asynchrone. C'est plus performant et plus sûr.

## 2. Angles Morts Détectés (Les "Gotchas")

### A. Le Coalescing (Batching de messages)
**Le problème :** Spacebot possède un mécanisme de `coalesce_buffer` qui regroupe les messages rapides en un seul tour LLM via `handle_message_batch`.
**La solution :** Le pre-hook ne doit pas s'exécuter sur chaque petit message du buffer, mais sur le `combined_text` généré dans `handle_message_batch`. Cela donnera une requête de recherche beaucoup plus riche sémantiquement et évitera de spammer LanceDB.

### B. Les Messages Système (Re-triggers)
**Le problème :** Quand un Worker ou une Branch termine, le système s'envoie un message synthétique (`source == "system"`) pour réveiller le Channel.
**La solution :** Il faut **désactiver le pre-hook** pour les messages système. Le contexte n'a pas changé du point de vue de l'utilisateur, il est inutile de refaire une recherche vectorielle.

### C. Le Buffer Sémantique et les Embeddings
**Le problème :** Le plan propose de comparer la similarité cosinus entre les mémoires récupérées et le `semantic_buffer` (les derniers messages). Cependant, `MemorySearch::search` retourne des `Memory`, pas leurs embeddings.
**La solution :** Pour calculer la similarité, nous devrons appeler `self.deps.memory_search.embedding_model().embed_one(&memory.content)` à la volée pendant le pre-hook. Comme le modèle d'embedding tourne localement, c'est très rapide, mais il faut le faire de manière asynchrone et concurrente (via `tokio::try_join!` ou `FuturesUnordered`) pour ne pas ralentir le tour de parole.

### D. L'Interaction avec le Compactor
**Le problème :** Que se passe-t-il avec `injected_ids` quand le `Compactor` résume l'historique ?
**La solution :** Contrairement au plan initial qui suggérait de vider `injected_ids`, il est en fait préférable de **ne pas le vider**. Si une mémoire a été injectée, elle a influencé la conversation. Lors de la compaction, l'essence de cette mémoire sera capturée dans le résumé. Si on vide `injected_ids`, on risque de réinjecter la mémoire brute au prochain tour, ce qui ferait doublon avec le résumé. Nous utiliserons simplement une structure bornée (ex: `VecDeque` de 100 éléments) pour éviter les fuites de mémoire.

## Conclusion de la Revue

L'architecture proposée est non seulement viable, mais elle s'insère de manière presque symbiotique dans les contraintes actuelles de Spacebot. Les modifications requises sont isolées et ne risquent pas de casser les fonctionnalités existantes (Workers, Branches, Compactor).

**Verdict : Prêt pour l'implémentation de la Phase 1.**
