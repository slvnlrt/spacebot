# Plan E — Finalization: migration CLI (#15) + construction dedup (#14)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development. Steps use checkbox (`- [ ]`).

**Goal:** Wire the existing `surreal_migrate::migrate_from_sqlite` into a CLI subcommand so an operator can migrate an agent's SQLite+Lance memory into SurrealDB (#15), and de-duplicate the 4× SQLite construction branch in the two construction sites (#14). These are the last two backlog items before the branch is fully finalized.

**Architecture:** The migration LOGIC already exists and is complete (`src/memory/surreal_migrate.rs::migrate_from_sqlite(source: &MemoryStore, embedding_model: &Arc<EmbeddingModel>, target: &SurrealMemoryStore) -> MigrationReport`, idempotent, per-`MemoryType`, regenerates embeddings). It just needs an invocation surface: a feature-gated `spacebot migrate-memory [--agent <id>]` CLI subcommand that builds the SQLite source + SurrealKV target per agent (daemon stopped) and runs it. Separately, the SQLite memory-backend construction (`MemoryStore` + `EmbeddingTable` + `ensure_fts_index` + `SqliteBackend::new`) is written 4× across the `#[cfg]` split in `main.rs`/`agents.rs`; extract a helper.

**Tech Stack:** Rust 2024, clap (existing CLI), tokio. The migration subcommand is `#[cfg(feature = "surreal-memory")]`-gated.

## Global Constraints

- Follow `RUST_STYLE_GUIDE.md`. All SurrealDB/migration code stays `#[cfg(feature="surreal-memory")]`-gated; the default (feature-off) build must stay green and behaviourally unchanged.
- **RAM/disk safety:** wrap cargo in `systemd-run --scope -q -p MemoryMax=40G -p MemorySwapMax=0 …`; `CARGO_BUILD_JOBS=8`; keep `target/` warm; **clean between feature-off and feature-on heavy passes** (the two variants coexist ~80-90 GB). Don't loop builds; stop after two same-cause failures.
- Real ort is available — RUN tests for real; use the ORT_LIB_LOCATION bypass only for feature-on clippy compile-check.
- **Migration safety contract:** the subcommand must run with the **daemon stopped** (it opens the per-agent stores directly; concurrent daemon writes would race). Guard: if the daemon is reachable, refuse with a clear message.
- The migration is **idempotent** (same-UUID upsert + UNIQUE edge index) — safe to re-run.

## File Structure

- **Modify** `src/main.rs`: add a `#[cfg(feature="surreal-memory")]` `Command::MigrateMemory { agent: Option<String> }` variant + match arm → `cmd_migrate_memory(cli.config, agent)`; add the gated `async fn cmd_migrate_memory`.
- **Modify** `src/main.rs` + `src/api/agents.rs`: extract the SQLite-backend construction into a shared helper (#14) used by all four `#[cfg]` arms.
- **Possibly add** `src/memory/backend.rs` (or a small `construction` helper): `pub fn build_sqlite_backend(sqlite: SqlitePool, agent_id: &str, lance: &lancedb::Connection) -> impl Future<...>` — but see Task 2 for the exact shape (the two sites differ in `MemoryStore::new` vs `with_agent_id`).

---

### Task 1: `migrate-memory` CLI subcommand (#15)

**Files:** `src/main.rs` (gated variant + handler). No new tests beyond a smoke check (the migration logic is already covered by `surreal_migrate`'s own context; an end-to-end CLI test needs a full instance + ort and is impractical — compile-check + a manual run-path is the gate).

**Interfaces:**
- Consumes: `Config::load`/`load_from_path` (existing CLI config helper, `main.rs:~1321`), `MemoryStore::with_agent_id`, `EmbeddingModel::new`, `SurrealMemoryStore::open`, `surreal_migrate::migrate_from_sqlite`, `crate::memory::lance::EMBEDDING_DIM`, the daemon status probe (`spacebot::daemon::send_command(.., Status)` as used by `cmd_status`).
- Produces: `Command::MigrateMemory { agent: Option<String> }` (gated) and `cmd_migrate_memory`.

- [ ] **Step 1: Add the gated subcommand variant**

In the `Command` enum (`main.rs:~29`):
```rust
/// Migrate an agent's SQLite+LanceDB memory into the embedded SurrealDB store.
/// Run with the daemon STOPPED. Requires the `surreal-memory` feature.
#[cfg(feature = "surreal-memory")]
MigrateMemory {
    /// Agent id to migrate (default: all configured agents).
    #[arg(long)]
    agent: Option<String>,
},
```
And the match arm (`main.rs:~372`):
```rust
#[cfg(feature = "surreal-memory")]
Command::MigrateMemory { agent } => cmd_migrate_memory(cli.config, agent),
```

- [ ] **Step 2: Implement `cmd_migrate_memory`** (gated)

```rust
#[cfg(feature = "surreal-memory")]
fn cmd_migrate_memory(config_path: Option<std::path::PathBuf>, agent: Option<String>) -> anyhow::Result<()> {
    let config = load_config(&config_path)?;            // existing helper, main.rs:1317

    // 1. Daemon-running guard — MUST use the loaded config's instance_dir, NOT
    //    from_default() (which ignores --config and would probe the wrong instance):
    let paths = spacebot::daemon::DaemonPaths::new(&config.instance_dir);
    if spacebot::daemon::is_running(&paths).is_some() {
        anyhow::bail!("the spacebot daemon is running — stop it before migrating memory \
                       (concurrent writes would corrupt the migration)");
    }

    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    runtime.block_on(async move {
        // 2. Select agents from RESOLVED configs (config.agents are unresolved AgentConfig
        //    with no data_dir; resolve_agents() yields ResolvedAgentConfig.id/.data_dir —
        //    the SAME data_dir the daemon uses, so no divergence).
        let resolved = config.resolve_agents();
        let targets: Vec<_> = match &agent {
            Some(id) => resolved.into_iter().filter(|a| &a.id == id).collect(),
            None => resolved,
        };
        if let Some(id) = &agent { if targets.is_empty() { anyhow::bail!("no agent '{id}' in config"); } }

        // 3. Shared EmbeddingModel (Arc — migrate_from_sqlite wants &Arc<EmbeddingModel>):
        let embedding_model = std::sync::Arc::new(
            spacebot::memory::EmbeddingModel::new(&config.instance_dir.join("embedding_cache"))?,
        );

        // 4. Per agent:
        for a in &targets {
            let db = spacebot::db::Db::connect(&a.data_dir).await?;            // .sqlite source (+ migrations)
            let source = spacebot::memory::MemoryStore::with_agent_id(db.sqlite.clone(), &a.id);
            let target = spacebot::memory::SurrealMemoryStore::open(
                &a.data_dir, &a.id, spacebot::memory::lance::EMBEDDING_DIM as usize,
            ).await?;
            let report = spacebot::memory::surreal_migrate::migrate_from_sqlite(
                &source, &embedding_model, &target,
            ).await?;
            println!("agent {}: migrated {} memories, {} associations", a.id, report.memories, report.associations);
        }
        anyhow::Ok(())
    })
}
```
(`load_config`/`new_current_thread` runtime mirror `cmd_status`/`cmd_secrets`. `Db::connect` runs SQLite migrations + opens lance/redb — a benign one-shot write to the source. `surreal_migrate` must be `pub` in `memory.rs` — confirm/expose it gated.)

- [ ] **Step 3: Default-build safety + feature-on compile-check**

- Feature OFF: `systemd-run … cargo check -p spacebot --bin spacebot` — clean; the subcommand simply doesn't exist (gated out).
- Feature ON: `mkdir -p /tmp/ortlib && systemd-run … env ORT_LIB_LOCATION=/tmp/ortlib ORT_PREFER_DYNAMIC_LINK=1 cargo clippy --features surreal-memory --bin spacebot -- -Dwarnings` — clean.

- [ ] **Step 4: Manual run-path sanity (feature-on, real ort)** — build `cargo build --features surreal-memory --bin spacebot` (bounded) and run `./target/debug/spacebot migrate-memory --help` to confirm the subcommand parses + the daemon-guard path is reachable. (A full migration needs a real `~/.spacebot` instance — out of scope for CI; document the manual test.)

- [ ] **Step 5: Commit** — `feat(memory): migrate-memory CLI subcommand (SQLite+Lance → SurrealDB)`

---

### Task 2: De-duplicate the SQLite construction branch (#14)

**Files:** `src/main.rs` (~2886-2920), `src/api/agents.rs` (~833-860).

**Interfaces:** Produces a helper that builds the SQLite `Arc<dyn MemoryBackend>` (store + embedding table + FTS index), used by all four `#[cfg]` arms.

- [ ] **Step 1: Extract the helper**

The repeated block is: build `MemoryStore` (NOTE: `main.rs` uses `with_agent_id`, `agents.rs` uses `new`), `EmbeddingTable::open_or_create(&db.lance)`, `ensure_fts_index().await` (warn on err), `Arc::new(SqliteBackend::new(store, embedding_table))`. Extract (helper returns `crate::error::Result`, error = `crate::error::Error`):
```rust
// in backend.rs (or a memory:: helper), NOT gated:
pub async fn sqlite_backend_arc(
    store: Arc<crate::memory::store::MemoryStore>,
    lance: &lancedb::Connection,
    agent_id: &str,                       // for error context (#4)
) -> Result<Arc<dyn MemoryBackend>> {
    let embeddings = crate::memory::lance::EmbeddingTable::open_or_create(lance)
        .await
        .map_err(|e| crate::error::DbError::LanceConnect(format!("agent '{agent_id}': {e}")))?;
    if let Err(error) = embeddings.ensure_fts_index().await {
        tracing::warn!(%error, agent = %agent_id, "failed to ensure FTS index");
    }
    Ok(Arc::new(SqliteBackend::new(store, embeddings)))
}
```
The caller passes the already-built `MemoryStore` (each site keeps its own `with_agent_id` vs `new`). The 4 arms collapse to: build `store` (site-specific), then call the helper.

- [ ] **Step 2: Apply at both sites — mind the error-type mismatch (🔴)**

- **`main.rs`** (`run` returns `anyhow::Result`; `crate::error::Error: std::error::Error`, so `?` converts):
  `let backend = sqlite_backend_arc(store, &db.lance, &agent_config.id).await?;`
- **`api/agents.rs`** (`create_agent_internal` returns `Result<_, String>` — there is **NO** `From<crate::error::Error> for String`, so a bare `?` will NOT compile). Use `.map_err`:
  `let backend = sqlite_backend_arc(store, &db.lance, &agent_id).await.map_err(|e| format!("failed to init memory backend: {e}"))?;`

Keep each site's site-specific `MemoryStore` constructor; the Surreal arm is unchanged. Both `#[cfg]` arms (feature-on `else` + feature-off) use the helper.

- [ ] **Step 3: Dual-build check**

Feature OFF: `cargo check -p spacebot --bin spacebot` clean (behaviour unchanged — same calls, factored). Feature ON: clippy `--features surreal-memory --bin spacebot` clean.

- [ ] **Step 4: Commit** — `refactor(memory): extract sqlite_backend_arc helper (dedup cfg-split construction, #14)`

---

### Task 3: Gates + docs

- [ ] **Step 1: Dual-build gates** — feature-off `systemd-run … ./scripts/gate-pr.sh` ALL GREEN; **clean target**; feature-on `clippy --all-targets --features surreal-memory` (ORT bypass) clean; feature-on tests RUN real (`--lib memory` + `--test surreal_memory`) green.
- [ ] **Step 2: Docs** — `followups.md`: #15 RESOLVED (CLI wired), #14 RESOLVED (helper); #12 (migrate not transactional) note it's now reachable via the CLI but still idempotent/acceptable. `handoff.md`: add the `migrate-memory` command to the build/test section + remove #14/#15 from "what's NOT done". Commit.

---

## Self-Review (updated after Opus review of this plan)

- **Spec coverage:** migration CLI (#15, Task 1), construction dedup (#14, Task 2), gates+docs (Task 3).
- **Default-build safety:** the migration subcommand is fully `#[cfg]`-gated (absent feature-off); the dedup helper is non-gated, a pure factoring of existing calls — gates verify both configs.

**Corrections applied from the Opus review (verified against source):**
- 🔴 **Error type at the `agents.rs` site:** `create_agent_internal` returns `Result<_, String>` with no `From<crate::error::Error> for String`, so a bare `?` on the helper won't compile there → use `.map_err(|e| format!(...))?` (main.rs returns `anyhow::Result`, `?` is fine).
- 🟠 **Daemon guard:** use `DaemonPaths::new(&config.instance_dir)` + `daemon::is_running` — NOT `from_default()` (which `cmd_status` uses but ignores `--config`, probing the wrong instance).
- 🟡 **Agent enumeration:** iterate `config.resolve_agents()` (`ResolvedAgentConfig` has `.id`/`.data_dir`) — `config.agents` are unresolved with no `data_dir`. This also guarantees the CLI targets the SAME `data_dir` the daemon uses (no divergence).
- 🟡 **`EmbeddingModel` must be `Arc`-wrapped** (`migrate_from_sqlite` wants `&Arc<EmbeddingModel>`).
- Verified sound by the review: clap `#[cfg]` variant+arm gating, the sync-fn-builds-runtime handler pattern, `Db::connect` (.sqlite + runs migrations — benign one-shot), `send_command`/`is_running` APIs, helper types/lifetimes.

- **Risks remaining:** the daemon guard is a correctness requirement (concurrent writes) — must not be skipped; `cmd_migrate_memory` builds an `EmbeddingModel` (ONNX, heavy) — necessary (migration regenerates embeddings).
- **Out of scope:** #9 (slim build, deferred); a live-instance end-to-end migration test (needs a real `~/.spacebot` + ort) — Task 1 Step 4 is a `--help`/parse smoke check only.
