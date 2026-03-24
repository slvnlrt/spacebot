# Rapport — État des changements Git

**Branche:** `merge/upstream-main-2026-02-24` (à jour avec origin)  
**Date:** 2026-02-26

---

## 1. Vue d'ensemble

### Fichiers modifiés (5 fichiers, +128/-33 lignes)

| Fichier | Nature du changement |
|---------|---------------------|
| `src/agent/channel.rs` | Refactor majeur : filtrage cosinus relatif two-pass dans `compute_memory_injection` |
| `src/config.rs` | Default `contextual_min_score` : 0.01 → **0.70** (ratio cosinus) |
| `src/api/settings.rs` | Fallback API : 0.01 → **0.70** |
| `interface/src/routes/Settings.tsx` | Slider 0–0.05 → 0–1, step 0.001 → 0.01, defaults 0.70/0.85 |
| `interface/src/routes/AgentConfig.tsx` | Slider 0–0.05 → 0–1, step 0.001 → 0.01 |

### Fichiers non trackés (4 fichiers)

| Fichier | Statut |
|---------|--------|
| `memory-v2-plan/HANDOFF.md` | Doc de debug/handoff — à garder comme référence |
| `memory-v2-plan/IMPLEMENTATION 2/memory-injection-viz-plan.md` | Plan pour la visualisation timeline — pas encore implémenté |
| `migrations/20260225000001_memory_injection_events.sql` | **Reliquat** — table pour la viz UI, code rollbacké mais DB déjà migrée |
| `migrations/20260225000002_memory_injection_historical.sql` | **Reliquat** — colonne `historical_json`, même situation |

---

## 2. Analyse du Bug 1 — Injection Persistence

### Ce que dit le HANDOFF.md

Le HANDOFF affirme que le Bug 1 est "FIXED ✅" et décrit un fix qui déplace l'injection dans le guard partagé AVANT le clone. **Ce fix n'est PAS dans les changements git actuels.**

### Ce que fait le code actuel

Voici le flow exact dans `run_agent_turn` (lignes 1905–1951) :

```
1. Clone history depuis le guard partagé (read lock)
2. Capture history_len_before = history.len()
3. Prune + push injection block dans le CLONE
4. LLM agentic loop avec le clone
5. apply_history_after_turn:
   - Ok / MaxTurnsError → *guard = history  (le clone ENTIER remplace le guard)
   - PromptCancelled / Error → guard.truncate(history_len_before)
```

### Analyse : est-ce un vrai bug ?

**Cas 1 — Turn réussie (Ok) :** `*guard = history` → le clone complet (avec injection block) est écrit dans le guard. ✅ L'injection persiste.

**Cas 2 — MaxTurnsError :** Même chose, `*guard = history`. ✅ L'injection persiste.

**Cas 3 — PromptCancelled (le cas normal du reply tool) :** `guard.truncate(history_len_before)`. Le guard est tronqué à sa taille d'avant le clone. L'injection block n'a jamais été écrite dans le guard (elle était dans le clone). **Mais** `history_len_before` est capturé AVANT l'injection, donc le truncate ne touche pas le guard original — il le laisse tel quel.

**Voici le point clé :** Sur `PromptCancelled`, le guard n'est PAS modifié par le truncate (il a déjà la bonne taille). L'injection du turn courant est perdue (elle était dans le clone), mais les injections des turns PRÉCÉDENTS (qui avaient été écrites via `*guard = history` sur des turns réussis) sont préservées.

### Conclusion : le bug EXISTE — la bounded persistence est cassée

Après analyse approfondie, **le HANDOFF a raison : c'est un vrai bug.**

#### Le problème fondamental

Pour les channels, le chemin normal est **toujours** `PromptCancelled` : le reply tool fire, le hook retourne `HookAction::Terminate`, Rig lève `PromptCancelled`. C'est le cas standard, pas l'exception.

Or sur `PromptCancelled`, `apply_history_after_turn` fait `guard.truncate(history_len_before)`. Comme `history_len_before` est capturé AVANT l'injection, et que l'injection est faite dans le clone (pas dans le guard), **aucun bloc d'injection ne persiste jamais dans le guard partagé**.

#### Trace multi-turn

```
Turn 1:
  guard = [msg1, msg2]
  history = clone = [msg1, msg2]
  history_len_before = 2
  injection → history = [msg1, msg2, [Context]: block1]
  LLM reply → PromptCancelled
  guard.truncate(2) → guard = [msg1, msg2]  ← block1 PERDU

Turn 2:
  guard = [msg1, msg2]  ← pas de block1 !
  history = clone = [msg1, msg2]
  history_len_before = 2
  injection → history = [msg1, msg2, [Context]: block2]
  LLM reply → PromptCancelled
  guard.truncate(2) → guard = [msg1, msg2]  ← block2 PERDU
```

**Résultat : la feature `max_injected_blocks_in_history` est complètement non-fonctionnelle.** Le `prune_old_injection_blocks` n'a jamais rien à pruner car les blocs ne persistent jamais dans le guard.

#### Impact concret

Le design de bounded persistence (ADR dans `memory-injection-persistence-model.md`) repose sur le fait que les blocs d'injection persistent pendant N tours pour :
1. **Follow-ups implicites** : "pourquoi ça ?" → le LLM a encore le contexte mémoire du turn précédent
2. **Contexte tacite** : les 7 mémoires sur 10 que le LLM n'a pas citées mais qui informaient sa compréhension
3. **Continuité conversationnelle** : pas besoin de re-retriever les mêmes mémoires à chaque turn

Sans persistence, chaque turn est isolé. Le LLM perd le contexte mémoire entre les turns.

#### Le fix décrit dans le HANDOFF

Le HANDOFF décrit la solution correcte : déplacer l'injection dans le guard AVANT le clone :

```rust
// AVANT (bugué) :
let mut history = guard.clone();
let history_len_before = history.len();
// injection dans history (le clone)

// APRÈS (fix) :
// injection dans guard (le partagé)
let mut history = guard.clone();
let history_len_before = history.len();
```

Ainsi :
- L'injection est dans le guard ET dans le clone
- `history_len_before` est capturé APRÈS injection
- Sur `PromptCancelled`, `guard.truncate(history_len_before)` préserve l'injection car elle est en dessous de la ligne de truncation

#### Pourquoi le fix n'est pas dans les changements git

Le HANDOFF dit "Status: All 204 lib tests pass. This fix is done." mais le fix n'est pas dans le diff. Deux hypothèses :
1. Le fix a été implémenté puis rollbacké avec le reste du code problématique (le dev qui s'est mal passé)
2. Le HANDOFF a été écrit de manière anticipée (décrivant le fix prévu, pas encore appliqué)

### Recommandation

**Le fix de Bug 1 est nécessaire** pour que la bounded persistence fonctionne. Il faut implémenter le changement décrit dans le HANDOFF : injecter dans le guard partagé avant le clone, et capturer `history_len_before` après l'injection.

Cependant, **ce n'est pas bloquant pour le Bug 2 (cosine filtering)** — les deux sont indépendants. Le cosine filtering améliore la qualité de ce qui est injecté ; la persistence améliore la durée de vie de ce qui est injecté.

---

## 3. Analyse du Bug 2 — Cosine Filtering (les changements en attente)

### Ce qui est implémenté

Le refactor two-pass dans `compute_memory_injection` :

1. **Pass 1 :** Pour chaque candidat, résoudre l'embedding et calculer la similarité cosinus avec la query. Tracker `max_cosine`.
2. **Pass 2 :** `dynamic_threshold = max_cosine × contextual_min_score` (ratio). Filtrer les candidats en dessous. Puis dedup sémantique.

Changements associés :
- `SearchConfig.min_score: 0.0` (ne plus filtrer sur le score RRF)
- `SearchConfig.graph_seed_limit: 0` (désactiver le graph traversal qui flood avec les stop words)
- Import de `cosine_similarity` depuis `crate::memory`
- `InjectionSource` dérive `PartialEq` (nécessaire pour le check `== InjectionSource::Contextual`)
- Logging de debug pour le tuning

### État de la config (après correction)

| Emplacement | Valeur | Rôle |
|-------------|--------|------|
| `src/config.rs` default | **0.70** | Default Rust pour les nouveaux déploiements |
| `src/api/settings.rs` fallback | **0.70** | Fallback API quand pas de config en DB |
| `Settings.tsx` useState | **0.70** | Default UI initial |
| `Settings.tsx` useEffect | **0.85** | ⚠️ Fallback quand settings chargés mais valeur absente |

> Note : le useEffect fallback à 0.85 dans Settings.tsx (ligne 1253) est une incohérence mineure. Si les settings sont chargés depuis le backend, la valeur viendra du backend (0.70). Le 0.85 ne s'applique que si `settings.memory_injection.contextual_min_score` est `undefined`, ce qui ne devrait pas arriver.

### Statut

- ✅ Implémenté
- ❌ Pas encore testé (ni build, ni live, ni unit tests)
- ⚠️ Contient du logging INFO à downgrader avant commit

---

## 4. Migrations orphelines

Les deux migrations `20260225000001` et `20260225000002` créent la table `memory_injection_events` pour la visualisation timeline dans l'UI. Le code correspondant (décrit dans `memory-injection-viz-plan.md`) a été rollbacké, mais les migrations doivent rester car la DB a déjà été migrée.

**Impact :** Aucun. La table existe mais n'est ni lue ni écrite par le code actuel. Elle sera utilisée quand la feature de visualisation sera ré-implémentée.

---

## 5. Prochaines étapes suggérées

1. **Build + test** : `cargo build` puis `cargo test --lib` pour vérifier que le two-pass compile et que les tests existants passent
2. **Test live** : envoyer des messages de test et vérifier les logs cosinus
3. **Tuner le ratio** : commencer à 0.70, ajuster selon les résultats
4. **Cleanup logging** : downgrader les `tracing::info!` de debug en `tracing::debug!`
5. **Commit** : les 5 fichiers modifiés ensemble
6. **Plus tard** : ré-implémenter la visualisation timeline (les migrations sont prêtes)
