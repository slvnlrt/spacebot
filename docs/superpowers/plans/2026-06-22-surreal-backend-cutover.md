# SurrealDB Backend Conformance + Cutover — Implementation Plan (Plan B)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax.

**Goal:** Make `SurrealMemoryStore` a first-class `MemoryBackend`, selectable at runtime (behind the `surreal-memory` feature) via config, and delete the now-redundant duplicated `surreal_search.rs` / `surreal_maintenance.rs`.

**Architecture:** Plan A made memory storage pluggable behind `trait MemoryBackend` and collapsed search/maintenance onto it. Plan B implements that trait for the existing `SurrealMemoryStore` (which already has ~all the methods), adds a `memory_backend` config selector, wires the two construction sites to pick SQLite or SurrealDB, and removes the parallel `SurrealMemorySearch` / surreal maintenance code (now covered by the generic `MemorySearch` + `maintenance.rs` operating on `dyn MemoryBackend`). The default build (feature off) is completely unaffected.

**Tech Stack:** Rust 2024, tokio, `async-trait` (already a dep), embedded SurrealDB 3.1 (`surrealdb` crate, feature-gated), fastembed/ort (links the crate).

## Global Constraints

- Follow `RUST_STYLE_GUIDE.md`. `#[async_trait::async_trait]` for the impl.
- **All Surreal code stays behind `#[cfg(feature = "surreal-memory")]`.** The default (no-feature) build must be byte-for-byte unchanged — verify with a feature-off build.
- **RAM/disk safety (incident 2026-06-22):** never run unbounded/full-workspace cargo. Wrap every cargo in `systemd-run --scope -q -p MemoryMax=24G -p MemorySwapMax=0 …`; targeted builds only; `CARGO_BUILD_JOBS` is globally capped at 4. Do not loop builds; stop after two same-cause failures.
- **ONNX/ort linking:** the crate links fastembed/ort, so any `cargo build`/`test` that links needs the ORT bypass for compile-check: `mkdir -p /tmp/ortlib && ORT_LIB_LOCATION=/tmp/ortlib ORT_PREFER_DYNAMIC_LINK=1 cargo clippy --lib --features surreal-memory`. Running feature tests for real needs a working ONNX runtime; if unavailable, compile-check only and say so.
- **NEVER edit an applied migration.** No migrations change in Plan B (SurrealKV is schemaless-by-`define_schema`, not sqlx migrations).
- Decision A (per-agent SurrealKV instance, at `data_dir/surreal`) is settled — mirrors the existing per-agent data dir and `SurrealMemoryStore::open`.
- Decision B (cross-store atomicity): working memory stays on SQLite, so a `MemorySaved` write touches the memory backend (SurrealDB) and `working_memory_*` (SQLite) across two engines with no shared transaction. Accepted best-effort for the cutover — **document it in the PR summary**, do not attempt a distributed transaction.

---

## File Structure

- **Modify** `src/memory/surreal_store.rs`: add `#[async_trait] impl MemoryBackend for SurrealMemoryStore`; add inherent `get_associations_between`; the trait's `set_embedding(&Memory, …)` delegates to the existing id-based logic.
- **Modify** `src/memory.rs`: remove `pub mod surreal_search;` / `pub mod surreal_maintenance;` and their `pub use`; keep `surreal_store` (+ `surreal_migrate`). Re-export nothing new (the impl is a trait impl).
- **Delete** `src/memory/surreal_search.rs` and `src/memory/surreal_maintenance.rs` (redundant — `MemorySearch` + `maintenance.rs` are backend-agnostic after Plan A).
- **Modify** `src/config/types.rs`: add `MemoryBackendKind` enum (`Sqlite` default, `Surreal`) and a `memory_backend: MemoryBackendKind` field on `DefaultsConfig`; add the matching field to the TOML schema if `src/config/toml_schema.rs` mirrors it.
- **Modify** `src/main.rs` (~2886-2906) and `src/api/agents.rs` (~833-852): select the backend by feature + config.
- **Modify** `tests/surreal_memory.rs`: drop `SurrealMemorySearch`; exercise the generic `MemorySearch` over a `SurrealMemoryStore` backend (and `maintenance::run_maintenance` if it covered that).

### The trait gap (only two real deltas; everything else already matches)

`SurrealMemoryStore` already has, with signatures matching `MemoryBackend`: `agent_id`, `save(&Memory, Option<&[f32]>)`, `load`, `update`, `delete`, `forget`, `record_access`, `create_association`, `get_associations`, `delete_associations_for_memory`, `get_neighbors`, `get_by_type`, `get_high_importance`, `get_sorted`, `prune_below`, `merge(survivor_id, loser_id, new_content, new_embedding)`, `vector_search`, `text_search`, `find_similar`. The deltas:
1. **`set_embedding`**: inherent is `set_embedding(&self, id: &str, embedding: &[f32])`; trait is `set_embedding(&self, memory: &Memory, embedding: &[f32])`. The rename to `set_embedding_by_id` is needed because the **signatures differ** (`&str` vs `&Memory`) — NOT to avoid recursion. (Rust resolves `self.method(...)` to the inherent method first when an inherent of that name exists, so the other same-name trait methods — `save`, `merge`, `agent_id`, etc. — delegate to their inherents with no recursion and need no rename.) Verified: there are **zero other in-crate callers** of the inherent `set_embedding` (grep `set_embedding src/memory/` hits only `backend.rs`'s `&Memory`-based code and this site), so the rename is local.
2. **`get_associations_between`**: missing — add an inherent + trait method (SurrealQL: select `relates` edges whose both endpoints are in `$ids`).

---

### Task 1: Implement `MemoryBackend` for `SurrealMemoryStore`

**Files:**
- Modify: `src/memory/surreal_store.rs`
- Test: `src/memory/surreal_store.rs` (`#[cfg(test)]` using `kv-mem`, gated) — add a `get_associations_between` test; the trait impl is exercised by existing store tests.

**Interfaces:**
- Consumes: `MemoryBackend` (`src/memory/backend.rs`), `Memory`/`Association`/`MemoryType`, `SearchSort`.
- Produces: `impl MemoryBackend for SurrealMemoryStore`; inherent `get_associations_between(&self, ids: &[String]) -> Result<Vec<Association>>`; renamed inherent `set_embedding_by_id`.

- [ ] **Step 1: Add `get_associations_between` (failing test first)**

```rust
// in the gated #[cfg(test)] mod
#[tokio::test]
async fn associations_between_returns_only_internal_edges() {
    let store = mem_store().await; // existing kv-mem helper
    let a = mem("a"); let b = mem("b"); let c = mem("c");
    for m in [&a,&b,&c] { store.save(m, None).await.unwrap(); }
    store.create_association(&Association::new(&a.id,&b.id,RelationType::RelatedTo)).await.unwrap();
    store.create_association(&Association::new(&b.id,&c.id,RelationType::RelatedTo)).await.unwrap();
    let within = store.get_associations_between(&[a.id.clone(), b.id.clone()]).await.unwrap();
    assert_eq!(within.len(), 1); // a->b only; b->c excluded (c not in set)
}
```

- [ ] **Step 2: Run it, watch it fail** (bounded, kv-mem needs no ort):
`cd spikes/surreal-memory` is NOT this — run in-crate gated test: `systemd-run --scope -q -p MemoryMax=24G cargo test --features surreal-memory --lib memory::surreal_store::tests::associations_between_returns_only_internal_edges` (needs ort to link; if ort unavailable, compile-check with the ORT_LIB_LOCATION bypass and run the equivalent in `spikes/surreal-memory` reference instead). Expected: fails (method missing).

- [ ] **Step 3: Implement `get_associations_between`**

```rust
pub async fn get_associations_between(&self, ids: &[String]) -> Result<Vec<Association>> {
    if ids.is_empty() { return Ok(Vec::new()); }
    let recs: Vec<RecordId> = ids.iter().map(|s| RecordId::new("memory", s.clone())).collect();
    let mut r = self.db
        .query("SELECT meta::id(in) AS source, meta::id(out) AS target, relation_type, weight, \
                created_at FROM relates WHERE in IN $ids AND out IN $ids")
        .bind(("ids", recs)).await.map_err(err)?;
    let rows: Vec<AssocRow> = r.take(0).map_err(err)?;
    Ok(rows.into_iter().map(Association::from).collect())
}
```

- [ ] **Step 4: Rename `set_embedding` → `set_embedding_by_id`; add the trait impl**

Rename the inherent `pub async fn set_embedding(&self, id: &str, …)` to `set_embedding_by_id` and update any in-crate caller (grep `set_embedding` in `src/memory/`). Then add the impl block:

```rust
#[async_trait::async_trait]
impl crate::memory::backend::MemoryBackend for SurrealMemoryStore {
    fn agent_id(&self) -> &str { self.agent_id() }
    async fn save(&self, m: &Memory, e: Option<&[f32]>) -> Result<()> { self.save(m, e).await }
    async fn set_embedding(&self, m: &Memory, e: &[f32]) -> Result<()> { self.set_embedding_by_id(&m.id, e).await }
    async fn delete(&self, id: &str) -> Result<()> { self.delete(id).await }
    async fn load(&self, id: &str) -> Result<Option<Memory>> { self.load(id).await }
    async fn update(&self, m: &Memory) -> Result<()> { self.update(m).await }
    async fn forget(&self, id: &str) -> Result<bool> { self.forget(id).await }
    async fn record_access(&self, id: &str) -> Result<()> { self.record_access(id).await }
    async fn get_by_type(&self, t: MemoryType, l: i64) -> Result<Vec<Memory>> { self.get_by_type(t, l).await }
    async fn get_high_importance(&self, th: f32, l: i64) -> Result<Vec<Memory>> { self.get_high_importance(th, l).await }
    async fn get_sorted(&self, s: SearchSort, l: i64, t: Option<MemoryType>) -> Result<Vec<Memory>> { self.get_sorted(s, l, t).await }
    async fn create_association(&self, a: &Association) -> Result<()> { self.create_association(a).await }
    async fn get_associations(&self, id: &str) -> Result<Vec<Association>> { self.get_associations(id).await }
    async fn get_associations_between(&self, ids: &[String]) -> Result<Vec<Association>> { self.get_associations_between(ids).await }
    async fn delete_associations_for_memory(&self, id: &str) -> Result<u64> { self.delete_associations_for_memory(id).await }
    async fn get_neighbors(&self, id: &str, d: u32, ex: &[String]) -> Result<(Vec<Memory>, Vec<Association>)> { self.get_neighbors(id, d, ex).await }
    async fn vector_search(&self, q: &[f32], l: usize) -> Result<Vec<(String, f32)>> { self.vector_search(q, l).await }
    async fn text_search(&self, q: &str, l: usize) -> Result<Vec<(String, f32)>> { self.text_search(q, l).await }
    async fn find_similar(&self, id: &str, th: f32, l: usize) -> Result<Vec<(String, f32)>> { self.find_similar(id, th, l).await }
    async fn prune_below(&self, th: f32, older: chrono::DateTime<chrono::Utc>) -> Result<u64> { self.prune_below(th, older).await }
    async fn merge(&self, s: &str, l: &str, c: &str, e: Option<&[f32]>) -> Result<()> { self.merge(s, l, c, e).await }
}
```
(Inherent methods take priority in method-call syntax, so `self.agent_id()` / `self.save(...)` etc. resolve to the inherents — no recursion, no rename needed for those. Only `set_embedding` was renamed, because its signature differs.)

- [ ] **Step 5: Build-check (feature on) + run gated tests if ort available**

`mkdir -p /tmp/ortlib && systemd-run --scope -q -p MemoryMax=24G env ORT_LIB_LOCATION=/tmp/ortlib ORT_PREFER_DYNAMIC_LINK=1 cargo clippy --lib --tests --features surreal-memory -- -Dwarnings`
Expected: clean. If a real ONNX runtime is present, also `cargo test --features surreal-memory --lib memory::surreal_store` (bounded) — all green.

- [ ] **Step 6: Commit** — `git add src/memory/surreal_store.rs && git commit -m "feat(memory): impl MemoryBackend for SurrealMemoryStore + get_associations_between"`

---

### Task 2: Config backend selector (instance default + per-agent override)

**IMPORTANT — config architecture (verified):** `DefaultsConfig`/`AgentConfig`/`ResolvedAgentConfig` (`src/config/types.rs`) are NOT `Deserialize`. Deserialization happens on the `Toml*` structs in `src/config/toml_schema.rs` (which ARE `Deserialize`), then `src/config/load.rs` merges them by hand into the typed structs. `DefaultsConfig` has a **manual `Default`** (`types.rs:~1482`) and a **manual `Debug`** (`types.rs:~667`). Mirror the existing `worker_log_mode` field exactly — it is the template (`types.rs:662` field, `:696` Debug, `:1508` Default; `toml_schema.rs:311` `Option<String>`; `load.rs:1763` merge).

**Files:**
- Modify: `src/config/types.rs` (enum; `DefaultsConfig` field + its manual `Default` + manual `Debug`; `AgentConfig` Option-override field; `ResolvedAgentConfig` field; `AgentConfig::resolve`).
- Modify: `src/config/toml_schema.rs` (`TomlDefaultsConfig` + `TomlAgentConfig` `Option<String>` fields).
- Modify: `src/config/load.rs` (merge both, parsing the string → enum).
- Test: `src/config/types.rs` (resolve test).

**Interfaces:**
- Produces: `pub enum MemoryBackendKind { Sqlite, Surreal }` (`Default`=`Sqlite`, with `FromStr`/parse); `DefaultsConfig.memory_backend: MemoryBackendKind`; `AgentConfig.memory_backend: Option<MemoryBackendKind>`; `ResolvedAgentConfig.memory_backend: MemoryBackendKind`.

- [ ] **Step 1: Add the enum with a parse helper**

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MemoryBackendKind {
    #[default]
    Sqlite,
    Surreal,
}
impl MemoryBackendKind {
    pub fn parse_opt(s: Option<&str>) -> Option<Self> {
        match s?.trim().to_ascii_lowercase().as_str() {
            "sqlite" => Some(Self::Sqlite),
            "surreal" | "surrealdb" => Some(Self::Surreal),
            _ => None, // unknown → let caller fall back to default
        }
    }
}
```
(No serde derive — the typed structs aren't `Deserialize`; the `Toml*` structs hold `Option<String>` and `load.rs` parses via `parse_opt`.)

- [ ] **Step 2: Thread it through the typed structs (mirror `worker_log_mode`)**

- `DefaultsConfig`: add `pub memory_backend: MemoryBackendKind,`; add `memory_backend: MemoryBackendKind::default(),` to the manual `Default` impl; add `.field("memory_backend", &self.memory_backend)` to the manual `Debug` impl.
- `AgentConfig`: add `pub memory_backend: Option<MemoryBackendKind>,` (the override layer; default it to `None` wherever `AgentConfig` is built).
- `ResolvedAgentConfig`: add `pub memory_backend: MemoryBackendKind,`.
- `AgentConfig::resolve(&self, defaults: &DefaultsConfig, ...)`: set `memory_backend: self.memory_backend.unwrap_or(defaults.memory_backend),`.

- [ ] **Step 3: Toml schema + load merge**

- `toml_schema.rs`: `TomlDefaultsConfig` += `pub(super) memory_backend: Option<String>,`; `TomlAgentConfig` += `pub(super) memory_backend: Option<String>,`.
- `load.rs` (defaults merge, near `:1763`): `memory_backend: MemoryBackendKind::parse_opt(toml.defaults.memory_backend.as_deref()).unwrap_or(base_defaults.memory_backend),`.
- `load.rs` (per-agent `AgentConfig` build): `memory_backend: MemoryBackendKind::parse_opt(toml_agent.memory_backend.as_deref()),` (stays `Option`, resolved against defaults later).
(Match the exact field/merge style already used for `worker_log_mode` in each location.)

- [ ] **Step 4: Resolve test**

```rust
#[test]
fn agent_memory_backend_falls_back_to_defaults() {
    let mut defaults = DefaultsConfig::default();
    defaults.memory_backend = MemoryBackendKind::Surreal;
    let agent = AgentConfig { memory_backend: None, ..AgentConfig::minimal_for_test() };
    let resolved = agent.resolve(&defaults /*, … other args */);
    assert_eq!(resolved.memory_backend, MemoryBackendKind::Surreal); // inherited
    let agent2 = AgentConfig { memory_backend: Some(MemoryBackendKind::Sqlite), ..AgentConfig::minimal_for_test() };
    assert_eq!(agent2.resolve(&defaults /*, … */).memory_backend, MemoryBackendKind::Sqlite); // override wins
}
```
(Use whatever existing test constructor/`resolve` arity the file already has — adapt to the real `resolve` signature; do not invent `minimal_for_test` if a different pattern exists.)

- [ ] **Step 5: Run (bounded)** — `systemd-run --scope -q -p MemoryMax=24G cargo test -p spacebot --lib config 2>&1` (if it links ort, use the `ORT_LIB_LOCATION` bypass). Expected: pass.

- [ ] **Step 6: Commit** — `git commit -m "feat(config): memory_backend selector (defaults + per-agent override)"`

---

### Task 3: Wire the construction sites to select the backend

**Files:**
- Modify: `src/main.rs` (~2886-2906), `src/api/agents.rs` (~833-852).

**Interfaces:**
- Consumes: `agent_config.memory_backend` (`ResolvedAgentConfig`, Task 2), `agent_config.data_dir`, `agent_config.id`, `SurrealMemoryStore::open` (returns `Result<Arc<Self>>`), `SqliteBackend::new`, `crate::memory::lance::EMBEDDING_DIM` (made `pub` below).

- [ ] **Step 1: Make `EMBEDDING_DIM` shareable**

In `src/memory/lance.rs:12` change `const EMBEDDING_DIM: i32 = 384;` → `pub const EMBEDDING_DIM: i32 = 384;` (and re-export if convenient). It's `i32`, so callers cast `as usize`.

- [ ] **Step 2: Replace the backend construction at EACH site (site-specific SQLite branch)**

The two sites differ: `main.rs` builds the SQLite store with `MemoryStore::with_agent_id(db.sqlite.clone(), &agent_config.id)`; `api/agents.rs` uses `MemoryStore::new(db.sqlite.clone())`. **Preserve each site's existing SQLite constructor** — do NOT paste `with_agent_id` into `agents.rs`. Only the new Surreal branch + the selection wrapper are added. For `main.rs`:

```rust
let backend: Arc<dyn MemoryBackend> = {
    #[cfg(feature = "surreal-memory")]
    if matches!(agent_config.memory_backend, MemoryBackendKind::Surreal) {
        let dim = crate::memory::lance::EMBEDDING_DIM as usize;
        // open() already returns Arc<Self>; coerce to the trait object (NO extra Arc::new).
        let store = spacebot::memory::SurrealMemoryStore::open(
            &agent_config.data_dir, &agent_config.id, dim,
        ).await.map_err(|e| /* same error-context style as the SQLite path */ e)?;
        store as Arc<dyn MemoryBackend>
    } else {
        let memory_store = MemoryStore::with_agent_id(db.sqlite.clone(), &agent_config.id);
        let embedding_table = EmbeddingTable::open_or_create(&db.lance).await?;
        let _ = embedding_table.ensure_fts_index().await; // Plan A behaviour
        Arc::new(SqliteBackend::new(memory_store, embedding_table))
    }
    #[cfg(not(feature = "surreal-memory"))]
    {
        if matches!(agent_config.memory_backend, MemoryBackendKind::Surreal) {
            tracing::warn!(agent = %agent_config.id,
                "memory_backend=surreal but the `surreal-memory` feature is not compiled in; using SQLite");
        }
        let memory_store = MemoryStore::with_agent_id(db.sqlite.clone(), &agent_config.id);
        let embedding_table = EmbeddingTable::open_or_create(&db.lance).await?;
        let _ = embedding_table.ensure_fts_index().await;
        Arc::new(SqliteBackend::new(memory_store, embedding_table))
    }
};
let memory_search = Arc::new(MemorySearch::new(backend, embedding_model.clone()));
```
For `api/agents.rs`: identical structure, but the SQLite branch keeps `MemoryStore::new(db.sqlite.clone())` (its current call) and the site's existing error-handling/`?` style. (`Arc<SurrealMemoryStore>` → `Arc<dyn MemoryBackend>` is a valid unsizing coercion once Task 1's impl exists.)

- [ ] **Step 3: Default-build compile-check (feature OFF — must be unchanged behaviour)**

`systemd-run --scope -q -p MemoryMax=24G cargo check -p spacebot --bin spacebot`. Expected: clean; the `#[cfg(not(...))]` arm behaves exactly as today (SQLite), only adding a warn when misconfigured.

- [ ] **Step 3: Feature-on compile-check**

`mkdir -p /tmp/ortlib && systemd-run --scope -q -p MemoryMax=24G env ORT_LIB_LOCATION=/tmp/ortlib ORT_PREFER_DYNAMIC_LINK=1 cargo clippy --features surreal-memory --bin spacebot -- -Dwarnings`. Expected: clean.

- [ ] **Step 4: Commit** — `git commit -m "feat(memory): select SQLite/SurrealDB backend at construction by config"`

---

### Task 4: Delete redundant surreal search/maintenance; migrate the surreal test

**Files:**
- Delete: `src/memory/surreal_search.rs`, `src/memory/surreal_maintenance.rs`
- Modify: `src/memory.rs` (drop their `pub mod` + `pub use SurrealMemorySearch`), `tests/surreal_memory.rs`.

**Interfaces:**
- Consumes: generic `MemorySearch` (Plan A), `maintenance::run_maintenance` (Plan A), `SurrealMemoryStore` as `Arc<dyn MemoryBackend>`.

- [ ] **Step 1: Confirm nothing references the deleted items**

`grep -rnE "SurrealMemorySearch|surreal_search|surreal_maintenance" src/ tests/` — expected hits ONLY: `memory.rs` module decls/re-export (verified at `memory.rs:9-10` surreal_maintenance mod, `:13-14` surreal_search mod, `:25-26` `pub use surreal_search::SurrealMemorySearch`) and `tests/surreal_memory.rs:6,17,230`. `surreal_migrate` (`:11-12`) and `surreal_store` (`:15-16`, `:27-28`) STAY. If anything else in `src/` uses them, STOP and report.

- [ ] **Step 2: Delete the files and their module wiring**

`git rm src/memory/surreal_search.rs src/memory/surreal_maintenance.rs`; in `src/memory.rs` remove the exact lines: the `surreal_maintenance` `#[cfg]`+`pub mod` (9-10), the `surreal_search` `#[cfg]`+`pub mod` (13-14), and the `#[cfg]`+`pub use surreal_search::SurrealMemorySearch` (25-26). Leave `surreal_migrate` and `surreal_store` wiring intact.

- [ ] **Step 3: Rework `tests/surreal_memory.rs` (do NOT route hybrid through `MemorySearch`)**

CONSTRAINT (verified): `tests/surreal_memory.rs` uses `DIM=4` and passes hand-built 4-element vectors. The deleted `SurrealMemorySearch::search` took an explicit `query_embedding`. The generic `MemorySearch::search(query, config)` takes NO embedding — it computes one internally via `self.embedding_model.embed_one(query)` (`search.rs:86-90`), which needs a real ONNX model AND emits 384-dim vectors — **incompatible** with the test's dim-4 SurrealKV schema (`vector_search` rejects dim mismatch, `surreal_store.rs:603`). So the old hybrid test CANNOT be ported to `MemorySearch` with hand vectors.

Do this instead:
- **Delete** the `SurrealMemorySearch`-based hybrid test (the `~line 230` block). The hybrid RRF/traversal *logic* is now the single shared `MemorySearch`/`search.rs` implementation, already covered by the SQLite-backed tests in `src/memory/search.rs` — re-testing it over Surreal would only duplicate that logic coverage.
- **Keep / add** store-primitive tests on `SurrealMemoryStore` directly (these take explicit dim-4 vectors, need no `EmbeddingModel`): `vector_search`, `text_search`, `find_similar`, `get_neighbors`, plus the Task 1 `get_associations_between`. These validate exactly the Surreal-specific behaviour the shared search logic depends on.
- If any test referenced `surreal_maintenance`, replace with `maintenance::run_maintenance(backend.clone(), embedding_model, &config)` ONLY in an ort-capable, dim-384 setup; otherwise drop it and note that full-pipeline Surreal validation is deferred to an ONNX-capable environment (see Task 5 Step 3).
- Remove the now-unused `use` of `SurrealMemorySearch` and any `EmbeddingModel` import that's no longer needed.

- [ ] **Step 4: Feature-on compile-check + run if ort available**

`mkdir -p /tmp/ortlib && systemd-run --scope -q -p MemoryMax=24G env ORT_LIB_LOCATION=/tmp/ortlib ORT_PREFER_DYNAMIC_LINK=1 cargo clippy --features surreal-memory --lib --tests -- -Dwarnings`. Expected: clean (no references to deleted modules). If ONNX present: `cargo test --features surreal-memory --test surreal_memory` (bounded) green.

- [ ] **Step 5: Commit** — `git commit -m "refactor(memory): drop duplicated surreal search/maintenance (covered by generic MemorySearch/maintenance)"`

---

### Task 5: Gates + dual-build verification

- [ ] **Step 1: Default build gates (feature OFF)** — bounded `just gate-pr` equivalent: `systemd-run --scope -q -p MemoryMax=24G ./scripts/gate-pr.sh`. Expected: all green (fmt, check --all-targets, clippy -Dwarnings, test --lib, test --no-run).
- [ ] **Step 2: Feature-on clippy (bounded, ort bypass)** — `mkdir -p /tmp/ortlib && systemd-run --scope -q -p MemoryMax=24G env ORT_LIB_LOCATION=/tmp/ortlib ORT_PREFER_DYNAMIC_LINK=1 cargo clippy --features surreal-memory --all-targets -- -Dwarnings`. Expected: clean.
- [ ] **Step 3: If a real ONNX runtime is available**, run the feature-on memory + surreal tests bounded and (optionally) the 384-dim recall sanity from Plan C's scope; otherwise record that runtime validation of the live Surreal path is deferred to an ort-capable environment.
- [ ] **Step 4: Commit any fmt fixes; update the branch handoff doc** noting decisions A (per-agent) settled, B (cross-store best-effort) documented, C (SurrealKV backup) still open.

---

## Self-Review (updated after Opus adversarial review of this plan)

- **Spec coverage:** trait impl + gaps (Task 1), config selector (Task 2), construction wiring (Task 3), delete duplicates + rework test (Task 4), gates (Task 5). Followup #3 (duplication) is closed by Task 4.
- **Default build untouched:** Tasks 3/5 explicitly compile-check the feature-off path; all Surreal code is `#[cfg]`-gated; the feature-off arm only adds a warn on misconfiguration.

**Corrections applied from the Opus review (each verified against source):**
- 🔴 Config: `DefaultsConfig`/`AgentConfig`/`ResolvedAgentConfig` are NOT `Deserialize`. Task 2 rewritten to mirror `worker_log_mode`: enum has no serde derive; `Toml*` structs carry `Option<String>`; `load.rs` parses+merges; `DefaultsConfig` gets manual `Default`+`Debug` entries. Per-agent override threaded through `ResolvedAgentConfig`.
- 🔴 `resolved_memory_backend` was undefined → now `agent_config.memory_backend` (a real `ResolvedAgentConfig` field), in scope at both sites.
- 🔴 `EMBEDDING_DIM` is private `i32` → Task 3 makes it `pub` (cast `as usize`).
- 🔴 Double-`Arc`: `open()` returns `Result<Arc<Self>>` → use `store as Arc<dyn MemoryBackend>`, no outer `Arc::new`.
- 🔴 Construction sites differ (`with_agent_id` in main.rs vs `new` in agents.rs) → Task 3 keeps each site's SQLite constructor; only adds the Surreal branch.
- 🔴 Test migration infeasible (dim-4 hand vectors vs `MemorySearch`'s internal 384-dim embed) → Task 4 drops the hybrid-via-MemorySearch test (logic already covered by SQLite search tests) and keeps Surreal store-primitive tests instead.
- 🟠 `set_embedding` rename rationale corrected (signatures differ, not recursion); inherent methods take call priority so `save`/`merge`/`agent_id` don't recurse.

**Remaining known risks (acceptable / out of scope):** the surreal store still uses a hand-rolled BFS in `get_neighbors` + keyword-seed graph search (Plan C replaces it); running the feature for real needs ONNX (compile-checks + dim-4 store tests are the gate in this environment; live 384-dim validation deferred to an ort-capable env).

## Out of scope (Plan C)
Native graph recursion (`{..N+collect}`/`{..N+shortest}`) replacing the hand-rolled BFS, `snowball(english)` FTS analyzer, EF tuning, 384-dim recall/latency benchmark.
