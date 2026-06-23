# Architectures des « big players » de la mémoire — comparatif technique

> Date : 2026-06-23. Synthèse de recherche web (5 deep-dives sourcés) sur les **choix techniques et
> architecturaux** des principaux systèmes de mémoire d'agents : **Mem0, Zep/Graphiti, Letta/MemGPT, Cognee,
> MemOS, Honcho**. Complète l'[étude de faisabilité](./research-llm-memory-feasibility.md) (le « pourquoi ») et le
> [gap analysis](./gap-analysis-intelligence.md) (spacebot vs SOTA). Ici : **comment chacun est bâti**, et **où se
> situe le pari SurrealDB de spacebot**.
>
> Sources : papiers arXiv (Mem0 2504.19413, Zep 2501.13956, MemGPT 2310.08560, MemOS 2507.03724), docs officielles,
> GitHub, DeepWiki. Chiffres de benchmark = *auteur/vendor-reported* (cf. gap analysis §8). [unverified] = non confirmé.

---

## 1. Tableau comparatif maître

| | **Stockage** | **Ingestion** | **Retrieval** | **Temporel** | **Async/fond** | **Multi-user / gouvernance** | **Déploiement** |
|---|---|---|---|---|---|---|---|
| **Mem0** | Vector store **pluggable** (Qdrant embarqué par défaut, 20+ backends) + SQLite/PG (historique) ; **graph-dans-le-vector** en v3 (collection `_entities`) | LLM **ADD/UPDATE/DELETE/NOOP** vs top-10 similaires (v3 = single-pass) | Hybride multi-signal (sémantique + BM25 + entités), fusion 1 score | Soft-delete (v2) / overwrite (v3) ; pas de bi-temporel | Résumé async ; écritures faits ~sync | Scopes `user/agent/app/run` indexés (TAG) | Lib OSS (embarqué) · serveur Docker · cloud · MCP |
| **Zep / Graphiti** | **Graph DB** (Neo4j déf. / FalkorDB / Neptune) ; **embeddings DANS le graphe** (1024-d BGE-m3) ; BM25 via Lucene | Extraction entités+arêtes → **résolution d'entités** → **invalidation temporelle** d'arêtes contredites | Hybride (cos + BM25 + BFS graphe) + **rerankers** (RRF/MMR/cross-encoder/épisodes/distance) ; **pas de LLM dans le hot-path** (<300 ms) | **Bi-temporel à 4 timestamps** (valid/invalid + created/expired) ; faits jamais supprimés ; requêtes « as-of » | (extraction au write) | Zep (produit) : RBAC/ABAC/audit/retention ; Graphiti OSS : rien | Graphiti OSS (self-host Neo4j) · Zep cloud/BYOC · MCP |
| **Letta / MemGPT** | **Postgres + pgvector** (tout : agents, messages, blocks, archival) ; SQLite en dev ; **MemFS = markdown git-versionné** (nouveau) | **Self-editing** : le LLM édite sa mémoire via outils (`memory_replace/append/rethink`) | Archival = vecteur + FTS fusionnés **RRF** ; recall = recherche plein-texte | (pas temporel ; recursive summarization à l'éviction) | **Sleep-time agent** : agent de fond partageant les blocks, `rethink_memory()` async | Blocks **partageables entre agents** (attach/detach) ; MemFS = worktrees git isolés | Self-host Docker · Letta Cloud · Letta Code (local, git) |
| **Cognee** | **Tri-store** : graphe (Kuzu déf./Neo4j) + vector (**LanceDB** déf./pgvector) + relationnel (SQLite/PG) ; cohérence par **UUID partagé** | Pipeline **ECL** (Extract→**Cognify**→Load) ; extraction LLM structurée (`instructor`) → `KnowledgeGraph` | **15 modes** (GRAPH_COMPLETION déf., RAG, hybride, CoT, Cypher, NL→Cypher, temporal…) | « Temporal cognification » : events datés en append ; remplacement au niveau chunk | `memify()` : enrichissement post-graphe (consolidation d'entités, règles…) | **NodeSets** (`belongs_to_set`) pour l'isolation par sujet/tenant ; ACL dataset | Lib OSS (embarqué : SQLite+LanceDB+Kuzu) · MCP · Docker/Modal/K8s |
| **MemOS** | **Multi-backend** : Neo4j + Qdrant + SQLite + **Redis Streams** (scheduler) + ES/S3 | `MemReader` → **MemCube** (payload + metadata header) ; 3 substrats : plaintext / activation (KV-cache) / paramétrique (LoRA) | Hybride symbolique (graphe) + sémantique (vecteur), routage par tâche | `MemLifecycle` (FSM 5 états) + « Time Machine » snapshots/rollback | **MemScheduler** (Redis Streams, ms-level) : placement/éviction cross-type | **MemGovernance** : ACL **par MemCube**, TTL, audit ; **MemStore** = marketplace pub/sub inter-agents | OSS Apache-2.0 · cloud · serveur NEO / plugin MCP |
| **Honcho** | **Postgres + pgvector** (HNSW + GIN/FTS) ; Redis (workers) ; Turbopuffer/LanceDB optionnels | **Inference-at-ingest** : modèle fine-tuné (**Neuromancer XR**, Qwen3-8B) → conclusions typées (explicit/déductif) | **Dialectic** = agent **tool-using** (pas de lookup direct) ; hybride RRF (pgvector + FTS) | (conclusions horodatées ; pas de bi-temporel formel [unverified]) | **Dreamer** : raisonnement de fond idle-triggered (déductif/inductif/abductif), priorisé par *surprisal* | **Peer** (humain/agent/idée) ; collections clé **(observer, observed)** ; isolation par `workspace` | OSS (Docker PG+Redis) · cloud · MCP |
| **➡ spacebot** | **SurrealDB unique embarqué** (graphe + vecteur HNSW + document + FTS BM25 **dans un seul moteur**) ; SQLite reste la DB applicative | **Persistence branch** (LLM batch relit la conv) ; `memory_save` = **insert brut** + merge post-hoc (0.95) | Hybride **RRF (vecteur+FTS+graphe)** ; **pas** d'importance/récence en fusion, pas de rerank | **Aucun** (created/updated_at seulement) | **Cortex** (= sleep-time : 4 boucles de synthèse LLM) | **Per-agent** (un store/agent) ; `user_id` mémoire **absent** ; pas de gouvernance | Binaire embarqué (feature-gated), **per-agent embedded** |

---

## 2. Analyse par axe architectural

### 2.1 Stockage — quatre familles, et le choix de spacebot

Les big players se répartissent en **4 patterns de stockage** :

1. **Poly-store** (moteurs séparés, cohérence par UUID partagé) — **Mem0** (vector + historique SQL + graph optionnel), **Cognee** (graphe + vecteur + relationnel). Souple, mais 2-3 systèmes à opérer et à garder cohérents.
2. **Graph-DB-centrique** (le graphe porte aussi les vecteurs + le FTS) — **Zep/Graphiti** (Neo4j avec index vectoriel natif + Lucene). Élégant pour le graphe, mais lié à un serveur graphe.
3. **Postgres-centrique** (un seul RDBMS + pgvector) — **Letta**, **Honcho**. Un seul système mûr, transactionnel, mais serveur PG requis.
4. **Multi-backend orchestré** — **MemOS** (Neo4j + Qdrant + SQLite + Redis). Puissant mais lourd (infra de fournisseur).

**Où est spacebot :** un **5ᵉ pattern — moteur multi-modèle unique *embarqué*** (SurrealDB : graphe + vecteur + document + FTS dans un seul process, un fichier par agent). **Aucun des big players ne fait ça.** Le plus proche philosophiquement est Zep (« une DB porte graphe+vecteurs ») mais SurrealKV est (a) **embarqué** (pas de serveur, comme les défauts Kuzu+LanceDB de Cognee) et (b) **vraiment multi-modèle** (aussi document + FTS natif, pas un graphe avec du vecteur greffé).

➡ **Observation clé : le pari de spacebot est aligné avec une vraie tendance de convergence — réduire les pièces mobiles.** Mem0 v3 a *supprimé* la DB graphe externe (graph-dans-le-vector) ; Letta/Honcho tiennent tout dans un seul Postgres. spacebot pousse cette logique plus loin : **un seul moteur pour les 4 modèles, embarqué**. C'est défendable et arguablement *plus propre* que le poly-store — au prix d'un moteur moins éprouvé que Postgres/Neo4j (d'où l'importance des gates + du backend pluggable qu'on a construit, qui permet de revenir à SQLite/Lance).

### 2.2 Ingestion — le clivage majeur : *quand* l'intelligence opère

- **Au write, inline** : Mem0 (ADD/UPDATE/DELETE/NOOP vs top-k), Zep (extraction+résolution+invalidation), Honcho (inference-at-ingest avec un **modèle fine-tuné dédié**, Neuromancer XR). → la conso/dédup/conflits sont résolus *à l'écriture*.
- **En pipeline explicite** : Cognee (ECL, user-triggered — l'utilisateur contrôle *quand* le graphe se (re)construit, pour maîtriser le coût LLM).
- **Par l'agent lui-même** : Letta (self-editing via outils).
- **En batch de fond** : **spacebot** (persistence branch qui relit la conversation). → c'est le plus proche de Honcho/Letta dans l'esprit (LLM décide), mais **sans la résolution de conflit au write** (insert brut + merge post-hoc) — c'est exactement l'écart I2 du gap analysis.

➡ Tendance : **inférence au write** (Honcho va jusqu'à un modèle fine-tuné spécialisé). spacebot fait l'inférence au write *partiellement* (la branch), mais sans la phase « comparer aux similaires et décider UPDATE/DELETE ».

### 2.3 Retrieval — convergence quasi-totale, sauf le reranking

**Tout le monde converge sur l'hybride** (sémantique + BM25/keyword + graphe) **fusionné par RRF**. spacebot a exactement ça. Les différenciateurs :
- **Reranking** : Zep offre 5 rerankers (RRF/MMR/cross-encoder/épisodes/distance) ; Mem0 a une passe de rerank ; spacebot **n'en a aucun** (→ I6).
- **Pas de LLM dans le hot-path** : choix délibéré de Zep pour <300 ms. spacebot aussi (bon réflexe latence).
- **Retrieval agentique** : Honcho (Dialectic = agent tool-using qui itère) et Cognee (modes CoT/context-extension) font du retrieval *itératif* piloté LLM — plus cher, plus puissant.

### 2.4 Temporel — Zep est seul vraiment en avance

**Zep/Graphiti** est l'unique implémentation bi-temporelle complète (4 timestamps + invalidation, faits jamais supprimés, « as-of »). Cognee fait une version append (events datés). Mem0 overwrite. **spacebot : rien** (→ I3, le mur de tout le monde mais Zep prouve que c'est le différenciateur). Note : SurrealDB rend I3 bon marché (datetime + propriétés d'arêtes natives).

### 2.5 Traitement asynchrone / « sleep-time » — spacebot est bien placé

Pattern convergent et **fort** : sortir le travail mémoire du chemin de latence utilisateur.
- **Letta** : sleep-time agent (partage les blocks, `rethink_memory()`).
- **Honcho** : Dreamer (idle, priorisé par surprisal, raisonnement déductif/inductif/abductif).
- **MemOS** : MemScheduler (Redis Streams).
- **spacebot** : **le cortex EST un agent sleep-time** (4 boucles de synthèse). → on est architecturalement alignés ; le manque est d'y faire tourner la **consolidation** (I2) et l'**inférence d'arêtes** (I5), pas la synthèse (déjà là).

### 2.6 Multi-user / gouvernance — Honcho & MemOS montrent la cible

C'est l'axe où spacebot (multi-user par conception) a le plus à apprendre :
- **Honcho** : le **peer** (humain/agent/idée) + collections **(observer, observed)** → modélise « ce qu'Alice sait de Bob » au niveau du *schéma*. Isolation par `workspace`. C'est la **theory-of-mind** structurelle.
- **MemOS** : **MemGovernance** = ACL **par MemCube** + TTL + audit ; **MemStore** = marketplace pub/sub inter-agents.
- **Mem0** : scopes `user/agent/app/run` indexés (le minimum vital).
- **Letta** : blocks partageables entre agents (mémoire commune multi-agent).
- **spacebot** : per-agent isolé, **pas de user_id**, pas de gouvernance. → I7 (+ le modèle Collaborative Memory de la note de veille).

### 2.7 Le « pari distinctif » de chacun

| Système | Pari architectural central |
|---|---|
| Mem0 | **LLM = moteur de CRUD** (pas de règles ; ADD/UPDATE/DELETE décidé par le LLM) + tout pluggable |
| Zep | **Graphe de connaissance bi-temporel** (le temps est citoyen de première classe sur chaque arête) |
| Letta | **LLM-as-OS** : mémoire auto-éditée par l'agent + état serveur + (nouveau) **mémoire = repo git de markdown** |
| Cognee | **Mémoire = pipeline ETL** + **ancrage ontologique OWL/RDF** (validation des entités) |
| MemOS | **Mémoire = ressource d'OS typée** (plaintext/activation/paramétrique) + gouvernance + marketplace |
| Honcho | **Theory-of-mind** : inférence au write (modèle fine-tuné) → représentations *perspectivales* par peer |
| **spacebot** | **Un seul moteur multi-modèle embarqué** (SurrealDB) derrière un **trait pluggable** + **cortex sleep-time** |

---

## 3. Où se situe spacebot — lecture stratégique

**Ce que spacebot a déjà de bon, validé par le comparatif :**
- **Le moteur unifié embarqué** (SurrealDB) est un pari cohérent et même en avance sur la tendance « moins de pièces mobiles » (Mem0 v3, Letta/Honcho mono-Postgres). C'est *plus* unifié qu'eux (4 modèles, 1 moteur, embarqué).
- **Le cortex = sleep-time compute** : on a déjà ce que Letta/Honcho/MemOS considèrent comme l'architecture cible pour le travail mémoire de fond.
- **Le retrieval hybride RRF + graphe** est l'état de l'art commun.
- **Le backend pluggable** (trait `MemoryBackend`) est une assurance que les autres n'ont pas formalisée : on peut faire évoluer/remplacer le moteur sans toucher la logique d'intelligence.

**Ce que le comparatif désigne comme les vrais manques (et qui les a résolus) :**
1. **Consolidation au write** (I2) — Mem0/Zep/Honcho la font ; spacebot insère brut. *À faire tourner dans le cortex (sleep-time), comme Letta/Honcho.*
2. **Bi-temporalité** (I3) — seul Zep l'a vraiment ; c'est *le* différenciateur, et SurrealDB le rend bon marché.
3. **Gouvernance multi-user** (I7) — Honcho (peer/observer-observed) + MemOS (ACL par MemCube) montrent la cible ; impératif vu que spacebot est multi-user.
4. **Reranking** (I6) — Zep en a 5 ; spacebot zéro.

**Idées concrètes à emprunter, par ordre de ratio :**
- De **Mem0** : le pipeline ADD/UPDATE/DELETE/NOOP vs top-k (→ I2).
- De **Zep** : le modèle bi-temporel à 4 timestamps + invalidation d'arête (→ I3) ; les rerankers (→ I6).
- De **Honcho** : les collections **(observer, observed)** pour la mémoire perspectivale par utilisateur (→ I7/I7+) ; l'idée d'**inférence au write**.
- De **Letta** : faire tourner la consolidation dans un **agent de fond partageant la mémoire** (notre cortex) ; éventuellement l'idée MemFS (mémoire inspectable) — à méditer vu que spacebot est déjà fichier-centrique.
- De **MemOS** : la **metadata header par mémoire** (provenance, permission, TTL, importance) — un en-tête de gouvernance léger qui sert I7 *et* la décroissance usage-driven.

**À NE PAS emprunter** (cohérent avec le gap analysis) : substrats paramétrique/activation (MemOS), paging MemGPT, ontologie OWL complète (Cognee) sauf besoin entité-centrique. YAGNI.

---

## Sources

Deep-dives sourcés (web, 2026-06-23) — voir liens inline dans chaque section. Principales :
- Mem0 : arXiv 2504.19413 · docs.mem0.ai · github.com/mem0ai/mem0
- Zep/Graphiti : arXiv 2501.13956 · help.getzep.com · github.com/getzep/graphiti · neo4j.com/blog
- Letta/MemGPT : arXiv 2310.08560 · docs.letta.com · letta.com/blog · AWS Aurora case study
- Cognee : docs.cognee.ai · github.com/topoteretes/cognee · cognee.ai/blog
- MemOS : arXiv 2507.03724 / 2505.22101 · github.com/MemTensor/MemOS
- Honcho : github.com/plastic-labs/honcho · honcho.dev/docs · plasticlabs.ai (Neuromancer XR)

Tous les chiffres de benchmark sont *auteur/vendor-reported* — cf. [gap analysis §8](./gap-analysis-intelligence.md) (écarts de repro ~20 pts ; tester sur nos données).
