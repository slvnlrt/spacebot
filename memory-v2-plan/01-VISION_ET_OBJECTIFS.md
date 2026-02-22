# Vision et Objectifs : SpaceBot Memory V2

## 1. Le Contexte
SpaceBot dispose actuellement d'un système de mémoire très sophistiqué (graphe causal, typage fort, bases embarquées SQLite + LanceDB). Cependant, l'injection de mémoire repose sur un agent secondaire (le "Branch") qui décide de chercher, filtre les résultats et les retourne au Channel principal. 

**Problème actuel :**
- Coût en latence élevé (~1-3s) pour chaque recherche approfondie.
- Coût en tokens (un appel LLM supplémentaire).
- Le LLM doit "décider" de chercher, ce qui le rend parfois amnésique s'il omet de le faire.

## 2. L'Inspiration (MemOS)
MemOS a prouvé qu'une injection **systématique et silencieuse** (pre-hook) avant chaque appel LLM est redoutablement efficace. Le LLM reçoit le contexte pertinent comme s'il l'avait toujours su, sans avoir à le demander.

## 3. L'Objectif de la V2
Combiner la structure sémantique et causale de SpaceBot avec l'injection systématique de MemOS, tout en restant 100% local et sans LLM additionnel pour le flux standard.

**Objectifs chiffrés :**
- **Latence d'injection :** < 200ms (0 appel LLM).
- **Pertinence :** Injection automatique des mémoires critiques (Identity, Important, Recent) + Top-K sémantique.
- **Déduplication :** 0 redondance dans la fenêtre de contexte (filtre par ID exact + filtre sémantique cosinus > 0.85).

## 4. Le Nouveau Pipeline
1. **Vector Pre-hook (LanceDB) :** Recherche HNSW sur le message utilisateur.
2. **SQL Pre-hook (SQLite) :** Récupération des mémoires Identity, Important, Recent.
3. **Déduplication Engine :** Filtrage des mémoires déjà injectées (ID) ou sémantiquement trop proches (Cosinus).
4. **LLM Channel :** Génération de la réponse avec le contexte enrichi.
5. **Post-hook :** Mise à jour de l'état d'injection.
6. **Compactor :** Reset de l'état d'injection lors du nettoyage de la fenêtre de contexte.
