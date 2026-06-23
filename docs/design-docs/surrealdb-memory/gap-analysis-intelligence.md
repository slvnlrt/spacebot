# Gap Analysis — La couche « intelligence » de la mémoire

> Date : 2026-06-22. Branche : `feat/surrealdb-memory`.
> **Révisé 2026-06-23** après recherche web : MemOS scindé (gouvernance/permissions ≠ YAGNI), user-scoping remonté en
> priorité haute (spacebot est multi-user *par conception*), sleep-time compute (Letta), bi-temporalité confirmée
> convergente (Zep + Cognee + Mem0), et ajout des frontières émergentes 2026. Sources en §Sources.
>
> **Objet :** comparer, axe par axe, ce que la mémoire de spacebot fait *réellement aujourd'hui*
> (intelligence, pas stockage) avec les mécanismes des systèmes de mémoire d'agents à l'état de l'art
> (Mem0, Zep/Graphiti, Cognee, Letta/MemGPT, MemOS, Generative Agents, A-MEM, HippoRAG), et en déduire
> **ce qu'on emprunterait** pour atteindre un système state-of-the-art.
>
> **Scope :** la couche *intelligence*. L'infrastructure (moteur de stockage unifié SurrealDB, recherche
> hybride, graphe natif) est livrée par les Plans A–E ([`handoff.md`](./handoff.md)) et est hors-scope ici —
> sauf pour noter, à chaque axe, **en quoi l'infra qu'on vient de bâtir abaisse le coût** de l'upgrade.
>
> **Méthode :** l'état actuel est **vérifié dans le code** (`file:line` + formules verbatim), pas déduit des
> design-docs. Les design-docs qui décrivent des features *non câblées* sont marqués DESIGN-ONLY. Le volet
> SOTA est mécanisme-centré ; les chiffres de benchmark sont **vendor-reported** (à prendre avec prudence).
> Étude fondatrice associée : [`research-llm-memory-feasibility.md`](./research-llm-memory-feasibility.md).

---

## 0. Tableau de bord (scorecard)

Maturité spacebot : ●●● mûr · ●●○ partiel · ●○○ embryonnaire · ○○○ absent.

| # | Axe | spacebot | Écart vs SOTA | Priorité d'emprunt |
|---|-----|:---:|---|:---:|
| 1 | Formation / ingestion | ●●○ | Extraction auto **batch** (persistence branch) vs extract-update **inline** (Mem0) | Moyenne |
| 2 | Consolidation / dédup / conflits | ●●○ | Merge post-hoc à 0.95 ; **aucune** classification ADD/UPDATE/DELETE à l'écriture | **Haute** |
| 3 | Structure de connaissance / relations typées | ●●○ | 8 types + 6 relations, mais arêtes **100% manuelles**, pas d'entités/ontologie | Moyenne |
| 4 | Raisonnement temporel / bi-temporalité | ○○○ | **Absent** total (que created/updated_at) ; pas d'invalidation ni de requête « as-of » | **Haute** |
| 5 | Intelligence de retrieval | ●●○ | Hybride RRF+graphe OK, mais **ni importance/récence dans la fusion**, ni reranking, ni diversité | **Haute** (quick win) |
| 6 | Réflexion / synthèse | ●●● | 4 boucles LLM (cortex) **livrées** ; manque la consolidation autonome (Phase 4) | Basse |
| 7 | Oubli / décroissance / tiering | ●●○ | Décroissance+prune OK ; **tiered memory DESIGN-ONLY** (pas de colonne `tier`) | Moyenne |
| 8 | Personnalisation / scoping | ●●○ | Scopé par agent ; `channel_id` non filtré ; **user-scoping absent** (or spacebot est multi-user *par conception*) | **Haute** |

**Lecture rapide :** spacebot n'est *pas* un débutant — il a déjà l'extraction automatique, un vrai pipeline de
maintenance (décroissance/merge), un cortex qui synthétise, des relations typées et un retrieval hybride. Les
écarts les plus rentables sont **(5) le scoring de retrieval** (quasi-gratuit, fort impact), **(2) la consolidation
à l'écriture** façon Mem0, **(4) la bi-temporalité** (que SurrealDB rend bon marché), et — spacebot étant **multi-user
par conception** — **(8) le user-scoping** (sans lui, les mémoires de tous les utilisateurs d'un agent se mélangent).

---

## 1. Ce que spacebot fait déjà bien (pour ne pas le sous-estimer)

- **Extraction automatique pilotée par LLM** : `spawn_memory_persistence_branch` (`channel.rs:5`), déclenchée par
  `check_memory_persistence()` sur seuil de messages + temps (`channel.rs:1800, 2403, 3702`). Une branch *silencieuse*
  relit l'historique du canal et persiste via `memory_save`. → on a déjà le « quoi retenir » délégué à un LLM.
- **Pipeline de maintenance réel** (`maintenance.rs`) : décroissance, prune, merge par similarité — sur planning
  piloté par le cortex.
- **Cortex à 4 boucles de synthèse LLM** (`cortex.rs`) : knowledge synthesis, bulletin, synthèse intraday, résumé
  quotidien, + profil d'agent. C'est la « réflexion » des Generative Agents, déjà livrée.
- **Relations typées** (6 types) + **types de mémoire** (8, avec importance par défaut) + **retrieval hybride**
  (vecteur + FTS + RRF + traversée de graphe).

Autrement dit : les *briques* sont là. Le gap SOTA est dans la **finesse des algorithmes** qui les exploitent.

---

## 2. Analyse axe par axe

### Axe 1 — Formation / ingestion des mémoires

**Actuel (vérifié).** Deux chemins : (a) l'agent appelle `memory_save` pendant son tour ; (b) la *persistence branch*
automatique (seuil messages/temps) relit la conversation et décide quoi sauver. `memory_save` fait un
`store.save(&memory, None)` (`memory_save.rs:258`) — **insert brut**, la salience est entièrement déléguée au LLM de la
branch. Pas d'extraction *inline* par message ni de scoring de salience explicite.

**SOTA.** **Mem0** extrait en continu : à chaque échange, un prompt d'extraction produit des « candidate facts », puis
une 2ᵉ passe les compare aux mémoires similaires existantes. **Zep/Graphiti** ingère des « episodes » et en dérive des
entités/faits via LLM. **Generative Agents** loggue chaque observation avec un *score d'importance* attribué par le LLM
(1–10). **A-MEM** construit une « note » structurée (mots-clés, tags, contexte) à chaque ajout.

**Écart.** spacebot extrait **par batch** (une branch qui relit), pas **inline** ; et il n'attache pas de **score
d'importance LLM** explicite à la création (l'importance vient du *type* par défaut, `types.rs:77-89`).

**À emprunter.** (i) Faire produire par la persistence branch un **score d'importance/salience** par mémoire (prompt
Generative-Agents : « note de 1 à 10 la portée de ce souvenir »), au lieu de l'importance-par-type. (ii) Optionnel :
extraction *inline* légère pour les faits saillants, en gardant la branch pour la consolidation lourde.

**Coût / valeur.** Moyen / moyen. L'infra n'y change rien (c'est du prompt + un champ déjà présent).

---

### Axe 2 — Consolidation / dédup / résolution de conflits  ⭐ levier majeur

**Actuel (vérifié).** À l'écriture : **aucune** déduplication ni détection de contradiction (`memory_save.rs:258`).
La consolidation est **post-hoc**, dans `maintenance.rs` :
- merge des quasi-doublons par similarité vectorielle, seuil **0.95** (`merge_similarity_threshold: 0.95`, l.38) ;
  le plus important gagne, le perdant est *forgotten*, embedding supprimé, associations recâblées, contenus
  **concaténés**, arête `Updates` ajoutée.
- `Contradicts` existe comme **type de relation** mais n'est **jamais inféré** — uniquement asserté à la main.

**SOTA.** **Mem0** est l'archétype : pour chaque fait extrait, il récupère le top-k similaire et demande au LLM une
**décision ADD / UPDATE / DELETE / NOOP** — c'est *ça* qui évite la prolifération de doublons et qui **met à jour** un
fait obsolète plutôt que d'empiler. **Zep/Graphiti** va plus loin : un nouveau fait qui contredit un ancien
**invalide** l'arête précédente (la marque expirée) au lieu de la fusionner. **A-MEM** fait *évoluer* les anciennes
notes quand une nouvelle arrive (Zettelkasten).

**Écart.** spacebot **empile puis nettoie** (merge brutal par concaténation à 0.95) ; il ne fait ni **update
sémantique** (« la préférence X a changé »), ni **invalidation** d'un fait contredit. La concaténation de contenus au
merge est un *anti-pattern* (elle gonfle le texte au lieu de réécrire).

**À emprunter (le plus rentable du doc).** Le **pipeline d'écriture Mem0** : sur `memory_save` (ou dans la persistence
branch), récupérer le top-k similaire et demander au LLM `{ADD|UPDATE|DELETE|NOOP}` + le contenu réécrit. Remplace la
fois (le merge-concaténation post-hoc *et* le besoin de seuil magique 0.95). Bénéfice rapporté par Mem0 : forte
réduction de tokens/latence vs contexte plein et qualité supérieure sur LOCOMO *(vendor-reported)*.

**Coût / valeur.** Moyen / **élevé**. **SurrealDB aide** : top-k + réécriture + invalidation dans un seul store
transactionnel (pas de va-et-vient SQLite↔Lance).

---

### Axe 3 — Structure de connaissance / relations typées / entités

**Actuel (vérifié).** 8 `MemoryType` (`types.rs:77-89`), 6 `RelationType` (`RelatedTo, Updates, Contradicts, CausedBy,
ResultOf, PartOf`, `types.rs:178-193`). Les arêtes sont **toujours créées à la main** par le LLM (`associations: [...]`
sur `memory_save`), avec validation d'existence de la cible (`memory_save.rs:274-295`). **Pas d'extraction d'entités,
pas de normalisation/résolution d'entités, pas d'ontologie.**

**SOTA.** **Graphiti/Zep** et **Cognee** construisent un **graphe d'entités** typé par LLM : extraction d'entités,
**résolution d'entités** (dédupe « Bob » = « Robert »), arêtes sémantiques inférées. **mem0g** (variante graphe de Mem0)
infère relations sujet-prédicat-objet. **HippoRAG** s'appuie sur ce KG pour le multi-hop.

**Écart.** spacebot a la *forme* (types d'arêtes) mais pas l'**inférence automatique** ni la couche **entités**. Le
graphe ne s'enrichit que si le LLM pense à poser des arêtes.

**À emprunter.** Faire **inférer les relations** par la persistence branch (elle voit déjà le contexte) : « parmi les
mémoires {liste}, propose des arêtes typées ». Couche entités = chantier plus lourd (résolution d'entités) — à acter
seulement si l'usage multi-entités le justifie (YAGNI sinon).

**Coût / valeur.** Inférence d'arêtes : faible-moyen / moyen. Entités/ontologie : élevé / variable. **SurrealDB aide**
(RELATE natif, traversée `{..N+collect}` déjà en place).

---

### Axe 4 — Raisonnement temporel / bi-temporalité  ⭐ différenciateur

**Actuel (vérifié).** **Absent.** `Memory` n'a que `created_at / updated_at / last_accessed_at` (transaction-time).
Grep `valid_from|valid_to|as_of|bitemporal|fact_time` dans `src/memory/` → **zéro**. Impossible de répondre « que
savais-je de X *au* 12 mars ». `Updates`/`Contradicts` ne servent **pas** à filtrer/invalider au retrieval.

**SOTA.** **Zep/Graphiti** est le référent : modèle **bi-temporel à 4 timestamps** — `t_valid`/`t_invalid` (intervalle
où le *fait* a été vrai) **distincts** de `t_created`/`t_expired` (temps *système* de création/invalidation). Un fait
contredit voit son arête **invalidée** (datée), **pas supprimée** ; requêtes **point-in-time**. Zep rapporte +18,5 % sur
LongMemEval et −90 % de latence *(vendor-reported)*. **Pattern convergent, pas exotique :** **Cognee** (DataPoint
versionné/horodaté qui « invalide sans supprimer ») et **Mem0** (opération DELETE sur contradiction) font la même chose
— trois systèmes SOTA **invalident au lieu d'écraser**. C'est du *table-stakes*.

**Écart.** Total. C'est le plus gros manque conceptuel : spacebot ne sait pas qu'un fait a **cessé d'être vrai**.

**À emprunter.** Ajouter `valid_from` / `valid_to` (nullable) sur `Memory`, et au moment d'un UPDATE/Contradicts (axe 2),
**fermer** l'intervalle de l'ancien fait au lieu de l'oublier. Retrieval : par défaut `valid_to IS NULL` (faits
courants), avec option « as-of ».

**Coût / valeur.** Moyen / **élevé** (différenciateur réel). **SurrealDB aide beaucoup** : champs datetime + arêtes
porteuses de propriétés temporelles, requêtes natives — c'est précisément le terrain de jeu d'un moteur graphe+document.

---

### Axe 5 — Intelligence de retrieval  ⭐ quick win

**Actuel (vérifié).** Pipeline hybride complet (`search.rs:151-256`) : FTS + ANN vectoriel + traversée de graphe depuis
les seeds importance ≥ 0.8, fusionnés par **RRF** `score = Σ 1/(k+rank)`, `k=60` (`search.rs:416,445`). Scoring de
traversée : `importance × edge_weight × type_multiplier` (Updates 1.5, CausedBy/ResultOf 1.3, RelatedTo 1.0, PartOf 0.8,
Contradicts 0.5 ; `search.rs:358-365`).

Mais : **l'importance n'entre PAS dans la fusion RRF** (elle n'agit que sur le score de traversée). **Pas de boost de
récence** au retrieval. **Pas de query rewriting/expansion** (l'embedding vient de la requête brute). **Pas de reranking
LLM/cross-encoder.** **Pas de diversité/MMR.** `curate_results` = `take(max_results)` (`search.rs:482`).

**SOTA.** **Generative Agents** : score = `α·récence + β·importance + γ·pertinence` — la formule canonique qui mélange
les trois. **Zep/Mem0** : reranking après fusion. **HippoRAG** : **personalized PageRank** sur le KG pour le multi-hop.
Beaucoup font du **query rewriting** (HyDE / multi-query).

**Écart.** spacebot a la *plomberie* hybride mais une **fonction de score appauvrie** : un fait crucial récemment
consulté ne bat pas un fait obscur ancien à rang égal.

**À emprunter (gain immédiat).** Injecter **importance + récence** dans le score final (formule Generative-Agents :
pondérer le score RRF par `importance` et par un decay de `last_accessed_at`). C'est **quelques lignes** dans
`search.rs` et ça relève directement la qualité de rappel. Ensuite (optionnel) : MMR pour la diversité, puis reranking
LLM sur le top-N.

**Coût / valeur.** Importance+récence : **faible / élevé** (le meilleur ratio du doc). Reranking : moyen / moyen.
**SurrealDB aide** : multi-hop via traversée native (vers une approche PageRank-like).

---

### Axe 6 — Réflexion / synthèse / auto-organisation

**Actuel (vérifié).** **Le plus mûr.** 4 boucles LLM dans `cortex.rs` : *knowledge synthesis* (change-driven,
`cortex.rs:2903`), *bulletin* 8 sections (`2712`), *synthèse intraday* (`3184`), *résumé quotidien* (`3331`), + *profil
d'agent* (`3540`). C'est l'équivalent du **reflection tree** des Generative Agents — **déjà livré**.

**Manque (DESIGN-ONLY).** La **Phase 4 « Memory Consolidation »** de `cortex-implementation.md` (agent LLM autonome avec
`memory_consolidate` + `system_monitor`) : ces outils sont *référencés dans les prompts mais n'existent pas* (le doc le
dit lui-même). C'est l'agent qui résoudrait les contradictions cross-canal et produirait des mémoires d'ordre supérieur.

**SOTA.** Generative Agents (reflection périodique), **A-MEM** (évolution de notes). spacebot est **au niveau** sur la
synthèse descendante ; il lui manque la **boucle d'écriture autonome** (réinjecter des insights *comme nouvelles
mémoires*, pas seulement comme bulletins).

**Insight architectural — « sleep-time compute » (Letta).** L'état de l'art récent sépare le *memory shaping* de
l'interaction : un **agent sleep-time** partage la mémoire de l'agent principal et la **remodèle en arrière-plan** (idle)
via des appels type `rethink_memory()`, sortant le raisonnement lourd de la latence utilisateur (≈ −5× de calcul en
fenêtre interactive *(vendor-reported)*). **Le cortex de spacebot EST déjà un agent sleep-time.** L'implication directe :
la **consolidation (I2)** et l'**inférence d'arêtes (I5)** devraient tourner **dans le cortex en arrière-plan**, pas dans
le tour de conversation — spacebot est idéalement placé pour ça.

**À emprunter.** Implémenter la Phase 4 comme **boucle de consolidation sleep-time** : laisser le cortex **écrire des
mémoires de synthèse**, **invalider/merger** (axe 2) et **horodater** (axe 4) en arrière-plan. Naturellement couplé aux
axes 2 & 4.

**Coût / valeur.** Élevé / moyen (l'essentiel de la valeur de synthèse est déjà capté par les bulletins ; le gain neuf
est de faire tourner I2/I4 *là*, en async).

---

### Axe 7 — Oubli / décroissance / cycle de vie / tiering

**Actuel (vérifié).** Décroissance `age_decay = 1 − min(days_old·0.05, 0.5)` ; `new_importance = importance · age_decay ·
access_boost` (boost 1.1/<7j, 0.9/>30j ; `maintenance.rs:115-124`) ; prune < 0.1 après 30j ; merge à 0.95 ; soft-delete
(`forgotten`). Working-memory = log d'événements append-only avec compression intraday/quotidienne — **livré**.

**DESIGN-ONLY.** Le **tiered memory** de `tiered-memory.md` (tier *hot* working-state vs *warm* graph, TTL 3j, éviction
LRU, **boost retrieval ×1.5** pour le hot) **n'existe pas** : pas de champ `tier` (grep vide), `MemoryPromoted/Demoted`
sont des **stubs jamais émis** (`working.rs:62-65`).

**SOTA.** **Letta/MemGPT** : paging OS-like entre contexte et mémoire externe. **MemOS** : scheduling/gouvernance de
mémoire. La plupart distinguent un *hot store* récent prioritaire.

**Écart.** spacebot a la décroissance mais pas la **hiérarchie chaud/froid** au retrieval — or l'axe 5 (importance+récence
dans le score) **couvre 80% du bénéfice du tiering** sans colonne `tier`.

**À emprunter.** Faire d'abord l'axe 5 (récence dans le score) ; n'implémenter le tier explicite (`tiered-memory.md`) que
si on a besoin du *plafond borné* du hot-set. **Recommandation : YAGNI** sur le tier tant que l'axe 5 suffit.

**Coût / valeur.** Tier complet : moyen / faible (redondant avec l'axe 5). Décroissance actuelle : suffisante.

---

### Axe 8 — Personnalisation / scoping

**Actuel (vérifié).** Scoping **par agent** : effectif (un store/DB par agent). `channel_id` **stocké mais jamais filtré
au retrieval** (toutes les recherches sont channel-agnostiques) — métadonnée morte. **`user_id` mémoire : absent**
(0 occurrence dans `types.rs`). Le design `user-scoped-memories.md` (identité canonique + recall « mémoires du user +
globales ») est **DESIGN-ONLY**.

**SOTA.** Mem0/Zep scopent nativement par **user_id / session_id / agent_id**, et renvoient « mémoires du user +
partagées ». C'est indispensable en multi-utilisateur (un bot communautaire/entreprise).

**Écart.** En multi-utilisateur, **toutes les mémoires d'un agent se mélangent** : le projet d'Alice pollue le rappel de
Bob (le problème exact décrit dans `user-scoped-memories.md`).

**À emprunter.** Le design existe déjà — implémenter `user_id` optionnel + recall scopé (mémoires du user + globales).
Pré-requis : la résolution d'identité (table `user_identifiers`) déjà spécifiée. **À compléter par une couche de
gouvernance façon MemOS** : metadata de **permission/provenance par mémoire** (qui peut lire/écrire), `MemGovernance`
abstrait — c'est la dimension « access-control across users » que MemOS formalise et qu'un système multi-user exige.

**Coût / valeur.** Moyen / **élevé** — **spacebot étant multi-utilisateur par conception, ce n'est PAS optionnel** :
sans scoping, un déploiement communauté/équipe mélange les mémoires de tous les utilisateurs (le problème exact de
`user-scoped-memories.md`). **Priorité haute, pas YAGNI.**

---

## 3. Feuille de route « intelligence » (par ratio valeur/complexité)

> Chaque incrément est indépendant et livrable derrière la même discipline (plan → revue Opus → exécution).
> Les premiers sont les meilleurs ratios.

| Incrément | Axe | Effort | Valeur | Dépend de |
|---|---|:---:|:---:|---|
| **I1. Importance + récence dans le score de retrieval** | 5 | Faible | **Élevée** | — (quelques lignes `search.rs`) |
| **I2. Pipeline d'écriture Mem0 (ADD/UPDATE/DELETE/NOOP)** | 2 | Moyen | **Élevée** | top-k similaire (déjà là) |
| **I3. Bi-temporalité (valid_from/valid_to + invalidation)** | 4 | Moyen | **Élevée** | I2 (l'UPDATE ferme l'intervalle) |
| I4. Score d'importance LLM à l'extraction | 1 | Faible-moyen | Moyenne | persistence branch (déjà là) |
| I5. Inférence d'arêtes typées par la branch | 3 | Faible-moyen | Moyenne | — |
| I6. Reranking LLM + MMR/diversité (top-N) | 5 | Moyen | Moyenne | I1 |
| **I7. User-scoping (`user_id` + recall scopé) + gouvernance/permissions** | 8 | Moyen | **Élevée** | — (multi-user = besoin réel, pas hypothétique) |
| I8. Cortex Phase 4 = boucle de consolidation **sleep-time** (porte I2/I4/I5 en async) | 6 | Élevé | Moyenne | I2, I4 |
| I9. Tiered memory explicite (colonne `tier`, TTL, boost) | 7 | Moyen | Faible | retrieval-boost redondant avec I1 ; le *bornage* du hot-set reste — **à mesurer avant** |
| I10. Couche entités / résolution d'entités | 3 | Élevé | Variable | usage-dépendant — **YAGNI** par défaut |

**Cœur SOTA recommandé : I1 + I2 + I3, + I7 (gouvernance multi-user).**
- I1 corrige le retrieval à coût quasi nul.
- I2 transforme l'écriture-puis-nettoyage en **consolidation intelligente** (le cœur de Mem0) — à faire tourner en
  **sleep-time** dans le cortex (cf. axe 6).
- I3 ajoute la **dimension temporelle** (le cœur de Zep ; convergent avec Cognee/Mem0) — précisément ce que le moteur
  SurrealDB qu'on vient d'installer rend bon marché.
- **I7** n'est pas optionnel ici : spacebot étant **multi-user par conception**, le scoping + la gouvernance des
  mémoires (permissions/provenance, façon MemOS) sont un **prérequis produit**, pas un raffinement.

I1–I3 s'enchaînent logiquement (I2 produit les UPDATE que I3 horodate) ; I7 est orthogonal et peut avancer en parallèle.

---

## 4. Ce qu'on N'EMPRUNTE PAS (anti-over-engineering)

> Distinction importante (corrigée après recherche) : **MemOS n'est pas YAGNI en bloc.** Il faut scinder.

- **MemOS — substrats paramétrique & activation** (deltas de poids, KV-cache) : **YAGNI** — mécanismes d'une autre
  classe (éditer le modèle / réutiliser le cache), sans rapport avec « stocker des faits dans une DB ».
  ⚠️ **MAIS la couche gouvernance de MemOS n'est PAS YAGNI** : MemCube porte une *Metadata Header (lifecycle,
  **permission**, storage policy)* et `MemGovernance` formalise l'**access-control across users** — c'est exactement
  ce dont un spacebot multi-user a besoin. → **emprunté via I7**, pas écarté.
- **Tiering explicite (colonne `tier`)** : le *boost retrieval* est redondant avec I1 ; le seul apport restant est le
  **bornage du hot-set** (TTL/LRU) — bénéfice non démontré aujourd'hui. **À mesurer avant d'implémenter**, pas YAGNI
  par principe.
- **Couche ontologie / résolution d'entités complète** (Cognee) : coûteuse et fragile (entity-linking LLM corrompt le
  graphe en cas d'erreur). YAGNI **par défaut** — mais l'**inférence d'arêtes typées** (I5), elle, est rentable et
  retenue. Flip si des requêtes entité-centriques deviennent un besoin.
- **Paging OS-like (MemGPT)** : non pertinent — le contexte est géré par working-memory + cortex. (Le *self-editing
  memory* de Letta, lui, est déjà fait via `memory_save` et étendu par I2.)

## 5. En quoi l'infra SurrealDB (Plans A–E) sert cette couche

- **I2 (consolidation)** : top-k + réécriture + invalidation dans **un seul store transactionnel** — fini le
  va-et-vient SQLite (faits) ↔ Lance (vecteurs).
- **I3 (bi-temporalité)** : datetime + arêtes à propriétés + requêtes natives — terrain naturel d'un moteur
  graphe+document+vecteur.
- **I5/I6 (multi-hop, PageRank-like)** : traversée récursive native `{..N+collect}` déjà en place.
- La couche intelligence se construit **au-dessus du trait `MemoryBackend`** : la plupart de ces incréments touchent
  `search.rs` / la logique générique, pas les backends — donc bénéficient aux deux (SQLite et SurrealDB).

---

## 6. Frontières émergentes 2026 (à connaître, pas à construire tout de suite)

Le survey *« Memory for Autonomous LLM Agents: Mechanisms, Evaluation, and Emerging Frontiers »* (arXiv 2603.07670)
classe les mécanismes en 5 familles (compression in-context, stores augmentés-retrieval, **auto-amélioration
réflexive**, contexte virtuel hiérarchique, **gestion par politique apprise**) et liste 5 problèmes ouverts :

- **Gestion par politique apprise / admission control adaptatif** (cf. *Adaptive Memory Admission Control*, arXiv
  2603.04549 ; *Adaptive Memory Structures*, 2602.14038) : remplacer les **seuils fixes** par des politiques
  **adaptatives/apprises**. spacebot est aujourd'hui 100 % seuils fixes (merge 0.95, prune 0.1, decay 0.05, seed 0.8) —
  c'est la direction de recherche, mais **probablement YAGNI court-terme** (gain incertain vs complexité).
- **Continual consolidation** : exactement ce que I2 + le sleep-time (I8) adressent.
- **Causally-grounded retrieval** (« se souvenir du *pourquoi* ») : spacebot a déjà `CausedBy`/`ResultOf` mais ne les
  exploite pas au retrieval — **opportunité quasi-gratuite** à brancher dans I1.
- **Learned forgetting** : oubli adaptatif (vs décroissance à taux fixe). Émergent.
- **Trustworthy reflection** : fiabilité de l'auto-synthèse (pertinent pour le cortex).

Verdict : ces frontières confirment la direction (I1–I3 + sleep-time), mais les variantes *apprises/adaptatives* sont
de la recherche — à surveiller, pas à intégrer dans le premier jet.

## Sources

Survey interne associé (systèmes, patterns convergents, schéma cible) :
[`research-llm-memory-feasibility.md`](./research-llm-memory-feasibility.md) §1–2 et §6.

Recherche web (consultée le 2026-06-23) :
- Survey 2026 — *Memory for Autonomous LLM Agents* : https://arxiv.org/abs/2603.07670
- *Adaptive Memory Admission Control for LLM Agents* : https://arxiv.org/pdf/2603.04549
- Mem0 (extract → ADD/UPDATE/DELETE/NOOP ; mem0g graphe) : https://arxiv.org/html/2504.19413v1 · https://docs.mem0.ai/platform/advanced-memory-operations
- Zep/Graphiti (KG bi-temporel à 4 timestamps, edge invalidation, LongMemEval +18,5 %) : https://arxiv.org/html/2501.13956v1 · https://neo4j.com/blog/developer/graphiti-knowledge-graph-memory/
- MemOS / MemCube (Metadata Header permission/lifecycle, MemGovernance, MemScheduler) : https://arxiv.org/pdf/2507.03724 · https://arxiv.org/abs/2505.22101
- Letta — sleep-time compute (agent de remodelage mémoire en arrière-plan) : https://www.letta.com/blog/sleep-time-compute · https://docs.letta.com/guides/agents/architectures/sleeptime/
- Cognee — ECL + ontologie RDF + DataPoint versionné (« invalide sans supprimer ») : https://docs.cognee.ai/core-concepts/main-operations/cognify
- État du domaine 2026 (Mem0/Zep/Letta/Cognee) : https://mem0.ai/blog/state-of-ai-agent-memory-2026

Systèmes également référencés : Generative Agents (reflection + score récence·importance·pertinence), A-MEM (notes
Zettelkasten évolutives), HippoRAG (personalized PageRank multi-hop). Benchmarks (LOCOMO, LongMemEval) :
**vendor-reported** — à valider indépendamment avant d'en faire un argument.
