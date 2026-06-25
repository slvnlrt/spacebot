# Cortex memory consolidation (Phase 4) — kickoff brief (light)

> **Ce que c'est.** Un brief d'orientation pour une **session dédiée** future, pas un plan TDD. Il télécommande le prochain agent : *quel est le problème, où lire, ce qui est SOTA, et les contraintes non-négociables.* Le détail lourd (inventaire ligne-par-ligne, citations) est dans la **cartographie** — ne pas le dupliquer ici, le lire.
>
> **Détail complet → [`../../design-docs/memory-consolidation-cartography.md`](../../design-docs/memory-consolidation-cartography.md)** (à lire en premier).
>
> Statut : conception. Branche cible : à décider (probablement `feat/surrealdb-memory`, où vit l'abstraction `MemoryBackend`). Rien à coder avant d'avoir arbitré le périmètre (§4).

## 0. Problématique en un paragraphe

spacebot n'a **aucune consolidation mémoire intelligente**. Aujourd'hui : (1) écriture = insert aveugle, (2) un nettoyage **mécanique** (decay/prune/merge par similarité 0.95) tourne dans le cortex toutes les heures. Résultat : doublons quasi-identiques empilés, contradictions jamais résolues, pas de mise à jour sémantique. Or **la solution était déjà conçue** — une « Phase 4 » du cortex : un **agent LLM de fond** (sleep-time) qui reçoit des clusters de mémoires similaires et **discrimine** (merge / associe / supersède / flague contradiction / crée des observations). Les outils (`memory_consolidate`, `system_monitor`) sont **référencés dans le prompt mais jamais implémentés**. Le but de la session : **câbler la Phase 4**, en y intégrant dès la conception les 3 mécaniques qui la rendent réellement SOTA.

## 1. À lire en premier (ordre)

| Ordre | Doc / fichier | Pourquoi |
|---|---|---|
| 1 | [`design-docs/memory-consolidation-cartography.md`](../../design-docs/memory-consolidation-cartography.md) | La synthèse : inventaire code (présent/manquant), design prévu, verdict SOTA, leviers classés. **Tout part de là.** |
| 2 | [`design-docs/cortex-implementation.md`](../../design-docs/cortex-implementation.md) **§Phase 4** (l.153-232) | La spec d'origine : contrat de `memory_consolidate`, câblage de l'agent, intervalle, observations. Reconnaît l.26-28 que les outils n'existent pas. |
| 3 | `prompts/en/cortex.md.j2` (l.29-50, 66-72, 87-92) | Le prompt opérationnel = la **politique de décision** (merge/associate/Updates/Contradicts) + le conservatisme. Déjà rendu mais inerte. |
| 4 | [`design-docs/surrealdb-memory/gap-analysis-intelligence.md`](../../design-docs/surrealdb-memory/gap-analysis-intelligence.md) (Axe 2, Axe 6, I2/I3/I8) | Pourquoi c'est « haute valeur » ; distinction I2 (write-time) vs I8 (sleep-time) ; I3 (bi-temporalité). |
| 5 | [`design-docs/cortex-history.md`](../../design-docs/cortex-history.md) (l.14-25) | Les events d'audit à émettre (`memory_merged`, `association_created`, `contradiction_flagged`, `observation_created`). |
| 6 | [`design-docs/memory-ingestion-bug-report-2026-06-23.md`](../../design-docs/memory-ingestion-bug-report-2026-06-23.md) | Les leçons B2 (LLM ≠ signal de contrôle), B4 (dédup write-time abandonnée), B5 (ne pas concaténer). |

**Code à lire (preuves dans la cartographie §1) :** `src/agent/cortex.rs` (boucle, invocations LLM one-shot, buffer `MemorySaved`, `create_cortex_tool_server` en dead-code l.1046), `src/memory/maintenance.rs` (le plancher mécanique), `src/memory/backend.rs` (ops dispo : `merge`/`create_association`/`update`/`forget`/`find_similar`/`vector_search`/`get_neighbors`), `src/memory/types.rs:180` (`RelationType`).

## 2. État en un coup d'œil

**Présent (réutilisable) :** boucle cortex + gating maintenance + circuit-breaker ; toutes les ops backend de consolidation ; les 6 `RelationType` (persistés/scorés) ; primitives de clustering ; buffer cross-canal `MemorySaved` (non consommé) ; stub `create_cortex_tool_server` (dead-code) ; infra LLM (AgentBuilder + routing `ProcessType::Cortex`).

**Manquant (le travail) :** l'outil `memory_consolidate` ; `system_monitor` (optionnel au départ) ; le récupérateur de clusters (scanner le buffer / requêter → `find_similar`) ; le **tour LLM à outils** monté sur `cortex.md.j2` sur un intervalle séparé ; la création d'`Observation`. → détail et `file:line` : **cartographie §1**.

## 3. SOTA & références (ce qu'il faut emprunter)

Le placement **asynchrone/sleep-time du cortex est déjà la bonne archi** (consensus 2026). Références utiles, avec *quoi en prendre* :

| Source | URL | À en retenir |
|---|---|---|
| Anthropic « Dreaming » (mai 2026) | felloai.com/what-is-claude-dreaming | Consolidation inter-sessions ; **store original jamais touché** (sortie séparée, révisable) → valide réversibilité/non-destructif. |
| Letta sleep-time compute | letta.com/blog/sleep-time-compute · docs.letta.com/.../sleeptime | Agent de fond pendant l'idle = le cortex. Best-practices de cadence. |
| Zep / Graphiti | arxiv.org/abs/2501.13956 · neo4j.com/blog/developer/graphiti-knowledge-graph-memory | **Bi-temporalité** (valid-time vs transaction-time) ; contradiction = **invalider l'arête** (jamais delete) ; communautés (clustering graph-aware). |
| Engram (dual-process) | arxiv.org/html/2606.09900 | **Cheap-then-escalate** : résolution déterministe d'abord, LLM seulement sur l'ambigu. Bi-temporal graph. |
| SSGM (drift) | arxiv.org/html/2603.11768v1 | **Le point neuf** : la consolidation LLM répétée **drifte** (O(T·ε)). Fix = ancrage sur un **ledger épisodique immuable** + idempotence + gate de validation. |
| Hindsight / Vectorize | hindsight.vectorize.io/blog/2026/02/09/resolving-memory-conflicts | **Résolution** de contradiction = recency-wins + invalidation explicite ; eviction = outil de conformité, pas de perf. |
| Mem0 | mem0.ai/blog/state-of-ai-agent-memory-2026 | LLM `ADD/UPDATE/DELETE/NOOP` ; writes async. (Chiffres vendeurs = directionnels.) |

Verdict (cartographie §3) : **en avance/aligné sur le placement**, **en retard sur 3 mécaniques** (ci-dessous).

## 4. Contraintes de design — NON-NÉGOCIABLES

À inscrire dans le plan détaillé comme **contraintes**, pas comme options :

**Invariants (déjà dans le prompt — les honorer) :**
- Conservatisme : *« prefer associations over merges ; merge only when similarity is very high »*.
- **Identité intouchable** (jamais merge/decay/prune sans instruction explicite).
- Merges **réversibles + loggés** (soft-delete/`forget`, jamais hard-delete sauf conformité).
- **Cheap** : la maintenance mécanique (B5 keep-winner) reste le plancher ; le LLM seulement quand ça vaut le coup.

**Les 3 mécaniques de frontière (à concevoir dès l'origine — coûte beaucoup moins que de rétrofitter) :**
1. **Bi-temporalité** (`valid_from`/`valid_to` + invalidation) — la supersession **ferme l'intervalle** de l'ancien fait au lieu de le dégrader ; permet les requêtes « as-of ». (= gap I3 ; SurrealDB le rend peu coûteux.)
2. **Résolution** de contradiction (recency/source/confidence → invalider + chaîne *supersedes*), `Contradicts` réservé aux vraies égalités. Ne **pas** se contenter de flaguer.
3. **Ancrage anti-drift + idempotence** : réconcilier les merges contre le **journal épisodique immuable** (working-memory event log existant), garantir qu'une re-consolidation d'un cluster déjà consolidé = **NOOP**, gate de validation (rejeter tout merge qui contredit un fait core/identité).

**Pattern à appliquer :** **cheap-then-escalate** (déterministe d'abord, LLM sur l'ambigu) + clustering **graph-aware** (seed cosinus puis expansion le long des arêtes, pas cosinus pur).

## 5. Décision de périmètre à arbitrer EN PREMIER (avec l'utilisateur)

- **Option A — Phase 4 minimale** : câbler `memory_consolidate` + tour LLM + observations, en gardant la sémantique actuelle (merge/associate/flag). Rapide, mais **rate les 3 mécaniques de frontière** et reste exposé au drift.
- **Option B — Phase 4 « frontière »** (recommandée) : Phase 4 **avec** bi-temporalité + résolution de contradiction + ancrage anti-drift dès le départ. Plus de design amont, mais c'est ce qui la rend réellement SOTA et évite un rétrofit douloureux.

→ **À trancher avec l'utilisateur avant tout code.** (Recommandation : B, en livrant par incréments mais avec le schéma bi-temporal + le ledger d'ancrage posés dès le 1ᵉʳ incrément.)

## 6. Ordre d'attaque suggéré (haut niveau, pas TDD)

1. Arbitrer le périmètre (§5).
2. Concevoir le **contrat de l'outil `memory_consolidate`** (ops : merge / associate / supersede-with-interval-close / resolve-contradiction / lower_importance) **avec les champs bi-temporels** et la sémantique d'ancrage.
3. Concevoir la **récupération de clusters** (buffer `MemorySaved` → candidats ; cosinus + expansion graphe ; cheap-resolve avant LLM).
4. Concevoir le **tour LLM cortex** (intervalle séparé def 6 h ; prompt `cortex.md.j2` ; idempotence/convergence).
5. **Observations** + **events d'audit** (`cortex-history.md`).
6. **Harnais d'éval** consolidation (faux-merge, résolution de contradiction, **drift** vs ledger) — sans lui, on ne saura pas si ça aide (cf. §8 du gap analysis : chiffres vendeurs peu fiables).
7. Décommissionner B4 (dédup write-time) ; garder un garde *exact-dup* trivial + le plancher mécanique B5.

## 7. Pièges / à NE PAS faire

- **Ne pas** confier au LLM une décision de **contrôle** déterministe (leçon B2). Le LLM produit le *jugement* (quoi merger) ; le code possède le *contrôle* (quand, idempotence, garde-fous).
- **Ne pas** refaire la dédup **à l'écriture** (B4) — risque de perte silencieuse ; la consolidation cortex est le bon foyer.
- **Ne pas** concaténer les contenus au merge (anti-pattern B5, déjà corrigé en keep-winner).
- **Ne pas** hard-delete (sauf conformité) ; soft-delete + invalidation.
- **Ne pas** ignorer le **drift** : une boucle qui re-réécrit ses propres sorties dérive. Ancrer sur le ledger immuable, garantir l'idempotence.
- **Ne pas** envoyer *tous* les clusters au LLM (coût + drift) — cheap-then-escalate.

## 8. Definition of done (esquisse)

- `memory_consolidate` câblé + registré + appelé par un tour cortex sur intervalle séparé ; observations créées ; events d'audit émis.
- Bi-temporalité + résolution de contradiction + idempotence/ancrage en place (si Option B).
- Harnais d'éval qui mesure faux-merge / résolution / drift sur des données de conversation réelles.
- Gates : feature-off `just gate-pr` vert ; feature-on `clippy --features surreal-memory` propre.
