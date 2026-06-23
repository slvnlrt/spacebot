# Notes de recherche — mémoire d'agents LLM (veille 2026)

> Date : 2026-06-23. **Doc vivant** — journal de veille curé et analysé, à enrichir au fil des lectures.
> Complète : l'[étude de faisabilité](./research-llm-memory-feasibility.md) (état de l'art « big players ») et le
> [gap analysis](./gap-analysis-intelligence.md) (spacebot vs SOTA + roadmap). Ce doc-ci se concentre sur la
> **recherche émergente** (papiers arXiv 2025-2026) et ses **implications pour spacebot**.
>
> **Confiance des sources** (honnêteté méthodo) : ✅ lu en profondeur (mécanisme + chiffres extraits) ·
> 🟡 extraction partielle/inférée par l'outil · 🔎 vu via résumés/abstracts seulement (pas lecture intégrale).
> Tous les chiffres de benchmark restent *vendor/auteur-reported* (cf. gap analysis §8 : écarts de repro ~20 pts).

---

## Thème A — Mémoire multi-agent & collaborative ⭐ (le plus pertinent pour spacebot)

spacebot est **multi-agent par conception** (org/communication graph) et **multi-user**. Or l'étude de faisabilité et
le gap analysis traitaient surtout la mémoire *par agent*. C'est le fil le plus directement actionnable.

### ✅ Collaborative Memory — Multi-User Memory Sharing with Dynamic Access Control (arXiv 2505.18279)

**Le papier le plus pertinent pour nous.** Architecture à **deux tiers** :
- **mémoire privée** `ℳ^private` (fragments visibles seulement de l'utilisateur d'origine) ;
- **mémoire partagée** `ℳ^shared` (fragments diffusés *si les permissions le permettent*).

Chaque fragment porte une **provenance immuable** : `created_at`, user d'origine, **agents contributeurs**, **ressources
accédées**. Contrôle d'accès = **deux graphes bipartites évolutifs dans le temps** : user→agent (qui peut invoquer quel
agent) et agent→ressource. Un agent servant l'utilisateur `u` ne voit un fragment cross-user que si `u` peut accéder à
**tous** les agents contributeurs **et** **toutes** les ressources utilisées (**asymétrie transitive**). À la promotion
privé→partagé, une **transformation context-aware** (prompt LLM) **retire les détails user-spécifiques** et n'extrait
que le « generally applicable knowledge ».

**Empirique :** −61 % d'utilisation de ressources à 50 % d'overlap (5 users / 6 agents / 2 556 requêtes), accuracy
>0,90 ; respect strict de la matrice d'accès même sous permissions dynamiques (octroi/révocation en temps réel).

**Implication spacebot :** c'est la **formalisation exacte de notre I7** (user-scoping + gouvernance) *étendue au
cross-agent*. Et ça **prolonge naturellement la dichotomie existante de spacebot** : aujourd'hui `spacebot.db` *global*
(tasks/projects partagés entre agents) vs `agent.db` *per-agent* (mémoire isolée). Le papier ajoute la pièce manquante :
une **mémoire partagée gouvernée par permissions**, avec provenance + transformation de partage. → modèle de référence
si on construit I7+ (partage mémoire multi-user/multi-agent).

### 🔎 Multi-agent memory : topologies, cohérence, perspective « architecture »

- **Topologies** (synthèse de plusieurs sources) : (a) *per-agent local* (ce que fait spacebot), (b) *centralized
  shared* (blackboard), (c) *hybride* (local perceptuel + world-state partagé résumé). Choisir la topologie est
  désormais un **axe de design nommé**.
- *Multi-Agent Memory from a Computer Architecture Perspective* (arXiv 2603.10062) : hiérarchie 3 couches (I/O, cache,
  mémoire), pointe les **gaps de protocole** sur le partage de cache + l'access-control structuré.
- *Scaling Teams or Scaling Time? Memory-Enabled Lifelong Learning in Multi-Agent Systems* (arXiv 2604.03295).
- **Défi ouvert n°1 cité partout : la cohérence mémoire multi-agent.**
- 🟡 *AMA: Adaptive Memory via Multi-Agent Collaboration* (arXiv 2601.20352) — **extraction échouée** (PDF en images) ;
  référencé comme pointeur, mécanisme non vérifié.

> **À décider pour spacebot (pas maintenant) :** rester *per-agent local* (statu quo, isolation forte) ou ajouter un
> **tier partagé gouverné** (modèle Collaborative Memory) pour que des agents d'une même org se partagent des faits
> sous permissions. C'est le prolongement de I7, à acter selon les besoins produit multi-user.

---

## Thème B — Mémoire procédurale / expérientielle ⭐ (gap réel : tes agents exécutent des tâches)

Distinction clé : la mémoire **épisodique/sémantique** (ce que spacebot a — des *faits*) vs la mémoire **procédurale**
(des **skills exécutables réutilisables** situation→action, appris des **traces d'exécution**). spacebot exécute des
workers/tâches mais **n'a aucune mémoire procédurale** : chaque tâche repart du raisonnement complet, rien n'est
capitalisé en « comment faire X ».

- 🔎 *Memp: Exploring Agent Procedural Memory* (arXiv 2508.06433) ; *ProcMEM* (2602.01869, PPO non-paramétrique) ;
  *Hierarchical Procedural Memory via Bayesian Selection + Contrastive Refinement* (2512.18950, AAMAS 2026).
- 🔎 *Experience Compression Spectrum* (arXiv 2604.15877) : cadre unificateur — **mémoire, skills et règles sont des
  points sur un même axe de compression** ; le « missing diagonal » = la compression adaptative cross-niveaux.
- Signal de maturité du champ : **workshop ICLR 2026 « Memory for LLM-Based Agentic Systems » (MemAgents)**.

**Implication spacebot :** piste de valeur **distincte** de I1-I7 (qui visent la mémoire de faits). Les agents
pourraient capitaliser des **procédures** depuis les traces de workers réussis (≈ skills). À rapprocher du système de
skills existant de spacebot — possible pont entre « skills » (curés) et « procédures apprises » (auto-extraites).
Frontière, pas quick-win ; mais conceptuellement importante pour un agent qui *agit*.

---

## Thème C — Politiques mémoire adaptatives / apprises

spacebot est aujourd'hui **100 % seuils fixes** (merge 0.95, prune 0.1, decay 0.05, seed 0.8). La recherche pousse vers
des politiques **apprises/adaptatives**.

- 🟡 *Adaptive Memory Admission Control for LLM Agents* (arXiv 2603.04549) : **politique d'admission apprise** (RL) qui
  décide quoi *retenir*, sur signaux **pertinence + utilité historique + récence/fréquence**, optimisée sur le succès
  de tâche. (Mécanisme fiable ; specifics de benchmark *inférés par l'outil de lecture* → à vérifier.) **Takeaway
  actionnable même sans RL** : instrumenter une **boucle de feedback** — tracer *quelles mémoires admises ont
  réellement aidé* (= signal d'utilité) — avant tout apprentissage.
- 🔎 *Choosing How to Remember: Adaptive Memory Structures* (arXiv 2602.14038) : la *structure* mémoire elle-même
  s'adapte à la tâche.

**Implication spacebot :** émergent, gain incertain vs complexité → **pas pour le premier jet**. Mais le *feedback
d'utilité* (log « cette mémoire a-t-elle servi un recall utile ? ») est un pré-requis peu coûteux qui ouvre la voie,
et améliorerait déjà la décroissance (axe 7) en la rendant *usage-driven* plutôt que purement temporelle.

---

## Thème D — Évaluation & réalité empirique

(Détaillé dans le gap analysis §8 — résumé + nouveautés ici.)

- **Ne pas faire confiance aux chiffres vendor** : repro indépendante de Mem0 LongMemEval = 73,8 % vs 93,4 % publié
  (~20 pts d'écart dus au prompt-engineering benchmark-spécifique). → tester sur **nos** données.
- ✅ **LongMemEval-V2** (arXiv 2605.12493) — *« Toward Experienced Colleagues »* : nouveau paradigme. Au lieu de
  « l'agent a-t-il rappelé le fait ? », on mesure s'il a **internalisé comment l'environnement marche** : 5 nouvelles
  capacités — **static state recall, dynamic state tracking, workflow knowledge, environment gotchas, premise
  awareness**. Constats : simple RAG s'effondre (40,1 %) ; les *gotchas* sont le plus dur (48,3 % au mieux) ; meilleur
  système *AgentRunbook-C* 72,5 %/70,1 % mais **108-140 s/requête (6-7× plus lent** que RAG). → la cible de l'éval se
  déplace vers la **connaissance procédurale/environnementale** (rejoint le thème B).
- 🔎 Benchmarks : LOCOMO, LongMemEval, **BEAM** (jusqu'à 10M tokens — falaise d'échelle 1M→10M ≈ −25 %),
  *ImplicitMemBench* (adaptation comportementale inconsciente, 2604.08064).

**Implication spacebot :** (1) se doter d'un **harness d'éval sur nos propres conversations** avant d'investir dans
l'intelligence (sinon on optimise à l'aveugle) ; (2) le temporel + les *gotchas*/procédural sont les murs durs — ne pas
sur-promettre.

---

## Thème E — Évolution dynamique / consolidation hiérarchique

- 🟡 *Chain-of-Memory* (arXiv 2601.14287) : mémoire **hiérarchique** (court/moyen/long terme) avec **consolidation
  périodique** (résumé montant de tier en tier), promotion de tier, décroissance, **réorganisation** — « dynamic
  evolution » = au-delà du CRUD : *consolider + mettre à jour + décroître + réorganiser*. (Mécanisme fiable ; chiffres
  non extraits.)
- Rejoint l'open-problem du gap analysis : « le SOTA traite le changement comme un *remplacement*, pas une *évolution* »
  (cf. note-evolution d'A-MEM). La vraie frontière de l'axe 2 (consolidation) est l'**évolution**, pas juste UPDATE/DELETE.

**Implication spacebot :** conforte que le cortex (sleep-time) est le bon endroit pour une **consolidation
hiérarchique** (working → synthèse → graphe), ce que les bulletins amorcent déjà. À garder en tête pour I8.

---

## Synthèse — ce que la veille ajoute à la roadmap du gap analysis

| Découverte | Nouveau pour spacebot ? | Rattachement |
|---|---|---|
| **Collaborative Memory** (private/shared + access-control gradué) | Oui — formalise + étend I7 au cross-agent | **I7 / I7+** |
| **Mémoire procédurale** (skills appris des traces) | **Oui — gap non identifié avant** (agents qui *agissent*) | nouveau **I11 (exploratoire)** |
| **Admission control adaptatif** + feedback d'utilité | Partiel — on est en seuils fixes | enrichit axes 1 & 7 |
| **LongMemEval-V2 / éval « experienced colleague »** | Oui — déplace la cible vers le procédural | motive un **harness d'éval interne** |
| **Consolidation hiérarchique / évolution** | Conforte le rôle du cortex sleep-time | I8 |

**Deux ajouts nets à considérer** (au-delà du cœur I1+I2+I3+I7 du gap analysis) :
1. **I7+ via le modèle Collaborative Memory** — mémoire partagée gouvernée par permissions, vu que spacebot est
   multi-user *et* multi-agent (prolonge la dichotomie global/per-agent existante).
2. **I11 (exploratoire) — mémoire procédurale** : capitaliser des procédures depuis les traces de workers réussis,
   en pont avec le système de skills existant. Frontière, mais c'est la mémoire propre aux agents qui *agissent*.

---

## Sources (consultées le 2026-06-23)

- Collaborative Memory : https://arxiv.org/abs/2505.18279 · https://arxiv.org/html/2505.18279v1
- Multi-Agent Memory (computer-arch perspective) : https://arxiv.org/html/2603.10062v1
- Scaling Teams or Scaling Time? : https://arxiv.org/pdf/2604.03295
- AMA (multi-agent, non vérifié) : https://arxiv.org/pdf/2601.20352
- Procedural memory : Memp https://arxiv.org/pdf/2508.06433 · ProcMEM https://arxiv.org/pdf/2602.01869 · Hierarchical Procedural Memory https://arxiv.org/pdf/2512.18950
- Experience Compression Spectrum : https://arxiv.org/html/2604.15877v1
- Adaptive Memory Admission Control : https://arxiv.org/pdf/2603.04549 · Adaptive Memory Structures : https://arxiv.org/pdf/2602.14038
- LongMemEval-V2 : https://arxiv.org/html/2605.12493v1 · Chain-of-Memory : https://arxiv.org/pdf/2601.14287
- Survey 2026 : https://arxiv.org/html/2603.07670v1 · ICLR 2026 MemAgents workshop : https://iclr.cc/virtual/2026/workshop/10000792
- AI Hippocampus (how far from human memory) : https://arxiv.org/pdf/2601.09113
