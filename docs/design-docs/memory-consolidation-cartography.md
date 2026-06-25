# Memory consolidation — cartography (état réel vs design prévu vs SOTA)

> **But.** Cartographier précisément la consolidation mémoire dans spacebot : ce qui est **implémenté** (et les morceaux à moitié construits), ce qui était **prévu** dans les docs de design, et si le design prévu est **SOTA** ou améliorable. Synthèse de 3 enquêtes (code / docs / web 2026), vérifiée contre le code.
>
> Date : 2026-06-23. Contexte : émergé de la discussion sur B4 (dédup mémoire) — l'idée de l'utilisateur de faire la consolidation **dans le cortex** (et non à l'écriture) s'est révélée être **exactement le design déjà prévu** (Phase 4), à moitié câblé.

## TL;DR

1. **C'était profondément prévu.** Un doc de design dédié — [`cortex-implementation.md`](./cortex-implementation.md) **§Phase 4** — spécifie un agent LLM de consolidation autonome avec deux outils (`memory_consolidate`, `system_monitor`), un contrat d'opérations, un intervalle (6 h ou déclenché par volume d'events), la création de mémoires `Observation`, et un journal d'audit ([`cortex-history.md`](./cortex-history.md)). Le doc **reconnaît lui-même** que ces outils « *Referenced in prompts but don't exist* » (l.26-28).
2. **Beaucoup de briques existent déjà** (backend ops, types de relations, primitives de clustering, la boucle cortex, un stub de tool server en dead-code). Le **travail manquant minimal est bien borné**.
3. **L'architecture prévue est sur la ligne SOTA 2026** (consolidation **asynchrone / sleep-time** = le consensus, validé par Anthropic « Dreaming » et Letta sleep-time) et **en avance** sur 2 points (merges réversibles/loggés/non-destructifs ; conservatisme « association plutôt que merge, identité intouchable »).
4. **3 mécaniques sont en retard sur la frontière** : (a) pas de **bi-temporalité** (la supersession reste destructive) ; (b) les contradictions sont **flaggées mais jamais résolues** (inertes) ; (c) **aucune défense anti-drift / idempotence** — le point neuf, absent des docs existants, et celui qui mordra une boucle cortex qui re-tourne.
5. **Conséquence pour les bugs récents** : la consolidation **cortex** est le bon foyer (et meilleur que B4 à l'écriture). **B5** (merge mécanique *keep-winner*, déjà corrigé) est le **plancher programmatique** de la Phase 3 et reste utile. **B4** (dédup à l'écriture) devrait être **abandonné** au profit de la Phase 4 — ou réduit à un garde *exact-dup* trivial (même hash → skip), sans seuil flou.

---

## 1. Inventaire du code — présent / partiel / manquant

Vérifié sur `feat/surrealdb-memory`. (`file:line` = preuve.)

| # | Brique | État | Preuve / note |
|---|---|---|---|
| 1 | Boucle cortex (tick) + gating maintenance | **PRÉSENT** | `cortex.rs:1990` (`run_cortex_loop`), tick `tick_interval_secs`=30 s (`config/types.rs:1168`), maintenance due si `elapsed ≥ maintenance_interval_secs` (def **3600 s**), circuit-breaker 3 échecs (`cortex.rs:2444-2490`). |
| 2 | Invocations LLM du cortex | **PARTIEL** | 5 agents *one-shot* (bulletin, knowledge_synthesis, profile, intraday, daily) via `AgentBuilder` **sans tool server** (`cortex.rs:2712/2903/3540/3184/3331`). **Aucun** pour la consolidation. |
| 3 | Tool server cortex / outils réels | **PARTIEL** | `create_cortex_tool_server` existe mais **`#[allow(dead_code)]`** et ne registre que `memory_save` (`tools.rs:1046`). Non appelé par la boucle. |
| 4 | Outil **`memory_consolidate`** | **MANQUANT** | Aucun fichier, 0 symbole dans `src/`. N'apparaît que dans `cortex.md.j2:66`. |
| 5 | Outil **`system_monitor`** | **MANQUANT** | Aucun `SystemMonitorTool`. Référencé dans le prompt seulement. |
| 6 | Primitives de clustering (candidats) | **PRÉSENT** | `find_similar(id,thr,limit)`, `vector_search(emb,limit)`, `get_neighbors(id,depth,excl)`, `get_associations*` sur `MemoryBackend` (`backend.rs`). Déjà utilisées par la maintenance. |
| 7 | Ops backend de consolidation | **PRÉSENT** | `merge`, `create_association`, `update`, `forget` (soft), `delete` (hard), `prune_below` (`backend.rs`). Importance via `update()` (pas de setter dédié). |
| 8 | Types de relations `RelationType` | **PRÉSENT** | 6 variants persistés/scorés (`types.rs:180`; multiplicateurs `search.rs:359` : Updates 1.5, Causal 1.3, RelatedTo 1.0, Contradicts 0.5, PartOf 0.8). Mais **seuls `RelatedTo`/`Updates` sont créés par du code de fond** ; `Contradicts/CausedBy/ResultOf/PartOf` ne sont posés **qu'à la main** via `memory_save`. |
| 9 | Maintenance mécanique (plancher actuel) | **PRÉSENT** | `run_maintenance` (`maintenance.rs:44`) : decay (0.05/j), prune (<0.1, >30 j), **merge par similarité 0.95** (`find_similar`→`merge`, *keep-winner* depuis B5), **sans LLM**. Caps : 2000 candidats / 500 merges par passe. |
| 10 | Buffer de signaux cross-canal | **PRÉSENT mais NON CONSOMMÉ** | `Signal::MemorySaved` bufferisé (`cortex.rs:893/1503`, capacité 100). Sur event → `bump_knowledge_synthesis_version()`. **Rien ne lit le buffer pour identifier des candidats de consolidation.** |
| 11 | Création de mémoires `Observation` par le cortex | **MANQUANT** | Le cortex *lit* des Observations (bulletin/synthèse) mais n'en **crée jamais** ; seul `memory_save` (branches) en crée. |
| 12 | `cortex.md.j2` (prompt opérationnel) | **PRÉSENT mais inerte** | Rendu et stocké dans `cortex.system_prompt` (`cortex.rs:1137/1761`) — mais **aucun agent n'est construit dessus** ; champ stocké, jamais utilisé pour un tour à outils. |

**Travail manquant minimal pour câbler la Phase 4 :** (a) l'outil `memory_consolidate` (struct + schéma + impl appelant `merge`/`create_association`/`update`) ; (b) `system_monitor` (optionnel au départ) ; (c) un **récupérateur de clusters** (scanner le buffer (10) ou requêter le backend → `find_similar` pour grouper) ; (d) un **tour LLM à outils** dans la boucle (ou tâche dédiée) construit sur `cortex.md.j2` + tool server, sur un intervalle séparé ; (e) registrer les outils (réveiller le stub dead-code) ; (f) création d'`Observation`. Le threshold/backoff/circuit-breaker sont déjà là et réutilisables.

---

## 2. Ce qui était prévu (docs de design)

### Source opérationnelle — `prompts/en/cortex.md.j2`
Politique de décision (verbatim, l.33-38, 88-89) :
- mémoires qui se recouvrent (multi-canal) → **merge** (garder le contenu le plus riche, unir les associations) ;
- une mémoire plus récente en met une à jour → arête **`Updates`** + **baisser l'importance** de l'ancienne ;
- contradiction → arête **`Contradicts`**, **ne supprimer ni l'une ni l'autre**, flaguer pour la prochaine branche ;
- défaut/incertain → **« prefer associations over merges »**, *« merge only when similarity is very high »* ;
- **« Identity memories are untouchable »** ;
- *« Save LLM reasoning for consolidation and pattern detection »* (la plupart des ticks restent programmatiques).

### Source d'implémentation — `cortex-implementation.md` §Phase 4 (l.153-232)
- **État reconnu** (l.26-28) : `memory_consolidate` et `system_monitor` *« referenced in prompts but don't exist »*.
- **Contrat `memory_consolidate`** (l.165-171) : opérations `merge` / `associate` (typée) / `lower_importance` / `flag_contradiction`.
- **Agent** (l.173-177) : prompt `cortex.md.j2` (éventuellement trimé), tool server `memory_consolidate` + `system_monitor` + `memory_save` (pour les observations), **intervalle séparé du tick** (def **6 h**, ou déclenché par volume d'events).
- **Cross-canal** (l.183-186) : events `MemorySaved` de canaux différents au contenu proche → mis en file pour le prochain run ; l'agent décide merge / associate / laisser.
- **Observations** (l.188-193) : le cortex est le seul à créer des `Observation` (détection de patterns depuis le buffer).
- **Ordre** (l.208-216) : Phase 4 dépend de Phase 1, **après** que la maintenance (Phase 3) tourne (graphe propre avant d'ajouter la complexité).
- **Question ouverte** (l.225-226) : déclencheur **intervalle fixe** vs **event-driven** (>N nouvelles mémoires). Commencer fixe, ajouter event-driven ensuite.

### Journal d'audit prévu — `cortex-history.md` (l.14-18)
Events à émettre quand la Phase 4 atterrit : `memory_merged` (survivor_id, merged_id, reason), `association_created` (source, target, relation_type, reason), `contradiction_flagged` (a, b, description), `observation_created` (memory_id, preview).

### Roadmap intelligence — `surrealdb-memory/gap-analysis-intelligence.md`
- **Axe 2 (consolidation/dédup/conflits) = ●●○** : merge post-hoc 0.95 ; *aucune* classification ADD/UPDATE/DELETE à l'écriture ; `Contradicts` **jamais inféré**.
- **I2** (write-time Mem0 ADD/UPDATE/DELETE/NOOP) : valeur **élevée**, effort moyen, dépend du top-k (déjà là).
- **I8 / Axe 6** : *« Le cortex de spacebot EST déjà un agent sleep-time »* → I2/I4/I5 devraient tourner **dans le cortex en arrière-plan**, pas dans le tour de conversation.

### Variante agents dormants — `agentic-backend-readiness.md` (l.143-147)
Pour un cortex `dormant`, la consolidation passe à un **« memory janitor » instance-wide** (cron quotidien off-peak, accès direct au store, sans réveiller l'agent).

### Écarts prompt ↔ réalité (à retenir)
1. `memory_consolidate`/`system_monitor` : promis comme « mécanisme principal » du cortex, **inexistants**.
2. `Contradicts` : scoré en retrieval mais **jamais inféré** automatiquement.
3. Phase 3 merge **concaténait** (anti-pattern) — **corrigé** par B5 (*keep-winner*).
4. `lower_importance` hors-merge : pas de chemin programmatique (n'existe que comme op planifiée de l'outil).
5. Confusion de séquençage : **I2 (write-time)** et **Phase 4/I8 (sleep-time)** sont deux choses ; le gap analysis recommande I2 d'abord, Phase 4 doc suppose directement l'agent LLM complet.

---

## 3. Évaluation SOTA (frontière 2026)

> Recherche web ciblée *consolidation* (au-delà des docs big-players existants). Chiffres vendeurs = directionnels (cf. §8 du gap analysis, ~20 pts d'écart de repro).

### Verdict : **en avance sur la plupart, sur la ligne de la frontière sur le placement, en retard sur 3 mécaniques.**

**Le placement asynchrone/sleep-time est le bon — et c'est le consensus 2026, pas un manque :**
- **Anthropic « Dreaming »** (preview, 6 mai 2026) : process asynchrone inter-sessions inspiré de la consolidation hippocampique — fusionne doublons, remplace les entrées périmées/contredites, fait émerger des insights, et **« the original memory store is never touched »** (sortie dans un store séparé, révisable/jetable). ⇒ valide **exactement** la philosophie spacebot (réversible/loggé, identité intouchable). [felloai](https://felloai.com/what-is-claude-dreaming/)
- **Letta sleep-time compute** : agent de fond pendant l'idle qui consolide/réécrit les blocs mémoire — conceptuellement identique au cortex. [letta.com](https://www.letta.com/blog/sleep-time-compute)
- **Mem0** (writes async), **Zep/Graphiti** (graphe en fond, « réponses correctes des heures plus tard »), **Engram** (System-2 de fond) : tous async.

**« Write-time vs background » est un faux binaire — le vrai consensus est hybride + cheap-path-first :** un *gate* d'extraction/dédup pas cher à l'écriture + consolidation riche en fond ; et **dans** la consolidation, résolution **déterministe d'abord** (slot/embedding/subsumption/temporel), LLM **seulement** sur les clusters ambigus (Engram, Zep). spacebot route *tout* cluster vers le LLM → correct pour la qualité, mais plus cher et plus exposé au drift.

**En avance / SOTA déjà :** placement sleep-time (cortex) ; merges réversibles+loggés non-destructifs (≈ Dreaming, devant l'overwrite de Mem0) ; conservatisme « association > merge, identité intouchable » (posture *plus* sûre, vindiquée par l'analyse de drift) ; soft-delete + flag-don't-delete (consensus unanime) ; arêtes typées + supersede.

**En retard sur la frontière (3 mécaniques) :**
- **(a) Pas de bi-temporalité** (`valid_from`/`valid_to` + invalidation) → la supersession *dégrade* l'ancienne mémoire au lieu de **borner son intervalle de validité** ; impossible de répondre « que savais-je de X en mars ? ». Zep/Engram rendent la supersession **non-destructive** en fermant l'intervalle et en gardant le fait requêtable. [Zep arxiv 2501.13956](https://arxiv.org/abs/2501.13956), [Engram arxiv 2606.09900](https://arxiv.org/html/2606.09900) — *(= notre gap I3)*
- **(b) Contradictions flaggées mais jamais résolues** → un `Contradicts` jamais arbitré **dégrade le recall** (deux faits conflictuels ressortent sans signal). Défaut 2026 = **recency-wins + invalidation explicite** + chaîne *supersedes* ; ne garder `Contradicts` que pour les vraies égalités. [Hindsight](https://hindsight.vectorize.io/blog/2026/02/09/resolving-memory-conflicts)
- **(c) Aucune défense anti-drift / idempotence (LE point neuf, absent de nos docs)** : **SSGM (arxiv 2603.11768)** montre formellement que la **consolidation LLM répétée accumule un drift sémantique O(T·ε)** (« aime un peu épicé » → « adore très épicé »). Une boucle cortex qui **re-tourne** sur des clusters et **réécrit** le contenu y est directement exposée. Fix = **consolidation ancrée** : réconcilier les merges contre un **journal épisodique append-only** (spacebot a déjà un working-memory event log), garantir l'**idempotence** (re-consolider un cluster déjà consolidé = NOOP), et un **gate de validation d'écriture** (rejeter tout merge qui contredit un fait core/identité). [SSGM arxiv 2603.11768](https://arxiv.org/html/2603.11768v1)

### Leviers d'amélioration, classés
**Tier 1 (bouge l'aiguille) :**
1. **Bi-temporalité + fermeture d'intervalle** sur update/contradiction (= I3). Le vrai différenciateur ; SurrealDB le rend peu coûteux.
2. **Résolution** de contradiction (recency/source/confidence → invalider + chaîne *supersedes*), `Contradicts` réservé aux vraies égalités.
3. **Ancrage anti-drift + idempotence** : réconcilier contre le ledger épisodique immuable, convergence garantie, gate de validation vs identité. **← l'insight neuf, silencieux dans le design actuel.**

**Tier 2 :**
4. **Cheap-then-escalate** : résoudre les cas faciles sans LLM, escalader seulement l'ambigu (coût + drift réduits).
5. **Clustering graph-aware** (pas du cosinus pur) : seed cosinus puis expansion/contrainte le long des arêtes (évite les collisions sémantiques → faux merges). Communautés à la Graphiti.
6. **Harnais d'éval consolidation-spécifique** : taux de faux-merge, exactitude de résolution de contradiction, **drift** (divergence d'embedding vs ledger). Proxies : sous-scores BEAM contradiction/update, LongMemEval knowledge-update/temporal.

**Tier 3 (nice-to-have / déjà couvert / YAGNI) :** insights écrits comme mémoires de 1ʳᵉ classe (≈ Dreaming/Letta ; modeste) ; réversibilité/log (**déjà SOTA**, ne rien changer) ; politique de consolidation apprise/adaptative (YAGNI court terme).

---

## 4. Recommandation / séquençage

1. **Construire la Phase 4 telle que prévue** (`memory_consolidate` + récupération de clusters + tour LLM cortex sur intervalle séparé + observations + events d'audit). Les briques sont là ; le travail est borné (§1).
2. **Mais y intégrer dès le départ les 3 mécaniques de frontière** plutôt que de les rajouter après : (a) bi-temporalité (I3), (b) **résolution** et non simple flag des contradictions, (c) **ancrage anti-drift + idempotence** (réconcilier contre le ledger, NOOP sur re-run, gate identité). Le coût marginal de les concevoir dès l'origine ≪ les rétrofitter sur une boucle déjà en prod.
3. **Adopter cheap-then-escalate** : garder la **maintenance mécanique (Phase 3, B5 keep-winner)** comme plancher déterministe pour les doublons évidents, et **réserver le LLM aux clusters ambigus** — c'est cohérent avec « be cheap » du prompt et réduit le drift.
4. **Abandonner B4 (dédup à l'écriture)** ou le réduire à un garde *exact-dup* (même hash → skip) ; la dédup floue revient à la consolidation cortex. ⇒ supprime le risque de perte silencieuse au write-path.
5. **Trigger** : commencer **intervalle fixe** (def 6 h), ajouter **event-driven** (buffer de signaux, brique 10 déjà présente) ensuite.

> Prochain pas suggéré : transformer cette cartographie en **plan d'implémentation** (skill writing-plans), à faire **reviewer par un agent Opus** — en y inscrivant les 3 mécaniques de frontière comme contraintes de design, pas comme options.
