# Bug report — runaway ingestion & memory duplication (2026-06-23)

> Découvert en test réel (déploiement SurrealDB, mais **ces bugs ne sont PAS spécifiques au backend** —
> ils touchent aussi SQLite+Lance : ingestion, merge, UI delete, access_count sont communs).
> Symptôme initial observé : des mémoires créées **en continu, sans interaction, toujours les mêmes sujets**,
> beaucoup dupliquées/concaténées, certaines avec des `access_count` à plusieurs centaines.
>
> Chaîne complète vérifiée dans le code + les tables. À transformer en tickets + corrections.

## Résumé de la chaîne causale

Un agent LLM a écrit `knowledge-base.md` dans `workspace/ingest/`. La **boucle de polling d'ingestion**
re-traite ce fichier **à chaque cycle** ; chaque passe **sauve des mémoires** (via `memory_save`) puis **marque le
chunk `failed`** (signal de complétion manquant) → jamais enregistré comme complété → **re-traité indéfiniment** →
**doublons quasi-identiques** que la **maintenance cortex concatène** ensuite. La **suppression UI** ne retire pas le
fichier du disque → impossible d'arrêter la boucle depuis l'UI.

---

## Bugs (du plus prioritaire au moins)

### B1 — 🔴 Ingestion : re-traitement infini d'un fichier `failed`
- **Symptôme** : `knowledge-base.md` re-ingéré toutes les ~4 min de 09:28 à 16:16 (toute la journée), sans fin.
- **Preuve** : `ingestion_files` → `status=failed`, `ingestion_progress` **vide** ; `started_at`/`completed_at` se
  mettent à jour à chaque cycle.
- **Cause** : `src/agent/ingestion.rs run_ingestion_loop` (`loop { scan_ingest_dir; process_file; sleep(poll_interval_secs) }`)
  **ne filtre PAS** les fichiers déjà `failed` au scan, et `process_file` ne saute un chunk que s'il est dans
  `ingestion_progress` (table vide ici car le chunk échoue toujours) → re-traitement complet à chaque poll.
- **Manque** : pas de **max-retries**, pas de **backoff**, pas de **quarantaine** des fichiers `failed`.
- **Fix** : sauter/quarantiner les fichiers `status=failed` (ou backoff exponentiel + cap), avec une voie de
  ré-essai explicite.

### B2 — 🔴 Un chunk **sauve des mémoires** puis est marqué `failed` (effets de bord sur échec)
- **Symptôme** : à chaque retry échoué, de **nouvelles mémoires sont quand même créées**.
- **Cause** : `ingestion.rs:~466` lance un **agent LLM par chunk** (outils `memory_recall` + `memory_save`). Le LLM
  appelle `memory_save` (persistance **immédiate**), mais le chunk finit *« without memory_persistence_complete
  signal »* (`ingestion.rs:537`) → `had_failure=true` → fichier `failed`, chunk **non** enregistré complété.
  Le petit modèle (`deepseek-v4-flash`) n'émet pas fiablement le signal de fin attendu.
- **Cause sous-jacente** : (a) **écritures mémoire non transactionnelles** avec la complétion du chunk (les effets
  persistent même quand le chunk « échoue ») ; (b) dépendance à un **signal d'outil** qu'un modèle faible n'émet pas.
- **Fix** : rendre la complétion idempotente/transactionnelle ; enregistrer le progrès **dès que** des mémoires ont
  été écrites ; ne pas traiter « signal manquant » comme un échec re-essayable indéfiniment.
- **Le moment exact de la bascule** (`process_chunk`, `ingestion.rs:~533`) : l'agent LLM (modèle `deepseek-v4-flash`)
  appelle `memory_save` (persistance immédiate) puis **termine sans appeler `memory_persistence_complete`** → le code
  fait `if !contract_state.has_terminal_outcome() { return Err(...) }`. Donc « mémoires écrites » **ET** « chunk
  failed » coexistent. **Pourquoi** : (1) les écritures ne sont pas conditionnées au contrat (découplées) ; (2) le
  succès dépend d'un appel d'outil de fin qu'un petit modèle n'émet pas fiablement (cas « répond OK sans signaler » —
  distinct de `MaxTurnsError` qui a sa propre branche) ; (3) l'échec est re-essayé sans fin (B1). Le contrat suppose un
  modèle qui respecte le protocole complet, mais les effets de bord atterrissent même quand le protocole échoue.

### B3 — 🟠 UI « delete ingest file » ne supprime PAS le fichier disque (UI↔disque déconnectés)
- **Symptôme** : fichier supprimé dans l'UI → **réapparaît** quelques minutes après.
- **Cause** : `src/api/ingest.rs delete_ingest_file` fait **uniquement** `DELETE FROM ingestion_files WHERE
  content_hash=?` — **aucun `fs::remove_file`**, et ne purge pas `ingestion_progress`. Le fichier reste sur le
  disque → le poll le re-scanne → re-crée la ligne (« réapparition »). Asymétrie avec `upload_ingest_file` (qui, lui,
  écrit le fichier).
- **Fix** : le delete doit **supprimer le fichier disque** + purger `ingestion_files` **et** `ingestion_progress`.
- **Pourquoi** : le handler agit sur la **mauvaise abstraction** — il supprime l'*enregistrement de suivi* (« le job »)
  au lieu de la **source de vérité de la boucle (le fichier disque)**. Asymétrie avec `upload_ingest_file` qui, lui,
  écrit le fichier. Le delete agit une couche trop haut → le prochain poll re-découvre le fichier et re-crée la ligne.

### B4 — 🟠 Aucune déduplication à l'écriture (`memory_save` = insert brut)
- **Symptôme** : prolifération de quasi-doublons (« Format de réponse préféré… » reformulé à chaque passe).
- **Cause** : `src/tools/memory_save.rs:258` = `store.save(&memory, None)` sans comparaison aux mémoires existantes.
- **Cause sous-jacente** : c'est le **gap I2** du [gap analysis](./surrealdb-memory/gap-analysis-intelligence.md)
  (pas de pipeline ADD/UPDATE/DELETE/NOOP façon Mem0). Combiné à B1/B2, ça explose.
- **Fix** : dédup/consolidation à l'écriture (récupérer le top-k similaire → UPDATE/NOOP au lieu d'un nouvel insert).

### B5 — 🟡 Le merge de maintenance **concatène** au lieu de réécrire (bloat)
- **Symptôme** : une mémoire « unique » contenant 9 versions concaténées de la même préférence.
- **Cause** : `src/memory/maintenance.rs:293 merged_memory_content` → `format!("{winner}\n\n{loser}")` ; seul dédup =
  `winner.contains(loser)` (sous-chaîne **exacte**), inopérant sur des reformulations. Déclenché par la **maintenance
  cortex** (`cortex.rs:2456 → run_maintenance_with_cancel`, seuil 0.95).
- **Fix** : à la fusion, **garder/réécrire une version** (la plus importante/récente) au lieu de coller ; idéalement
  consolidation LLM (lié à B4/I2).

### B6 — 🟡 (cosmétique) `access_count` gonflé
- **Symptôme** : certaines mémoires à plusieurs centaines d'« accessed », la plupart normales.
- **Cause** : `access_count += 1` (`surreal_store.rs:374` / SQLite équivalent) appelé **par résultat** dans l'outil
  `memory_recall` (`memory_recall.rs:236`). Les mémoires **importance 1.0** ressortent dans le top-k à **quasiment
  chaque recall** → incrémentées en permanence. **Pas** le cortex (ses requêtes `get_by_type`/`get_sorted`
  n'incrémentent pas). Pas de double-comptage (RRF dédoublonne).
- **Impact réel** : **cosmétique** — la décroissance utilise la **récence** (`last_accessed_at`), pas la magnitude.
- **Fix** (optionnel) : ne pas compter les hits de recalls automatiques/ambiants, ou afficher différemment.

---

### Principe de fix B2 (anti-pattern à corriger en général)

**Ne pas confier une décision de control-flow/cycle-de-vie à un LLM.** « Le chunk est-il terminé ? » est
**déterministe** : le harness le sait quand `prompt_once()` retourne `Ok`. Exiger un tool-call
`memory_persistence_complete` met une décision de lifecycle entre les mains d'un modèle (non-déterministe ; un petit
modèle l'oublie). **Fix correct** : marquer le chunk *completed* **dès que le run retourne `Ok`** ; si on veut vérifier
que du travail a eu lieu, **compter les appels `memory_save`** (fait observable) au lieu d'un « je déclare avoir fini ».
Le LLM produit le *contenu/jugement* (quoi sauver) ; le *contrôle* (« done ») reste au code. À auditer partout où un
tool-call LLM sert de **signal de contrôle** que le harness connaît déjà.

## Causes sous-jacentes (transverses)

1. **Aucune couche de consolidation/dédup mémoire** (gap I2) — la cause-mère de B4/B5, et l'amplificateur de B1/B2.
2. **Effets de bord non transactionnels** — un chunk « échoué » a déjà écrit des mémoires (B2) ; un delete UI
   désynchronisé du disque (B3). Pattern « état partiellement appliqué ».
3. **Boucles de fond sans garde-fou** — l'ingestion re-tente sans limite (B1) ; la maintenance amplifie (B5).

## AUDIT — « tool-call LLM utilisé comme signal de contrôle » — RÉALISÉ (2026-06-23)

> Anti-pattern : faire dépendre une décision de control-flow/lifecycle d'un appel d'outil que le LLM doit émettre,
> alors que le harness connaît (ou pourrait connaître) la réponse de façon **déterministe**. B2 en est un cas confirmé.
> Tous les sites candidats ont été **vérifiés dans le code** (verdicts ci-dessous).

| Site | Mécanisme | Verdict (vérifié) |
|---|---|---|
| `ingestion.rs:535` (chunk) | `memory_persistence_complete` → `has_terminal_outcome()` ; sinon `Err` → poll re-traite | **🔴 CONFIRMÉ buggé (B2).** Boucle de poll **non bornée** → re-exécution infinie + saves non-transactionnels. |
| `channel_dispatch.rs:207-247` → `branch.rs:137-239` (persistance canal) | **même** `MemoryPersistenceContractState` | **🟢 PAS le même bug.** Même contrat, mais : retries **bornés à 2** (`MAX_MEMORY_CONTRACT_RETRIES`, `branch.rs:19`), re-prompt du **même** agent (history continue → pas de re-save complet), puis **abandon propre** (`break` + `Ok(conclusion)`, l.182-183). Event-driven (1 branche/tour) → **aucun re-spawn en boucle**. Résidu **bénin** : si abandon, perte des `events` du tool de complétion ; risque de doublon **borné** (≤2) si un modèle faible re-save pendant les nudges. **Priorité basse** (fixé incidemment par B4 dédup). |
| `branch.rs:37-71` (overlay générique) | contrat optionnel porté par `BranchExecutionConfig` | **🟢 Idem ci-dessus** — c'est le même mécanisme borné ; l'enforcement vit dans `Branch::run` (l.137-239) et dans `should_reject_memory_persistence_completion` (`hooks/spacebot.rs:920`). Non buggé. |
| `task_update` (`status=done` posé par le LLM) | le LLM déclare une tâche terminée (`task_update.rs:173-272`) | **🟢 PAS l'anti-pattern.** « Le *but* de la tâche est-il atteint ? » est un **jugement sémantique** que le harness ne peut **pas** connaître déterministiquement — autorité LLM légitime. Aucun effet de bord sur échec, aucune boucle : si non émis, la tâche reste `in_progress`. L'`update` **est** le signal (pas de side-effect committé « avant »). |

**Conclusion de l'audit :** le runaway est **spécifique à l'ingestion** (`ingestion.rs`). L'anti-pattern « tool-call =
signal de contrôle » n'est nuisible que là où il se combine à une **boucle de relance non bornée** + des **effets de
bord non transactionnels**. Côté canal, le **même contrat** est inoffensif parce que la relance est bornée et
event-driven. La règle reste : *le LLM produit le contenu/jugement ; le contrôle (« done ») reste déterministe dans le
code* — appliquée en priorité à B2.

**Questions posées par site (méthode) :** (a) le harness connaît-il déjà la réponse de façon déterministe ? (b) que se
passe-t-il si le LLM **n'émet pas** le signal (no-op / retry / re-spawn / perte / boucle) ? (c) des **effets de bord
sont-ils committés avant** le signal (état partiellement appliqué) ?

## À vérifier ailleurs (même pattern)

- **Email ingestion** : a aussi un `poll_interval_secs` + boucle (`config/load.rs:2281`). Même retry-forever /
  effets-de-bord-sur-échec ? À auditer.
- **Autres handlers `delete` de l'API** : sont-ils DB-only vs disque (même asymétrie que B3) ? (ex. autres ressources
  workspace.)
- **Autres flux LLM par item** dépendant d'un **signal de complétion** (comme B2) : worker/branch qui persistent puis
  signalent — mêmes effets de bord si le signal manque ?
- **La boucle d'ingestion tourne pour CHAQUE agent** (`agents.rs:1121`, `main.rs:3860`) → le bug est par-agent, donc
  reproductible sur tout agent ayant un fichier `failed` dans son `ingest/`.

## Mitigation immédiate (en attendant les fixes)

- Retirer le fichier **du disque** (pas seulement via l'UI) : `workspace/ingest/<file>` — puis purger
  `ingestion_files` + `ingestion_progress`.
- Nettoyer les mémoires dupliquées accumulées (opération de données séparée).

## Hypothèse écartée : « le fichier a contourné l'UI → mal créé en DB → tout bugue »

**Faux (vérifié).** `upload_ingest_file` (chemin UI) ne fait qu'écrire le fichier sur disque (`tokio::fs::write`) et
**ne crée aucune ligne** `ingestion_files`. C'est la **boucle de poll** (`process_file`) qui crée la ligne, de façon
idempotente, **identiquement quel que soit le mode d'arrivée** du fichier. La ligne DB actuelle est bien formée
(`total_chunks=1`, `status=failed`). Donc l'écriture directe par l'agent ≡ un upload UI : **les bugs B1/B2/B4/B5 se
produiraient à l'identique via l'UI.** Le chemin d'arrivée n'est pas la cause.

**Mais** cette observation révèle une **question de conception** : un agent peut écrire dans son propre
`workspace/ingest/` et **auto-déclencher l'ingestion** (l'ingest dir est sous le workspace accessible en écriture par
les outils de l'agent). À décider : faut-il qu'un fichier écrit par l'agent soit auto-ingéré, ou réserver l'ingestion
aux uploads explicites ?

## Statut
**Documenté. Causes vérifiées au niveau code (2026-06-23). Audit anti-pattern réalisé. Non corrigé — plan en cours.**
Bugs généraux (SQLite + SurrealDB). Ne bloquent pas le merge de la branche surreal (orthogonaux), mais le **trio
B1+B2+B3** (Lot 1) arrête le runaway et doit être corrigé avant tout usage réel de l'ingestion.

**Plan de correction (3 lots) :**
- **Lot 1 — arrêt du runaway (déterministe, sans LLM, bas risque) :** B2 (retirer le gate `has_terminal_outcome` dans
  `process_chunk` — un chunk est terminé quand `prompt_once` retourne `Ok` ; logguer `saved_memory_ids().len()` pour
  l'observabilité), B1 (cap + backoff + quarantaine via colonnes `attempts`/`next_attempt_at` sur `ingestion_files`,
  ré-armement explicite), B3 (delete = `fs::remove_file` + purge `ingestion_files` **et** `ingestion_progress`).
- **Lot 2 — consolidation (= incrément intelligence I2) :** B4 (dédup-à-l'écriture : top-k similaire → UPDATE/NOOP ;
  filet de sécurité contre re-saves sur retry transitoire), B5 (fusion = garder la version canonique au lieu de
  concaténer).
- **Lot 3 — cosmétique :** B6 (ne pas compter les recalls ambiants).

Plan d'implémentation détaillé : voir `docs/superpowers/plans/2026-06-23-ingestion-runaway-fixes.md`.
