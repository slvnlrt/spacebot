# Kodex → LLM Memory : Étude de faisabilité

> **Rapatrié dans spacebot depuis Kodex** — source : `/opt/Kodex/docs/specs/2026-04-13-llm-memory-feasibility.md`,
> importé le 2026-06-22. C'est l'**étude fondatrice** qui a motivé la branche `feat/surrealdb-memory` :
> la valeur durable ici est le **survey de l'état de l'art (§1)**, les **patterns convergents (§2)** et
> l'**architecture cible SurrealDB (§6)**.
>
> **Réconciliation avec ce qui a réellement été construit :** spacebot n'a **pas** forké Kodex. Le backend
> mémoire SurrealDB a été implémenté **nativement dans spacebot**, derrière un trait `MemoryBackend`
> pluggable (Plans A–E — voir [`design.md`](./design.md), [`handoff.md`](./handoff.md)). Donc §1–2 et §6
> sont de la **recherche de référence** ; §5 et §7 (la décision « forker Kodex vs. repartir de zéro » et son
> plan de phases) sont du **contexte historique**, pas le chemin emprunté. Les patterns d'ingestion/
> consolidation à la Mem0 (§2.2) restent une piste future (cf. [`followups.md`](./followups.md)).
>
> _Document original ci-dessous, verbatim._

---

> Date : 2026-04-13
>
> Contexte : Pré-réflexion pour lancer un projet de mémoire d'agents LLM
> basé sur le moteur Kodex existant. Ce document couvre l'état de l'art,
> l'analyse de l'existant, et le plan d'approche.

---

## Table des matières

1. [État de l'art — Systèmes de mémoire LLM](#1-état-de-lart--systèmes-de-mémoire-llm)
2. [Patterns architecturaux convergents](#2-patterns-architecturaux-convergents)
3. [Analyse de Kodex — Ce qu'on a construit](#3-analyse-de-kodex--ce-quon-a-construit)
4. [Mapping Kodex ↔ Mémoire LLM](#4-mapping-kodex--mémoire-llm)
5. [Décision : partir de Kodex vs. repartir de zéro](#5-décision--partir-de-kodex-vs-repartir-de-zéro)
6. [Architecture cible](#6-architecture-cible)
7. [Plan d'implémentation](#7-plan-dimplémentation)

---

## 1. État de l'art — Systèmes de mémoire LLM

### 1.1 Mem0 (mem0.ai)

**Ce que c'est** : Couche mémoire open-source (Apache 2.0, ~48K stars GitHub, YC S24)
pour donner à n'importe quel LLM une mémoire persistante et personnalisée.

**Architecture — Pipeline en deux phases** :

- **Extraction** : Chaque paire de messages `(m_{t-1}, m_t)` est envoyée à un LLM
  (GPT-4o-mini par défaut) avec un résumé de conversation et les 10 derniers messages.
  Le LLM extrait des faits atomiques auto-suffisants (ex : "L'utilisateur est végétarien",
  "Jean est team lead chez Acme").
- **Mise à jour** : Chaque fait candidat est comparé aux 10 mémoires existantes les plus
  similaires par embeddings vectoriels. Un LLM choisit via function calling : **ADD**
  (nouvelle mémoire), **UPDATE** (enrichir une existante), **DELETE** (supprimer une
  contradiction), ou **NOOP** (pas de changement).

**Stockage hybride** :

- **Vector store (Mem0 de base)** : Mémoires en texte naturel avec embeddings denses.
  Backends : Qdrant, ChromaDB, Pinecone, pgvector, Milvus, FAISS, Redis.
- **Graph store (Mem0^g)** : Graph dirigé étiqueté `G=(V, E, L)` — nœuds = entités
  (personnes, lieux, événements) avec type, embedding, timestamp ; arêtes = triplets
  relationnels. Backend : Neo4j. Double le stockage (~14K tokens/conversation vs ~7K).

**Retrieval** :

- Base : top-k similarité vectorielle.
- Mem0^g : dual — (1) identifier les entités de la requête, traverser les relations ;
  (2) embedder la requête, retourner les triplets au-dessus d'un seuil.

**Performances (benchmark LOCOMO)** : +26% de précision vs. OpenAI Memory,
91% de réduction de latence p95 (0.2s search vs 17.1s full-context), 90% de tokens en moins.

### 1.2 OpenMemory MCP (par Mem0)

**Ce que c'est** : Serveur MCP local-first qui donne une mémoire partagée persistante
à tout client MCP (Cursor, Claude Desktop, Windsurf, VS Code Copilot).

**Architecture** : Docker local (API + Qdrant + UI React). Transport MCP via
Server-Sent Events (SSE) sur `http://localhost:8765/mcp/<client>/sse/<user_id>`.

**4 tools MCP** : `add_memories`, `search_memory`, `list_memories`, `delete_all_memories`.

**Cross-client** : Une mémoire stockée via Cursor est retrouvable depuis Claude Desktop.
Le `user_id` (via `whoami`) lie tous les clients au même pool.

### 1.3 Zep / Graphiti

**Ce que c'est** : Moteur de knowledge graph temporel pour mémoire d'agents.
Graphiti est le cœur open-source ; Zep Cloud le service managé.

**Architecture — Knowledge graph temporel `G = (N, E, phi)`** à 3 sous-graphes :

1. **Episode Subgraph** : Données brutes (messages, texte, JSON). Non-lossy, append-only.
   Liens épisodiques vers les entités sémantiques référencées.
2. **Semantic Entity Subgraph** : Entités extraites et résolues. Relations sémantiques
   typées. C'est la couche de connaissance structurée.
3. **Community Subgraph** : Clusters d'entités fortement connectées (label propagation).
   Résumés LLM par communauté. Permet les vues compressées de réseaux complexes.

**Bi-temporalité (innovation clé)** — Chaque arête porte 4 attributs temporels :

- `t'_created`, `t'_expired` : temps système (quand ingéré)
- `t_valid`, `t_invalid` : fenêtre de validité du fait (quand c'était vrai en réalité)

Permet des requêtes comme "qui était chef de projet en janvier ?" vs "qui est chef de
projet maintenant ?". Utilise des interval trees pour des requêtes historiques efficaces.

**Retrieval à 3 composants** :

1. **Search (φ)** : 3 méthodes en parallèle — cosine similarity, BM25, BFS sur graph (n-hop).
2. **Reranking (ρ)** : Reciprocal Rank Fusion, MMR, fréquence de mention, distance de nœud.
3. **Construction (χ)** : Formate les nœuds/arêtes sélectionnés en texte avec ranges temporels.

**Performances** : +18.5% d'amélioration de précision, 90% de réduction de latence vs baselines.

### 1.4 Cognee

**Ce que c'est** : Moteur de connaissances open-source pour mémoire d'agents IA
($7.5M seed, fondateurs OpenAI/FAIR).

**4 opérations async** :

1. **`add`** : Ingestion depuis 38+ formats (PDF, CSV, audio, images, code, URLs, S3).
2. **`cognify`** : Pipeline 6 étapes — classifier, vérifier permissions, extraire chunks,
   LLM-extraire entités/relations comme triplets sujet-relation-objet, générer résumés,
   embedder + committer dans graph.
3. **`memify`** : Auto-amélioration — élagage de nœuds stales, renforcement de connexions
   fréquentes, repondération d'arêtes par usage, dérivation de nouveaux faits.
4. **`search`** : 14 modes de retrieval.

**Stockage 3 couches** : Graph (Kuzu/Neo4j/FalkorDB) + Vecteurs (LanceDB) + Relationnel (SQLite).

**Différenciateur clé** : Ingestion multimodale, mémoire auto-améliorante via `memify`,
retrieval graph-first (pas vector-first).

### 1.5 Letta (ex-MemGPT)

**Ce que c'est** : Plateforme pour agents stateful où le LLM gère sa propre mémoire
— paradigme "LLM-as-Operating-System".

**Mémoire à 3 niveaux** :

1. **Core Memory (RAM)** : Petits blocs nommés ("persona", "human") injectés dans les
   instructions système. Toujours dans le context window. L'agent lit/écrit via tool calls.
2. **Recall Memory (Cache)** : Historique complet des conversations en base. Cherchable
   par date ou texte. L'agent l'interroge via tool calls.
3. **Archival Memory (Disque)** : Stockage vectoriel long-terme pour données volumineuses.

**Différenciateur clé** : L'agent est autonome sur sa mémoire. Mem0/Zep gèrent la mémoire
en externe ; Letta laisse le LLM décider quoi retenir, oublier, archiver via tool calls.

### 1.6 MemOS (agiresearch)

**Ce que c'est** : Framework académique (arxiv 2505.22101) traitant la mémoire comme
une ressource OS de première classe pour LLMs.

**3 types de mémoire** :

1. **Paramétrique** : Poids du modèle, modules LoRA.
2. **Activation** : KV-cache, poids d'attention — "mémoire de travail".
3. **Plaintext** : Documents, knowledge graphs, prompts.

**MemCube** : Abstraction standardisée avec métadonnées descriptives (timestamps, origine),
attributs de gouvernance (permissions, durée de vie, priorité, sensibilité), indicateurs
comportementaux (fréquence d'accès, scores de pertinence).

### 1.7 Neo4j Agent Memory

**Ce que c'est** : Bibliothèque mémoire graph-native par Neo4j Labs avec 3 couches.

| Type | Contenu | Modèle graph |
|------|---------|-------------|
| **Short-term** | Messages de conversation dans une session | Chaînes séquentielles avec metadata |
| **Long-term** | Entités extraites des conversations | Knowledge graph POLE+O (Person, Org, Location, Event, Object) |
| **Reasoning** | Traces de raisonnement de l'agent | Thoughts, tool calls, outcomes liés aux messages |

**Pipeline d'extraction multi-étapes** : spaCy (NER rapide) → GLiNER2 (zero-shot) →
GLiREL (extraction de relations) → LLM (refinement haute précision).

**Intégrations** : LangChain, PydanticAI, LlamaIndex, CrewAI, Microsoft Agent Framework v1.0.

### 1.8 SurrealDB Agent Memory

SurrealDB se positionne explicitement comme "the context layer for AI agents".

**Repo officiel `surrealdb/agent-memory`** : Démo en 5 étapes — schema `.surql`, ingestion
avec embeddings, query layer (vector + graph + hybrid), agent Python avec tool-calling,
`DEFINE AGENT` expérimental en SurrealQL.

**5 types de mémoire (Spectron)** :

1. **Working** : Contexte de session courante
2. **Semantic** : Knowledge graph (entités, relations, propriétés)
3. **Episodic** : Interactions historiques avec awareness temporelle
4. **Procedural** : Patterns appris et heuristiques de décision
5. **Preference** : Préférences utilisateur et patterns d'interaction

**Avantage architectural** : Tous les types de mémoire, relations graph, embeddings
vectoriels et records structurés vivent dans une seule base avec un seul langage de
requête (SurrealQL), transactions ACID, et RBAC unifié.

---

## 2. Patterns architecturaux convergents

### 2.1 Le modèle cognitif à 3 couches

Toutes les solutions matures convergent vers le même modèle, inspiré de la psychologie
cognitive :

| Couche | Rôle | Implémentation typique | Qui le fait |
|--------|------|----------------------|-------------|
| **Épisodique** | "Qu'est-ce qui s'est passé ?" | Messages horodatés, append-only | Zep, Letta, Neo4j |
| **Sémantique** | "Qu'est-ce qu'on sait ?" | Knowledge graph + vecteurs | Tous |
| **Procédurale** | "Comment faire ?" | Templates, workflows, règles apprises | MemOS, Letta (émergent) |

La mémoire **sémantique** est le cœur de tous les systèmes — c'est un knowledge graph
d'entités typées reliées par des relations typées, enrichi d'embeddings vectoriels.

### 2.2 Le pipeline d'ingestion

```
Texte brut (conversation, document, JSON)
  │
  ▼
[1] Chunking / normalisation
  │
  ▼
[2] Extraction LLM : entités + relations → triplets (sujet, relation, objet)
  │
  ▼
[3] Résolution : déduplication par similarité d'embeddings (seuil ~0.7)
  │        + résolution LLM si ambiguïté
  ▼
[4] Gestion de conflits : nouveau fait contredit un ancien ?
  │        → UPDATE / DELETE / invalidation temporelle
  ▼
[5] Stockage : graph (entités + relations) + vecteurs (embeddings)
```

**Point clé** : Le travail lourd se fait à l'écriture. On optimise pour des lectures rapides
au prix d'écritures plus lentes (~100-600ms par ingestion vs ~10-50ms pour du vector-only).

### 2.3 Le retrieval hybride

Le consensus de l'industrie est qu'aucune méthode seule ne suffit :

```
Query
  │
  ├──→ [1] Vector similarity (rapide, sémantique, ~10-50ms)
  │         → candidats larges
  │
  ├──→ [2] BM25 full-text (exact-match, mots-clés)
  │         → cas que les embeddings ratent
  │
  ├──→ [3] Graph traversal (précis, relationnel, ~100-300ms)
  │         → multi-hop depuis les entités ancrées
  │
  ▼
[4] Fusion : Reciprocal Rank Fusion (RRF, k=60)
  │
  ▼
[5] Reranking : cross-encoder LLM, MMR, fréquence, distance
  │
  ▼
Contexte final injecté dans le prompt
```

**Benchmarks** : Le retrieval hybride (vector + graph + BM25) atteint ~92% recall / 88%
precision pour les queries relationnelles, vs ~85% / 75% pour du RAG vector-only.

### 2.4 Les relations typées comme multiplicateur de qualité

Les arêtes typées dans le graph ne sont pas décoratives — elles influencent directement
le scoring du retrieval :

| Type de relation | Multiplicateur de score | Effet |
|-----------------|------------------------|-------|
| `Updates` | 1.5x | Booste le fait le plus récent |
| `CausedBy` | 1.3x | Renforce les chaînes causales |
| `ResultOf` | 1.3x | Renforce les conséquences |
| `RelatedTo` | 1.0x | Neutre |
| `PartOf` | 0.8x | Légèrement réduit (contexte large) |
| `Contradicts` | 0.5x | Démote activement les faits conflictuels |

Source : proposition NousResearch/Hermes Agent. Le BFS depuis les nœuds-ancres suit
les arêtes et multiplie les scores de pertinence par ces poids. Résultat : les mémoires
liées par `CausedBy` sont boostées 1.3x vs `RelatedTo`, et les `Contradicts` sont
activement démotées pour réduire les hallucinations.

### 2.5 La bi-temporalité

Innovation de Zep/Graphiti, adoptée par SurrealDB :

- **Event Time** (`valid_from`, `valid_to`) : quand le fait était vrai dans la réalité
- **Ingestion Time** (`created_at`, `expired_at`) : quand le système l'a appris

Sans bi-temporalité, impossible de distinguer "qui était chef de projet en janvier ?"
de "qui est chef de projet maintenant ?". Et impossible de corriger rétroactivement
un fait erroné sans perdre l'historique.

### 2.6 L'intégration via MCP

Le Model Context Protocol est devenu le standard d'intégration :

```
Agent (Claude, Cursor, Copilot...)
  │
  ▼
MCP Client ──SSE/stdio──→ MCP Server (mémoire)
                              │
                              ├── add_memories(content, metadata)
                              ├── search_memory(query, filters)
                              ├── list_memories(filters)
                              └── delete_memory(id)
```

Avantage : découple le backend mémoire du client IA. Un même store sert plusieurs
outils simultanément, avec mémoires partagées entre tous.

---

## 3. Analyse de Kodex — Ce qu'on a construit

### 3.1 Ce qu'est Kodex, abstrait du domaine cybersécurité

En retirant la couche métier (ISO 27001, SMSI), Kodex est un **graph-native knowledge
workspace** — un moteur de modélisation de domaines métier avec 4 interfaces de navigation :

- Une **table unique polymorphe** (`object` + `kind` + `props FLEXIBLE`)
- Un **graph de relations typées** à runtime entre ces entités
- Un **moteur de recherche full-text** BM25 avec facettes
- Des **vues sauvegardées** (smart folders) avec filtres arbitraires
- Un **kanban paramétrable** par kind + n'importe quel champ
- Un **graph canvas interactif** avec encodage visuel configurable
- Une **hiérarchie arborescente** via liens de composition
- Un **audit trail automatique** + versioning
- Un **système de tags** (eux-mêmes des objets, donc extensibles)
- Un **RBAC** multi-rôles

Le concept se situe à l'intersection de Notion (objets polymorphes, vues multiples),
Obsidian (graph de relations), Airtable (types configurables, vues sauvegardées),
et Neo4j Bloom (visualisation de graph interactive).

### 3.2 Qualité de l'architecture (score : 8.1/10)

| Axe | Note | Détail |
|-----|------|--------|
| Architecture backend | 9/10 | Layering Router → Service → Repository systématique |
| Modèle de données | 8/10 | Schema SurrealDB normalisé, props flexible, versioning, audit |
| Architecture frontend | 9/10 | Data layer 3 couches, TanStack Query, hooks composables |
| TypeScript | 8/10 | Interfaces strictes, `Record<string, any>` sur props (inévitable) |
| UX/UI | 9/10 | Graph glow/wave, kanban DnD optimiste, ObjectPanel, animations |
| Sécurité | 7/10 | JWT + RBAC 5 rôles, rate limiting, pas de refresh token |
| Tests | 6/10 | 116 tests unitaires, pas d'intégration Docker ni E2E |
| Documentation | 9/10 | STATUS.md, handoff, design docs, session logs |

### 3.3 Répartition générique vs. domaine-spécifique

**~65% du code est générique**, ~35% est cybersécurité-spécifique.

Le code cybersécurité est concentré dans **6 fichiers** (~500 lignes) :

**Backend (3 fichiers)** :
- `objects/service.py` (lignes 14-68, 210-376) — statuts calculés + numérotation refs
- `objects/props.py` (104 lignes) — validateurs Pydantic par kind ISO 27001
- `db/seed.py` (lignes 8-113) — link types français + tags catégorie

**Frontend (3 fichiers)** :
- `lib/constants.ts` (122 lignes) — kinds, statuts, PROPS_FIELDS, PARENT_KINDS
- `components/graph/graph-constants.ts` (226 lignes) — couleurs, formes, tailles par kind
- `lib/hooks/useProgressions.ts` — poids de statut, calcul de progression

---

## 4. Mapping Kodex ↔ Mémoire LLM

### 4.1 Ce que Kodex a déjà

| Composant mémoire LLM | Équivalent Kodex | Statut |
|----------------------|-----------------|--------|
| Entités typées (nœuds du KG) | `object` avec `kind` + `props` flexible | **Présent** |
| Relations typées (arêtes du KG) | `related` edge table + `link_type` à runtime | **Présent** — plus riche que Mem0 (qui n'a qu'un label string) |
| Full-text search BM25 | Analyzer français, facettes, scoring | **Présent** |
| Graph traversal récursif | `{..10+collect}`, descendants BFS, path finding | **Présent** — Zep fait exactement ça |
| Audit trail / historique | `history` table avec snapshots automatiques | **Présent** — base de la mémoire épisodique |
| Multi-tenant (user/agent scoping) | `created_by`, RBAC 5 rôles | **Présent** |
| Module MCP | `backend/app/mcp/` | **Existant** (Phase 4 roadmap) |
| Tags / catégorisation | Tags comme objets, avec scope et hiérarchie | **Présent** |
| Versioning | `version` sur object, conflict detection | **Présent** |
| SurrealDB v3 | Graph + BM25 + ACID dans un seul moteur | **Présent** — supporte nativement les vecteurs HNSW |

### 4.2 Ce qui manque

| Composant mémoire LLM | Ce qu'il faudrait ajouter | Complexité |
|----------------------|--------------------------|------------|
| Embeddings vectoriels | DEFINE INDEX ... HNSW sur object, pipeline d'embedding via API | Moyenne |
| Pipeline d'extraction | LLM extrait entités + relations depuis texte brut → crée objects + related | **Haute** — cœur du produit |
| Modélisation temporelle | `valid_from`/`valid_to` sur relations (bi-temporalité) | Basse — 2 champs + quelques WHERE |
| Retrieval hybride | Vector + graph + BM25 → fusion RRF | Moyenne — briques présentes, orchestration à construire |
| Contradiction / supersession | Détecter quand un fait contredit un ancien, invalider | Moyenne — LLM-in-the-loop + edge type |
| Mémoire épisodique structurée | Kind `episode` pour conversations brutes, liens vers entités | Basse — nouveau kind + relations |
| Community detection / résumés | Clustering d'entités + résumés LLM par cluster (GraphRAG) | Haute — algorithme externe |
| Serveur MCP standard | 4 tools (add, search, list, delete) via SSE/stdio | Basse |

### 4.3 Comparaison structurelle

```
Kodex aujourd'hui :
  Humain → crée manuellement des objets → les relie → navigue (graph, kanban, search)

Mémoire LLM :
  Agent → conversation → LLM extrait des faits → stocke (graph + vecteurs)
    → Agent requête → contexte injecté dans le prompt

Ce qui est partagé :
  [Entités typées] + [Relations typées] + [Graph traversal] + [BM25] + [SurrealDB]

Ce qui change :
  - Entrée  : saisie humaine       → pipeline d'extraction LLM
  - Sortie  : UI interactive        → API/MCP pour agents
  - Retrieval : search simple       → fusion hybride multi-signaux
```

La fondation data est identique. La logique applicative est à construire.

---

## 5. Décision : partir de Kodex vs. repartir de zéro

### 5.1 Inventaire fichier par fichier

| Composant | Fichiers | Réutilisable ? | Raison |
|-----------|----------|---------------|--------|
| DB client + utils | `db/client.py`, `db/utils.py` | **Oui, tel quel** | Plumbing SurrealDB durci : retry, normalize, check_db_result, query_multi, to_record_id |
| Schema core | `schema.surql` | **Oui, à étendre** | Ajouter champs (embeddings, temporal) mais la structure tient |
| Auth | `app/auth/*` | **Oui, tel quel** | JWT + RBAC, rien à changer |
| Relations | `app/relations/*` | **Oui, à 90%** | RELATE, graph traversal récursif, path finding |
| Link types | `app/link_types/*` | **Oui, tel quel** | CRUD runtime de types de relations |
| Search BM25 | `app/search/*` | **Oui, à étendre** | Ajouter le volet vector + fusion |
| Config + Main | `config.py`, `main.py` | **Oui, tel quel** | Lifespan, CORS, rate limiting, error handlers |
| Objects CRUD (repo) | `objects/repository.py` | **Oui** | create, get, list, update, delete — générique |
| Objects CRUD (service) | `objects/service.py` | **Partiellement** | CRUD core oui ; computed status et ref numbering à supprimer |
| Props validators | `objects/props.py` | **Non** | 100% cybersécurité, à réécrire pour des types mémoire |
| Seed | `db/seed.py` | **Non** | Données cybersécurité |
| Views | `app/views/*` | **Peut-être** | Utile si dashboard humain d'observabilité |
| Graph positions | `app/graph_positions/*` | **Peut-être** | Idem |
| Kanban sort | `app/kanban_sort/*` | **Non** | Pas de sens pour une mémoire LLM |
| Connectors | `app/connectors/*` | **Non** | Stubs cybersécurité |
| Frontend entier | `frontend/*` | **Non** | UI humaine, pas observabilité agent |

**Bilan** : ~60% du backend est réutilisable tel quel ou avec extensions mineures.

### 5.2 Chiffrage comparatif

**Partir de Kodex** :
- Code à supprimer : ~400 lignes (props cybersec, computed status, ref numbering, seed)
- Code à garder tel quel : ~1800 lignes (DB, auth, CRUD, relations, search, config)
- Code à construire : ~2000 lignes (extraction, embeddings, retrieval hybride, MCP, dashboard)
- **Total à écrire : ~2400 lignes**

**Partir de zéro** :
- Réécrire `db/client.py` + `db/utils.py` : ~170 lignes (subtilités SDK v3 : check_db_result, query_multi, bug silent-error, normalisation RecordID)
- Réécrire auth JWT + RBAC : ~300 lignes
- Réécrire CRUD objet + repository pattern : ~400 lignes
- Réécrire relations + graph traversal : ~400 lignes (patterns récursifs `{..+collect}`)
- Réécrire search BM25 + facettes : ~200 lignes
- Réécrire schema SurrealDB : ~100 lignes (avec les gotchas connus)
- Réécrire link type CRUD : ~150 lignes
- Reconfigurer FastAPI : ~100 lignes
- Plus le même code nouveau : ~2000 lignes
- **Total à écrire : ~3800 lignes**

### 5.3 Verdict

**Partir de Kodex. Fork, élaguer, construire dessus.**

Le calcul : ~3800 lignes from scratch vs ~2400 en partant de Kodex, pour le même résultat.
Les 1800 lignes de plumbing SurrealDB qu'on réécrirait seraient identiques — les contraintes
du SDK v3 imposent les mêmes patterns. Et on perdrait les bugs déjà découverts et documentés
(silent-error SDK, FETCH + RETURN AFTER incompatibles, NONE vs NULL, reserved params...).

Le seul argument pour repartir de zéro serait un changement de stack. Mais SurrealDB est
le meilleur choix pour ce use case (graph + vecteurs + BM25 + ACID dans un seul moteur),
et c'est exactement ce qu'on a déjà.

---

## 6. Architecture cible

### 6.1 Vue d'ensemble

```
                    ┌─────────────────────────────────────────────┐
                    │              Clients MCP / API               │
                    │  Claude · Cursor · Copilot · Apps custom    │
                    └──────────┬──────────────┬───────────────────┘
                               │ MCP (SSE)    │ REST API
                    ┌──────────▼──────────────▼───────────────────┐
                    │            Kodex Memory Server               │
                    │                                              │
                    │  ┌──────────┐ ┌───────────┐ ┌────────────┐  │
                    │  │ Ingestion│ │ Retrieval  │ │ Management │  │
                    │  │ Pipeline │ │  Engine    │ │    API     │  │
                    │  └────┬─────┘ └─────┬─────┘ └─────┬──────┘  │
                    │       │             │             │          │
                    │  ┌────▼─────────────▼─────────────▼──────┐  │
                    │  │         Service Layer (FastAPI)        │  │
                    │  │  MemoryService · EntityService ·       │  │
                    │  │  EpisodeService · RetrievalService     │  │
                    │  └────────────────┬──────────────────────┘  │
                    │                   │                          │
                    │  ┌────────────────▼──────────────────────┐  │
                    │  │       Repository Layer (SurrealDB)     │  │
                    │  │  Objects · Relations · Embeddings ·    │  │
                    │  │  History · Link Types                  │  │
                    │  └────────────────┬──────────────────────┘  │
                    └───────────────────┼──────────────────────────┘
                                        │
                    ┌───────────────────▼──────────────────────────┐
                    │              SurrealDB v3                     │
                    │  Graph (RELATE) + Vector (HNSW) + FTS (BM25) │
                    │  ACID transactions · RBAC · Events           │
                    └──────────────────────────────────────────────┘
```

### 6.2 Modèle de données (schema SurrealDB étendu)

**Tables existantes conservées** : `object`, `related`, `link_type`, `history`, `user`

**Extensions au schema existant** :

```surql
-- Ajout d'un champ embedding sur object (vecteur 1536 dims pour text-embedding-3-small)
DEFINE FIELD OVERWRITE embedding ON object TYPE option<array<float>>;
DEFINE INDEX OVERWRITE idx_object_embedding ON object FIELDS embedding
    HNSW DIMENSION 1536 DIST COSINE;

-- Bi-temporalité sur les relations
DEFINE FIELD OVERWRITE valid_from  ON related TYPE option<datetime>;
DEFINE FIELD OVERWRITE valid_to    ON related TYPE option<datetime>;
DEFINE FIELD OVERWRITE confidence  ON related TYPE option<float> DEFAULT 1.0;
DEFINE FIELD OVERWRITE source      ON related TYPE option<string>;  -- "extraction", "manual", "inference"

-- Table épisode : conversations brutes (mémoire épisodique)
DEFINE TABLE OVERWRITE episode SCHEMAFULL;
DEFINE FIELD OVERWRITE session_id  ON episode TYPE string;
DEFINE FIELD OVERWRITE agent_id    ON episode TYPE option<string>;
DEFINE FIELD OVERWRITE user_id     ON episode TYPE option<record<user>>;
DEFINE FIELD OVERWRITE role        ON episode TYPE string;           -- "user", "assistant", "system", "tool"
DEFINE FIELD OVERWRITE content     ON episode TYPE string;
DEFINE FIELD OVERWRITE embedding   ON episode TYPE option<array<float>>;
DEFINE FIELD OVERWRITE metadata    ON episode TYPE object FLEXIBLE DEFAULT {};
DEFINE FIELD OVERWRITE created_at  ON episode VALUE time::now() READONLY;
DEFINE INDEX OVERWRITE idx_episode_session ON episode FIELDS session_id;
DEFINE INDEX OVERWRITE idx_episode_agent   ON episode FIELDS agent_id;
DEFINE INDEX OVERWRITE idx_episode_embedding ON episode FIELDS embedding
    HNSW DIMENSION 1536 DIST COSINE;
DEFINE INDEX OVERWRITE idx_episode_ft_content ON episode FIELDS content
    FULLTEXT ANALYZER kodex_fr BM25;

-- Relation épisode → entité (provenance)
DEFINE TABLE OVERWRITE extracted_from TYPE RELATION IN object OUT episode ENFORCED;
DEFINE FIELD OVERWRITE confidence  ON extracted_from TYPE float DEFAULT 1.0;
DEFINE FIELD OVERWRITE created_at  ON extracted_from VALUE time::now() READONLY;
```

**Kinds mémoire** (remplacent les kinds cybersécurité) :

| Kind | Rôle | Équivalent cognitif |
|------|------|-------------------|
| `entity` | Personne, organisation, lieu, concept | Mémoire sémantique — nœuds |
| `fact` | Assertion factuelle ("Alice travaille chez Acme") | Mémoire sémantique — faits |
| `preference` | Préférence utilisateur/agent | Mémoire de préférence |
| `procedure` | Workflow, stratégie, pattern appris | Mémoire procédurale |
| `summary` | Résumé de communauté / cluster | Couche d'abstraction (GraphRAG) |
| `tag` | Catégorisation transversale | Conservé de Kodex |

**Link types mémoire** (remplacent les link types cybersécurité) :

| Nom | Inverse | Multiplicateur retrieval | Usage |
|-----|---------|-------------------------|-------|
| `related_to` | `related_to` | 1.0x | Lien générique |
| `part_of` | `contains` | 0.8x | Composition / hiérarchie |
| `caused_by` | `resulted_in` | 1.3x | Causalité |
| `updates` | `updated_by` | 1.5x | Supersession temporelle |
| `contradicts` | `contradicted_by` | 0.5x | Conflit factuel (démote) |
| `depends_on` | `required_by` | 1.2x | Dépendance |
| `extracted_from` | — | — | Provenance épisode → entité |

### 6.3 Pipeline d'ingestion

```python
class IngestionPipeline:
    """Conversation → faits structurés dans le knowledge graph."""

    async def ingest(self, content: str, metadata: MemoryMetadata) -> IngestionResult:
        # 1. Stocker l'épisode brut (mémoire épisodique, append-only)
        episode = await self.episode_service.create(content, metadata)

        # 2. Extraire entités + relations via LLM
        extraction = await self.extractor.extract(content, metadata.context_window)
        # → { entities: [{name, type, description}], relations: [{src, rel, dst}] }

        # 3. Générer embeddings pour chaque entité
        embeddings = await self.embedder.batch_embed(
            [e.description for e in extraction.entities]
        )

        # 4. Résoudre : dedup par similarité (seuil 0.7) + merge LLM si ambiguïté
        resolved = await self.resolver.resolve(extraction.entities, embeddings)
        # → pour chaque entité : CREATE (nouveau) | MERGE (existant) | SKIP

        # 5. Gérer les conflits : nouveau fait contredit un ancien ?
        conflicts = await self.conflict_detector.check(extraction.relations)
        # → pour chaque conflit : UPDATE (invalider ancien) | KEEP_BOTH

        # 6. Persister : objects + related + extracted_from (provenance)
        stored = await self.memory_service.store(resolved, conflicts, episode.id)

        return IngestionResult(
            entities_created=stored.created,
            entities_merged=stored.merged,
            relations_added=stored.relations,
            conflicts_resolved=stored.conflicts,
        )
```

**Extracteur LLM** — prompt structuré avec function calling :

```
Extrais les entités et relations factuelles de ce texte.

Pour chaque entité : nom, type (person/org/location/event/concept/object), description courte.
Pour chaque relation : source, type de relation, destination, confiance (0-1).

Ne génère que des faits explicitement présents dans le texte.
Si un fait met à jour ou contredit une connaissance antérieure, indique-le.
```

### 6.4 Pipeline de retrieval

```python
class RetrievalEngine:
    """Query → contexte pertinent fusionné multi-signaux."""

    async def retrieve(self, query: str, filters: RetrievalFilters) -> list[Memory]:
        # Embedding de la query
        query_embedding = await self.embedder.embed(query)

        # 3 recherches en parallèle
        vector_results, bm25_results, graph_results = await asyncio.gather(
            self._vector_search(query_embedding, filters, top_k=20),
            self._bm25_search(query, filters, top_k=20),
            self._graph_search(query, query_embedding, filters, max_hops=3),
        )

        # Fusion par Reciprocal Rank Fusion (k=60)
        fused = self._rrf_merge(vector_results, bm25_results, graph_results, k=60)

        # Filtrage temporel : ne garder que les faits valides
        if filters.point_in_time:
            fused = self._temporal_filter(fused, filters.point_in_time)

        # Reranking optionnel (cross-encoder LLM pour les top résultats)
        if filters.rerank:
            fused = await self._rerank(query, fused[:filters.top_k * 2])

        return fused[:filters.top_k]

    async def _graph_search(self, query, embedding, filters, max_hops):
        """BFS depuis les entités-ancres avec scoring pondéré par type de relation."""
        # 1. Trouver les entités-ancres (top-5 par vector similarity)
        anchors = await self._vector_search(embedding, filters, top_k=5)

        # 2. BFS depuis chaque ancre, scoring pondéré par link_type
        # SurrealQL : $anchor.{..3}->related->object
        neighbors = await self.repo.traverse_from(
            [a.id for a in anchors], max_hops=max_hops
        )

        # 3. Appliquer les multiplicateurs par type de relation
        for node in neighbors:
            node.score *= LINK_TYPE_MULTIPLIERS.get(node.edge_type, 1.0)

        return neighbors
```

**Requête SurrealQL hybride** (1 seul round-trip) :

```surql
-- Vector search + graph traversal + BM25 en une seule query
LET $vec_results = (
    SELECT id, title, body, props, embedding,
           vector::similarity::cosine(embedding, $query_emb) AS vec_score
    FROM object
    WHERE embedding <|20,COSINE|> $query_emb
    AND kind IN $kinds
);

LET $bm25_results = (
    SELECT id, title, body, props,
           search::score(0) + search::score(1) AS bm25_score
    FROM object
    WHERE (title @0@ $q OR body @1@ $q)
    AND kind IN $kinds
    LIMIT 20
);

LET $graph_results = (
    SELECT VALUE out FROM related
    WHERE in IN $anchor_ids
    AND (valid_to IS NONE OR valid_to > time::now())
    FETCH out
);

RETURN { vectors: $vec_results, bm25: $bm25_results, graph: $graph_results };
```

### 6.5 Serveur MCP

4 tools standard + 2 extensions :

| Tool | Description | Paramètres |
|------|-------------|-----------|
| `add_memories` | Ingérer du texte → extraction → stockage | `content`, `session_id`, `agent_id`, `metadata` |
| `search_memory` | Retrieval hybride | `query`, `top_k`, `filters` (kind, agent, temporal) |
| `list_memories` | Lister les mémoires avec filtres | `kind`, `agent_id`, `limit`, `offset` |
| `delete_memory` | Supprimer une mémoire spécifique | `memory_id` |
| `get_related` | Traverser le graph depuis une entité | `entity_id`, `max_hops`, `link_types` |
| `get_history` | Récupérer l'historique d'une entité | `entity_id`, `point_in_time` |

### 6.6 Dashboard d'observabilité (optionnel, Phase 2)

Version simplifiée du frontend Kodex, orientée lecture :

- **Graph explorer** : Visualiser le knowledge graph de l'agent (réutilise react-force-graph-2d)
- **Memory timeline** : Frise chronologique des épisodes et faits extraits
- **Search** : Recherche manuelle dans les mémoires (réutilise le search existant)
- **Stats** : Nombre d'entités, relations, épisodes, top entités par connexions

Pas de kanban, pas d'édition inline — c'est de l'observabilité, pas du workflow.

---

## 7. Plan d'implémentation

### Phase 0 — Fork et élagage (~1 jour)

- [ ] Fork Kodex → nouveau repo (ou branche `memory`)
- [ ] Supprimer `objects/props.py` (validateurs cybersec) → remplacer par kinds mémoire
- [ ] Supprimer de `objects/service.py` : computed status (lignes 14-68, 210-288), ref numbering (lignes 290-376)
- [ ] Remplacer `db/seed.py` : nouveaux link types mémoire + admin user
- [ ] Supprimer `app/kanban_sort/`, `app/connectors/`
- [ ] Supprimer le frontend entier (reconstruire plus tard si besoin)
- [ ] Mettre à jour le schema : ajouter `embedding`, `valid_from`/`valid_to`, table `episode`
- [ ] Vérifier que le backend démarre et que le CRUD fonctionne

### Phase 1 — Embeddings + Vector search (~3-4 jours)

- [ ] Pipeline d'embedding : appel API (OpenAI text-embedding-3-small ou local via Ollama)
- [ ] Génération d'embedding à la création/mise à jour d'un object
- [ ] Index HNSW sur `object.embedding` et `episode.embedding`
- [ ] Endpoint `POST /api/memory/search` avec vector search
- [ ] Tests unitaires : embedding mock, search ranking

### Phase 2 — Ingestion pipeline (~5-7 jours)

- [ ] `EpisodeService` : stockage des conversations brutes
- [ ] `ExtractorService` : extraction LLM (entités + relations) via function calling
- [ ] `ResolverService` : déduplication par cosine similarity (seuil 0.7) + merge LLM
- [ ] `ConflictDetector` : détection de contradictions, invalidation temporelle
- [ ] `MemoryService` : orchestration du pipeline complet (épisode → extraction → résolution → stockage)
- [ ] Endpoint `POST /api/memory/add` (texte brut → pipeline complet)
- [ ] Relations `extracted_from` (provenance épisode → entité)
- [ ] Tests : extraction mock, résolution, conflits

### Phase 3 — Retrieval hybride (~3-4 jours)

- [ ] `RetrievalEngine` : orchestration vector + BM25 + graph
- [ ] Fusion Reciprocal Rank Fusion (RRF)
- [ ] Multiplicateurs par type de relation (link_type → weight)
- [ ] Filtrage temporel (valid_from/valid_to)
- [ ] Endpoint `POST /api/memory/retrieve` (query → contexte ranké)
- [ ] Tests : ranking, fusion, filtrage temporel

### Phase 4 — Serveur MCP (~2-3 jours)

- [ ] Serveur MCP via SSE (compatible Claude, Cursor, Copilot)
- [ ] 6 tools : add_memories, search_memory, list_memories, delete_memory, get_related, get_history
- [ ] Scoping par agent_id + user_id
- [ ] Test d'intégration avec Claude Desktop

### Phase 5 — Dashboard d'observabilité (~5-7 jours, optionnel)

- [ ] Graph explorer (fork du GraphView Kodex, mode lecture seule)
- [ ] Memory timeline (frise chronologique)
- [ ] Search interface (fork du search Kodex)
- [ ] Stats dashboard

---

## 8. Estimation globale

| Phase | Effort | Cumulé | Livrable |
|-------|--------|--------|----------|
| Phase 0 — Fork et élagage | 1 jour | 1 jour | Backend clean qui démarre |
| Phase 1 — Embeddings | 3-4 jours | 5 jours | Vector search fonctionnel |
| Phase 2 — Ingestion | 5-7 jours | 12 jours | Texte brut → knowledge graph |
| Phase 3 — Retrieval hybride | 3-4 jours | 16 jours | Retrieval multi-signaux fusionné |
| Phase 4 — Serveur MCP | 2-3 jours | 19 jours | Utilisable par Claude/Cursor/Copilot |
| Phase 5 — Dashboard | 5-7 jours | 26 jours | Observabilité humaine |

**MVP fonctionnel (Phases 0-4) : ~3-4 semaines.**

Dashboard optionnel : +1 semaine.

---

## 9. Risques et mitigations

| Risque | Impact | Mitigation |
|--------|--------|-----------|
| HNSW SurrealDB v3 pas assez mature | Retrieval vector dégradé | Fallback sur un index externe (Qdrant via Docker) ; SurrealDB reste le graph store |
| Coût API embedding/extraction | Coût récurrent élevé | Ollama local pour les embeddings ; extraction LLM batched ; cache de dédup |
| Qualité de l'extraction LLM | Entités bruitées, relations erronées | Seuil de confiance, validation humaine optionnelle, pipeline spaCy+LLM (comme Neo4j) |
| Latence du retrieval hybride | >500ms par requête | Caching des embeddings fréquents, pré-calcul des ancres, index composites |
| Complexité de la résolution de conflits | Faits contradictoires non détectés | Commencer simple (last-write-wins + invalidation temporelle), itérer |

---

## 10. Différenciateurs vs. solutions existantes

| Aspect | Mem0 | Zep/Graphiti | Cognee | Kodex Memory |
|--------|------|-------------|--------|-------------|
| **DB unique** | Non (vector + graph séparés) | Non (Neo4j + externe) | Non (3 backends) | **Oui** — SurrealDB = graph + vector + BM25 + ACID |
| **Bi-temporalité** | Non | Oui | Non | Oui (hérité de Zep) |
| **Graph interactif** | Non | Non | Non | **Oui** — fork du GraphView Kodex |
| **Self-hosted simple** | Docker multi-conteneurs | Docker + Neo4j | Docker multi-conteneurs | **1 seul conteneur** SurrealDB + backend |
| **Relations typées pondérées** | Label string simple | Oui | Triplets | Oui + multiplicateurs au retrieval |
| **MCP natif** | Oui (OpenMemory) | Oui (graphiti-mcp) | Oui (cognee-mcp) | Oui |
| **Provenance épisodique** | Non | Oui | Partielle | Oui (extracted_from) |

**Le vrai différenciateur : la simplicité infra.** Un seul moteur de données (SurrealDB)
pour tout — graph, vecteurs, full-text, transactions — là où les concurrents assemblent
3-4 backends. Moins de moving parts = plus simple à déployer, opérer, et raisonner.

---

## Sources

### Papiers et recherche
- Mem0 Research Paper — arXiv 2504.19413
- Zep/Graphiti Paper — arXiv 2501.13956
- Microsoft GraphRAG — arXiv 2404.16130
- MemOS (agiresearch) — arXiv 2505.22101

### Documentations produit
- Mem0 docs : docs.mem0.ai
- Mem0 Graph Memory : docs.mem0.ai/open-source/features/graph-memory
- Neo4j Agent Memory : neo4j.com/labs/agent-memory
- SurrealDB Agent Memory : surrealdb.com/use-cases/agent-memory, github.com/surrealdb/agent-memory
- Cognee Architecture : cognee.ai/blog/fundamentals/how-cognee-builds-ai-memory
- Letta Memory Management : docs.letta.com/advanced/memory-management
- LangGraph Memory : docs.langchain.com/oss/python/langgraph/memory

### Comparatifs et benchmarks
- SparkCo — AI Agent Memory Comparison 2026
- MachineLearningMastery — Vector DB vs Graph RAG
- FalkorDB — Knowledge Graph vs Vector Database
- NousResearch/Hermes Agent — Structured Memory Proposal (GitHub issue #346)
