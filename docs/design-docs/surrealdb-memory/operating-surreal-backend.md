# Exploiter le backend mémoire SurrealDB (déploiement & ops)

> Date : 2026-06-23. Guide pratique pour **déployer et tester** la version SurrealDB de spacebot.
> Contexte technique : [`handoff.md`](./handoff.md). Sauvegarde : voir Décision C plus bas.

## TL;DR

```bash
# 1. Compiler avec la feature (release) :
cargo build --release --features surreal-memory --bin spacebot     # → target/release/spacebot

# 2. Activer dans la config (instance ou par agent) :
#    [defaults]
#    memory_backend = "surreal"        # défaut : "sqlite"

# 3. (Optionnel) migrer les données mémoire existantes, DAEMON ARRÊTÉ :
spacebot migrate-memory [--agent <id>]

# 4. Démarrer normalement. La mémoire de chaque agent vit dans <data_dir>/surreal/.
```

## 1. Build

La feature est **off par défaut** ; il faut compiler avec `--features surreal-memory` (la dépendance `surrealdb`
n'est sinon pas embarquée). Coût : binaire release ~321 MiB (vs ~270 sans la feature, +18 %).

- Binaire natif : `cargo build --release --features surreal-memory --bin spacebot`.
- Docker : passer la feature au `cargo build` du Dockerfile (mêmes flags). Vérifier que le dossier `skills/` est bien
  copié (cf. fix récent du Docker build).
- ⚠️ Le build **lie le runtime ONNX** (ort) — l'astuce `ORT_LIB_LOCATION` ne sert qu'au *compile-check*, pas à un build
  exécutable. ort télécharge onnxruntime au build (comme les jobs CI par défaut).

## 2. Activation (config)

Clé `memory_backend`, valeurs `"sqlite"` (défaut) ou `"surreal"`. Résolue comme les autres réglages : **défaut
d'instance** (`[defaults]`) avec **override par agent** possible.

```toml
[defaults]
memory_backend = "surreal"

# ... ou seulement pour un agent :
[[agents]]
id = "support"
memory_backend = "surreal"
```

**Garde-fou intégré :** si le binaire est compilé **sans** la feature mais que la config dit `surreal`, spacebot
**warn et retombe sur SQLite** (pas de crash) :
`"memory_backend=surreal but the `surreal-memory` feature is not compiled in; using SQLite"`. → si tu ne vois pas tes
données surreal, vérifie d'abord que le binaire est bien le build feature-on.

## 3. Layout sur disque

Tout est **par agent**, sous `agent_config.data_dir` (= `<instance_dir>/agents/<id>/data`) :

| Chemin | Backend | Contenu |
|---|---|---|
| `<data_dir>/agent.db` | SQLite | app (tasks/working-memory) **et** mémoire si `sqlite` |
| `<data_dir>/lance/` | LanceDB | embeddings + FTS si `sqlite` |
| `<data_dir>/surreal/` | SurrealKV | **mémoire (faits + graphe + vecteurs + FTS) si `surreal`** |

SQLite **reste utilisé** quel que soit le backend mémoire (c'est la DB applicative : working-memory, tasks…). SurrealDB
ne remplace **que** le stockage *mémoire long-terme* (`memories` + `associations` + embeddings).

## 4. Migration des données existantes

Si un agent a déjà de la mémoire en SQLite+Lance et que tu passes à `surreal`, **les données ne migrent pas toutes
seules** — sans migration, l'agent démarre avec une mémoire surreal **vide**.

```bash
spacebot migrate-memory --agent <id>     # un agent ; sans --agent : tous les agents configurés
```

- **Le daemon doit être ARRÊTÉ** (la commande refuse si elle détecte le daemon vivant — écritures concurrentes =
  corruption). Message : `"the spacebot daemon is running — stop it before migrating memory"`.
- **Idempotent** : ré-exécutable sans risque (upsert par UUID + index unique sur les arêtes).
- **Régénère les embeddings** (charge le modèle ONNX depuis `<instance_dir>/embedding_cache`) → un peu lent sur gros
  volumes, c'est normal.
- Affiche un compte par agent : `agent <id>: migrated N memories, M associations`.
- ⚠️ Migration **non transactionnelle** : si elle s'interrompt, relancer (idempotente). La source SQLite n'est **pas**
  supprimée — la migration est une **copie**.

## 5. Sauvegarde / restauration (Décision C — vérifiée)

Il n'existe **aucun backup in-app** pour *aucun* backend (ni SQLite/Lance, ni SurrealDB). Le backup = **copier le
`data_dir`** (qui contient `surreal/`).

- **À chaud** : possible mais pour une copie *cohérente*, **quiescer l'agent** (même contrainte que SQLite WAL / Lance).
- **À froid** (daemon arrêté) : `cp -a <data_dir> <backup>` round-trip proprement — **vérifié empiriquement**
  (test `surrealkv_cold_copy_backup_restores`). Restaurer = remettre le dossier en place.

## 6. Rollback vers SQLite — ⚠️ piège

SQLite et SurrealDB sont **deux stores séparés** dans le même `data_dir`. Conséquence :

- Repasser `memory_backend = "surreal"` → `"sqlite"` te ramène aux **données SQLite d'avant** (la migration ne les a
  pas effacées). **MAIS** toute mémoire créée *pendant* que tu tournais en surreal n'est **pas** dans SQLite → elle
  serait **perdue au rollback** (sauf migration inverse, non fournie).
- Donc : pour un **test sûr**, garde une **sauvegarde du `data_dir`** avant de basculer, et considère la phase surreal
  comme « en avant seulement » tant que tu n'as pas validé.

## 7. Quoi surveiller pendant le test

- **Démarrage** : log d'init mémoire par agent ; pas de warn « feature not compiled » (sinon tu tournes en SQLite).
- **FTS / HNSW** : la recherche plein-texte (analyzer `snowball(english)`) et l'index vectoriel HNSW (DIMENSION 384,
  F32) se construisent à l'ouverture — vérifier qu'aucune erreur d'index ne remonte.
- **Recall** : tester que `memory_recall` / le graphe dans l'UI (`/agents/$agentId/memories`) renvoie bien les mémoires
  migrées (nœuds + arêtes).
- **Maintenance** : décroissance/prune/merge tournent sur planning cortex — surveiller qu'elles s'exécutent sans erreur.
- **Disque** : `surreal/` grandit avec la mémoire ; surveiller la croissance.

## 8. Limites connues (cf. [`followups.md`](./followups.md))

- `Association.id` synthétisé (#10), schéma ré-appliqué à chaque open (#11), migration non-transactionnelle (#12,
  idempotente). Aucun n'est bloquant pour un test.
- Pas encore de *slim build* (#9) : embarquer les deux backends pèse +50 MiB.

---

**En cas de souci pendant le test, remonte-moi :** le message d'erreur exact, l'agent concerné, et si le binaire est
bien le build feature-on (`spacebot migrate-memory --help` doit exister — sinon c'est un binaire feature-off).
