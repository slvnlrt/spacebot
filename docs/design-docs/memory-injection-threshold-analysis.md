# Analyse complète du système de Memory Injection

*Date: 2026-03-04 — Document d'investigation interne*

---

## 1. Vue d'ensemble du pipeline

Quand un message utilisateur arrive, `compute_memory_injection` exécute ce pipeline :

```
Message utilisateur
    │
    ▼
┌─────────────────────────────────┐
│ 1. Recherche hybride            │  → vector + FTS + graph + RRF fusion
│    (search_limit = 20)          │
└─────────────┬───────────────────┘
              │ N candidats bruts (MemorySearchResult avec score RRF, rank, source_signal)
              ▼
┌─────────────────────────────────┐
│ 2. Filtrage re-injection        │  → skip si memory_id injecté dans les 10 derniers turns
└─────────────┬───────────────────┘
              ▼
┌─────────────────────────────────┐
│ 3. Dédupe par ID                │  → skip si même memory_id déjà vu dans le batch
└─────────────┬───────────────────┘
              ▼
┌─────────────────────────────────┐
│ 4. Calcul cosine directe        │  → cosine(query_embedding, memory_embedding)
│    + tracking de max_cosine     │
└─────────────┬───────────────────┘
              ▼
┌─────────────────────────────────┐
│ 5. Seuil dynamique              │  → dynamic_threshold = max(max_cosine × contextual_min_score, 0.60)
│    + seuils fixes par source    │
│    + effective = max(dynamic,   │
│      source_floor)              │
└─────────────┬───────────────────┘
              ▼
┌─────────────────────────────────┐
│ 6. Dédupe sémantique            │  → skip si cosine > 0.85 avec un embedding déjà injecté
└─────────────┬───────────────────┘
              ▼
┌─────────────────────────────────┐
│ 7. Budget final                 │  → max_total = 25
└─────────────┘
```

---

## 2. Les trois sources de recherche

### 2.1. Recherche vectorielle (cosine similarity)

**Fichier**: [memory/lance.rs](../../src/memory/lance.rs) → `vector_search()`

- **Modèle d'embedding**: `all-MiniLM-L6-v2` (fastembed, 384 dimensions)
- **Métrique**: Distance cosine (LanceDB default). Le code convertit : `similarity = 1.0 - distance`
- **Limite**: `max_results_per_source` (= `search_limit` = 20)
- **Pas de seuil minimum** à ce niveau — tout est renvoyé

**Caractéristiques de all-MiniLM-L6-v2** :
- Modèle léger, optimisé vitesse
- Entraîné sur des paires de phrases anglaises (NLI + STS)
- Performances honnêtes en anglais, **nettement dégradées en français**
- Les vecteurs capturent la similarité sémantique au niveau phrastique
- Plage typique de cosine similarity entre phrases non-reliées: **0.15 – 0.55**
- Plage typique entre phrases reliées thématiquement : **0.45 – 0.75**
- Plage entre paraphrases/reformulations : **0.70 – 0.95**

**Point critique** : ce modèle est multilingue par accident (pas par design). Du texte français produit des embeddings dans un espace moins bien structuré → les scores cosine sont compressés et moins discriminants. Un score de 0.55 en français n'a pas la même signification qu'un 0.55 en anglais.

### 2.2. Recherche Full-Text (BM25/Tantivy)

**Fichier**: [memory/lance.rs](../../src/memory/lance.rs) → `text_search()` via `full_text_search()`

- **Moteur**: Tantivy (intégré dans LanceDB) — équivalent Lucene en Rust
- **Algorithme**: BM25 (probabilistically weighted term frequency)
- **Tokenisation**: Dépend de la config Tantivy/LanceDB — probablement tokeniseur standard (whitespace + lowering)
- **Limite**: `max_results_per_source` (20)
- **Post-filtre**: top 50% par score BM25 (les résultats en dessous de la médiane sont éliminés)

**Caractéristiques BM25** :
- Match **lexical exact** — les mots doivent apparaître tels quels
- Favorise les termes rares (IDF) → "Jamie Pine" score très haut si ces mots sont rares dans le corpus
- Ne comprend PAS la sémantique : "Bonsoir" ≠ "soirée" ≠ "nuit"
- Les mots fréquents/courts ("le", "on", "va") sont quasi-invisibles (IDF bas)

**Ce que BM25 fait bien** :
- Noms propres : "Jamie", "Spacebot"
- Termes techniques : "RRF", "LanceDB", "tokio"
- Expressions exactes présentes dans les mémoires

**Ce que BM25 fait mal** :
- Phrases phatiques/conversationnelles : "Bonsoir ! On va reprendre les tests" → match sur "tests" dans n'importe quel contexte
- Demandes vagues : "Tu peux m'aider ?" → "m'aider" match dans des contextes non-reliés
- Synonymes : "voiture" ne trouvera pas "automobile"

### 2.3. Recherche par graphe

**Fichier**: [memory/search.rs](../../src/memory/search.rs) → `traverse_graph()`

- **Seeds**: Mémoires avec `importance >= 0.8` (jusqu'à 20 seeds)
- **Matching**: Intersection de mots du query avec le contenu du seed (`.contains(term)`)
- **Traversal**: BFS avec profondeur max 2, via associations (RelatedTo, PartOf)
- **Score**: `importance × weight × type_multiplier`

**Point critique**: désactivé dans le pipeline d'injection (`graph_seed_limit: 0`). Aucun résultat graphe n'entre dans le pipeline actuel.

→ Le graphe n'est **pas pertinent** pour cette analyse. Seuls vector + FTS contribuent.

---

## 3. Reciprocal Rank Fusion (RRF)

**Fichier**: [memory/search.rs](../../src/memory/search.rs) → `reciprocal_rank_fusion()`

Formule : `score = Σ 1/(k + rank)` pour chaque liste où l'item apparaît, avec `k = 60`.

### 3.1. Plages de scores RRF

| Situation | Score RRF |
|---|---|
| Rank 1 dans **une seule** source | 1/61 ≈ **0.0164** |
| Rank 1 dans **deux** sources (Both) | 2/61 ≈ **0.0328** |
| Rank 5 dans une source | 1/65 ≈ **0.0154** |
| Rank 10 dans une source | 1/70 ≈ **0.0143** |
| Rank 1 vector + Rank 3 FTS | 1/61 + 1/63 ≈ **0.0323** |
| Rank 20 dans une source | 1/80 ≈ **0.0125** |

**Observations** :
- L'étendue totale des scores est **très compressée** : ~0.012 à ~0.033
- La différence entre le meilleur et le pire candidat est souvent < 0.01
- Le score RRF ne code PAS la qualité absolue du match, seulement le rang relatif
- Un candidat Both (vector + FTS) a un bonus de rang, pas un bonus de qualité

### 3.2. Source Signals après fusion

| Signal | Signification | Fréquence typique |
|---|---|---|
| `VectorOnly` | Similarité sémantique mais pas de match lexical | Reformulations, concepts proches |
| `FtsOnly` | Match lexical mais pas sémantiquement proche | Termes partagés dans contextes différents |
| `Both` | Match sémantique ET lexical | Correspondance forte et lexicale |
| `None` (graph) | Via graphe seulement | N/A (graphe désactivé) |

---

## 4. Le double système de seuils

### 4.1. Cosine directe (PAS le score RRF)

Après la fusion RRF, le pipeline re-calcule une **cosine similarity directe** entre l'embedding du message et l'embedding de chaque mémoire candidate. C'est cette valeur qui est utilisée pour le filtrage, **pas le score RRF**.

Le score RRF sert uniquement au **tri** (quels candidats passer au filtrage, dans quel ordre).

### 4.2. Seuil dynamique

```rust
const ABSOLUTE_MIN_COSINE: f32 = 0.60;
let dynamic_threshold = (max_cosine * contextual_min_score).max(ABSOLUTE_MIN_COSINE);
```

- `max_cosine` = le cosine le plus élevé parmi tous les candidats du batch
- `contextual_min_score` = 0.70 (config par défaut, peut être changé à 1.0)
- `ABSOLUTE_MIN_COSINE` = 0.60 (constant hardcodé)

**Logique** : "Ne retient que les mémoires dont la cosine est au moins X% de la meilleure". Si la meilleure cosine est 0.65, avec `contextual_min_score=0.70` → threshold = max(0.455, 0.60) = **0.60**. Avec `contextual_min_score=1.0` → threshold = max(0.65, 0.60) = **0.65**.

### 4.3. Seuils fixes par source (source floors)

```rust
let source_floor: f32 = match scored.source_signal {
    Some(SourceSignal::FtsOnly) => 0.45,
    Some(SourceSignal::Both)    => 0.50,
    _                           => 0.0,    // VectorOnly, None
};
let effective_threshold = dynamic_threshold.max(source_floor);
```

**État actuel du code** (avec le fix non-commité) : `effective = max(dynamic, source_floor)`.

Les floors ne peuvent **jamais** abaisser le seuil en dessous du dynamique. Ils sont donc **actuellement redondants** car `ABSOLUTE_MIN_COSINE (0.60)` est toujours ≥ les deux floors (0.45, 0.50).

**Avant le fix** (code original), `effective_threshold = source_floor` quand ce floor > 0 — c'est-à-dire que `FtsOnly` et `Both` **contournaient** le seuil dynamique et utilisaient des seuils plus bas (0.45 et 0.50). C'est le bug identifié.

### 4.4. Dédupliation sémantique (semantic buffer)

```rust
semantic_threshold: 0.85  // config
```

Si un candidat a une cosine > 0.85 avec une mémoire **déjà injectée** récemment (dans les 10 derniers turns), il est rejeté. Cela empêche les quasi-doublons mais pas les mémoires thématiquement non-reliées.

---

## 5. Analyse par type de message

### 5.1. Message phatique / greeting

> "Bonsoir ! On va reprendre les tests"

**Vector** : L'embedding encode le sens "salutation + reprise de tests". all-MiniLM-L6-v2 va produire un vecteur plutôt générique, orienté "conversation" et "tests". En français, la qualité de l'embedding est dégradée.
- Cosine avec une mémoire "Jamie aime le thé" : **~0.35-0.55** (faiblement lié, domaine "user preferences")
- Cosine avec "Spacebot est un projet Rust" : **~0.40-0.55** (faiblement lié, domaine "technique")
- Cosine avec "L'utilisateur a quitté à 23h" : **~0.45-0.60** (partage le concept "soirée/temps")

**FTS** : "Bonsoir", "On", "va", "reprendre", "les", "tests"
- "Bonsoir" = rare → haut IDF si apparaît dans une mémoire → match fort mais non-pertinent
- "tests" = potentiellement commun → match dans toute mémoire parlant de tests
- "On", "va", "les" = stop words de facto → IDF très bas

**Problème central** : Le `max_cosine` dans ce cas sera typiquement **0.50-0.65**. Avec `ABSOLUTE_MIN_COSINE=0.60`, le seuil sera entre 0.60 et 0.65. Mais les cosines des mauvais candidats sont aussi dans cette plage, ce qui fait que des mémoires non-pertinentes passent le filtre par quelques centièmes.

**Score attendu** : Avec `contextual_min_score=1.0`, `dynamic_threshold = max_cosine` lui-même, donc AUCUN candidat ne passe sauf celui qui a le max_cosine exact. Avec `contextual_min_score=0.70`, threshold = max(max_cosine × 0.70, 0.60) — ce qui laisse passer beaucoup trop de candidats.

### 5.2. Question factuelle spécifique

> "Comment s'appelle mon chat ?"

**Vector** : Embedding centré sur "animal domestique" + "nom" + "question". Bonne discrimination.
- Cosine avec "Le chat de l'utilisateur s'appelle Mochi" : **~0.70-0.85** ✓
- Cosine avec "L'utilisateur aime le café" : **~0.25-0.40** ✗

**FTS** : "chat", "appelle"
- "chat" → match exact dans la mémoire pertinente
- Signal `Both` probable pour la bonne mémoire

**Résultat attendu** : Bon. Le max_cosine sera élevé (~0.80), le dynamic_threshold aussi (~0.60+ avec min_score 0.70, ou ~0.80 avec 1.0). Les mauvais candidats seront loin en dessous.

### 5.3. Nom propre / entité unique

> "Qu'est-ce que Jamie pense de Rust ?"

**Vector** : Embedding centré sur "Jamie" + "opinion" + "Rust (programming)". Attention : all-MiniLM-L6-v2 n'est pas excellent pour les entités nommées — il encode le sens phrastique, pas les entités.
- Cosine avec "Jamie Pine est le fondateur" : **~0.55-0.70** (partage "Jamie" mais exprime des faits différents)
- Cosine avec "Rust est le langage de Spacebot" : **~0.50-0.65** (thème Rust mais pas Jamie)

**FTS** : "Jamie", "Rust"
- "Jamie" → rare dans le corpus → IDF très élevé → match précis
- "Rust" → peut être commun si beaucoup de mémoires techniques

**Résultat attendu** : Bon grâce au FTS. Le FTS ramène exactement les mémoires contenant "Jamie", le vector confirme. Signal `Both` pour les mémoires pertinentes. Les mémoires contenant "Rust" sans "Jamie" seront `FtsOnly` avec un cosine plus bas.

### 5.4. Requête longue et détaillée

> "La dernière fois qu'on a discuté du système de mémoire, tu avais proposé d'ajouter un seuil dynamique basé sur le meilleur score cosine. Est-ce que c'est toujours la direction qu'on prend ?"

**Vector** : Embedding dense et spécifique → discrimine bien. Le modèle encode "conversation précédente" + "système de mémoire" + "seuil dynamique" + "score cosine".
- Cosine avec la mémoire pertinente : **~0.65-0.80** (bonne correspondance thématique)
- Cosine avec mémoires non-reliées : **~0.20-0.45** (loin du thème)

**FTS** : Beaucoup de termes à chercher, mais certains sont communs. "seuil", "dynamique", "cosine" sont des termes rares → bons discriminants.

**Résultat attendu** : Bon. max_cosine élevé, bonne séparation, le seuil dynamique filtre bien.

### 5.5. Message très court / ambigu

> "Oui"
> "D'accord"
> "Ok, merci"

**Vector** : Embedding extrêmement générique. Proche du centroïde de l'espace embedding. Cosine ~0.30-0.50 avec presque tout.

**FTS** : "Oui"/"D'accord"/"Ok"/"merci" — termes très communs, IDF minimal. Peu ou pas de résultats FTS.

**Résultat attendu** : Avec `ABSOLUTE_MIN_COSINE=0.60`, tous les candidats devraient être filtrés (cosines < 0.60). **C'est le cas idéal** — pas d'injection pour un message qui ne véhicule pas de contenu informatif. Mais si une mémoire a un cosine de 0.61 par chance (bruit statistique), elle passe.

### 5.6. Code-switching (français + anglais)

> "Tu peux me faire un quick summary du PR ?"

**Vector** : all-MiniLM-L6-v2 mixe les espaces français/anglais de façon imprévisible. Le vecteur peut être proche de mémoires en anglais ou en français de façon non-déterministe.

**FTS** : "quick", "summary", "PR" — termes anglais, matchent les mémoires techniques en anglais.

**Résultat attendu** : Variable. FTS aide si les termes clés sont en anglais. Le vector est peu fiable dans ce cas mixte.

---

## 6. Problèmes structurels identifiés

### P1. Le seuil dynamique est calculé sur un batch potentiellement bruité

`max_cosine` est le MAX parmi **tous** les candidats retournés par l'hybrid search. Si la recherche ramène 20 résultats et que le meilleur cosine est 0.65 (ce qui est un match médiocre), le seuil dynamique sera 0.60 (= `ABSOLUTE_MIN_COSINE`). Les candidats entre 0.60 et 0.65 passent — mais 0.60 n'est pas un bon match, c'est du bruit.

**Le problème** : le seuil dynamique ne discrimine bien que quand il y a un vrai bon match (max_cosine > 0.75). Quand max_cosine est moyen, le seuil plancher (0.60) prend le relais et il est trop bas pour filtrer le bruit.

### P2. `contextual_min_score` à 0.70 est trop permissif

Avec la valeur par défaut `0.70`, threshold = max(max_cosine × 0.70, 0.60). Même quand max_cosine = 0.90 (excellent match), le threshold = 0.63. Ça laisse passer des candidats à 0.64 qui n'ont rien à voir.

Avec `1.0` (mode strict testé hier), threshold = max_cosine lui-même, ce qui ne retient QUE le meilleur match — c'est trop strict (un seul résultat maximum en pratique).

### P3. Le cosine à 0.60 n'est pas un bon seuil universel pour all-MiniLM-L6-v2

Pour ce modèle et du texte français, la zone 0.50-0.65 est une **zone grise** : certains bons matches y tombent, mais aussi beaucoup de bruit. Le seuil devrait être contextuel :
- Si max_cosine > 0.75 → on a un vrai signal, on peut garder threshold ≈ 0.60-0.65
- Si max_cosine < 0.70 → le signal est faible, il faudrait un threshold plus conservateur (≈ max_cosine - 0.05 ou directement rejeter)

### P4. Le score RRF ne porte pas d'information de qualité absolue

Le score RRF est purement ordinal. Un score de 0.033 signifie "rang 1 dans 2 sources", pas "excellent match". Un candidat peut être rang 1 dans le vector search avec un cosine de 0.40 (mauvais) et quand même avoir le score RRF max.

Le pipeline actuel compense partiellement en re-calculant la cosine directe. Mais le RRF sert quand même à sélectionner les candidats passés au filtrage — un mauvais candidat qui sort rang 1 de deux sources bruités sera quand même évalué.

### P5. Le filtre BM25 top-50% est relatif, pas absolu

Garder le top 50% des résultats FTS est raisonnable quand il y a de vrais matches. Mais quand la query est "Bonsoir", tous les résultats FTS sont du bruit. Le top 50% du bruit est toujours du bruit. Il manque un seuil absolu BM25 minimum.

### P6. Français + modèle anglais = compression de l'espace de scores

Avec un modèle pre-entraîné principalement sur l'anglais, le texte français produit des scores cosine dans une plage plus étroite. La différence de cosine entre un match pertinent et un match non-pertinent peut être de seulement 0.05-0.10, ce qui rend le seuillage beaucoup plus critique et sensible.

---

## 7. Tableau récapitulatif : scoring attendu par type de message

| Type de message | max_cosine typique | Nbre candidats > 0.60 | Qualité filtrage | Risque faux positifs |
|---|---|---|---|---|
| Phatique ("Bonsoir") | 0.55-0.65 | 0-5 | **Mauvais** | **Élevé** |
| Court ambigu ("Oui") | 0.45-0.55 | 0 | Bon (tout filtré) | Faible |
| Question factuelle | 0.65-0.85 | 1-3 | **Bon** | Faible |
| Nom propre | 0.55-0.70 | 1-5 | Moyen (FTS aide) | Moyen |
| Requête longue/détaillée | 0.65-0.80 | 2-6 | **Bon** | Faible |
| Code-switching fr/en | 0.50-0.70 | 0-4 | Variable | Moyen |

---

## 8. Pistes de résolution (non-implémentées, à discuter)

### Option A : Seuil adaptatif basé sur la distribution des scores

Au lieu d'un seul max_cosine, examiner la **distribution** :
```
gap = max_cosine - second_cosine
if gap > 0.10 → le top match est clairement meilleur → threshold = max_cosine - 0.10
if gap < 0.05 → les scores sont groupés → soit tous bons, soit tous bruités
    → utiliser un seuil absolu plus conservateur (0.65-0.70)
```

### Option B : Seuil absolu contextuel basé sur max_cosine

```
if max_cosine < 0.65 → pas de signal clair, skip all (ou threshold = 0.70 pour being safe)
if max_cosine 0.65-0.75 → signal moyen, threshold = max_cosine - 0.05
if max_cosine > 0.75 → bon signal, threshold = max_cosine * 0.80
```

### Option C : Intégrer le source_signal dans le seuil de façon non-triviale

Au lieu de floors fixes, moduler le seuil :
- `Both` : légère **réduction** du threshold (-0.03) car la double confirmation est un signal de qualité
- `FtsOnly` : légère **augmentation** (+0.03) car le lexical seul est peu fiable
- `VectorOnly` : threshold normal (le seuil dynamique seul suffit)

### Option D : Scorer la "query quality" avant de chercher

Analyser la query avant la recherche :
- Longueur < 15 chars → "low-information query" → seuils très conservateurs
- Contient uniquement des mots fréquents → seuils conservateurs
- Contient des noms propres / termes techniques → seuils normaux
- Longue et détaillée → seuils permissifs

### Option E : Passer à un modèle multilingue

Remplacer all-MiniLM-L6-v2 par un modèle multilingue dédié (ex: `paraphrase-multilingual-MiniLM-L12-v2`, `multilingual-e5-small`) pour améliorer la discrimination en français. Les scores cosine seraient plus étalés et les seuils plus faciles à calibrer.

---

## 9. Données concrètes observées (test du 2026-03-04)

Message : "Bonsoir ! On va reprendre les tests"
Configuration : `contextual_min_score = 1.0` (mode strict)

| Mémoire | cosine | source_signal | Décision attendue | Décision réelle (avant fix) |
|---|---|---|---|---|
| "Profil vie quotidienne..." | 0.56 | Both | REJECT (non pertinent) | **ACCEPT** (floor 0.50 < 0.56) |
| "Spacebot project facts..." | 0.58 | Both | REJECT (non pertinent) | **ACCEPT** (floor 0.50 < 0.58) |
| "Préfère le thé..." | 0.56 | Both | REJECT (non pertinent) | **ACCEPT** (floor 0.50 < 0.56) |

- `max_cosine` = 0.6503 → `dynamic_threshold` = max(0.6503 × 1.0, 0.60) = **0.6503**
- Mais l'ancien code utilisait `source_floor = 0.50` pour ces candidats `Both`, contournant le 0.6503
- Le fix (non commité) fait `effective = max(0.6503, 0.50) = 0.6503` → rejet correct

---

## 10. Conclusion

Le système a une base solide (hybrid search + RRF + cosine directe). Les problèmes sont :

1. **Le bug des source floors** (corrigé mais non commité) — les floors contournaient le seuil dynamique
2. **Le seuil absolu 0.60 est fragile** — trop bas pour filtrer le bruit en français avec all-MiniLM-L6-v2
3. **Le seuil dynamique est proportionnel à max_cosine** — il ne gère pas le cas "max_cosine est lui-même du bruit"
4. **Pas de détection de low-information queries** — le système cherche aussi activement pour "Oui" que pour "Comment fonctionne le système de mémoire ?"

Le fix minimal (source floors → `max(dynamic, floor)`) corrige le bug P3/P4 de l'ancien code. Les améliorations plus profondes (Options A-E) nécessitent des données de scoring réelles sur un corpus de queries variées pour calibrer.
