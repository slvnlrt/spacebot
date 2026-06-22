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
    // Build a tokio runtime like the other async CLI handlers do, then:
    // 1. Refuse if the daemon is running (probe via daemon::send_command Status);
    //    print "stop the daemon first" and exit non-zero.
    // 2. Load Config (load_config helper). Select agents: all, or the one matching `agent`
    //    (error if `agent` given but not found).
    // 3. Build a shared EmbeddingModel (EmbeddingModel::new(<embedding cache dir>) — mirror
    //    how main.rs builds it at startup, ~line 1736).
    // 4. For each selected agent (sequentially):
    //      - source: let store = MemoryStore::with_agent_id(<sqlite pool for agent.data_dir>, &agent.id);
    //        (open the per-agent SQLite via spacebot::db::Db::connect(&agent.data_dir) and use .sqlite —
    //        or a lighter SqlitePool open; reuse Db::connect for correctness + migrations.)
    //      - target: let target = SurrealMemoryStore::open(&agent.data_dir, &agent.id, EMBEDDING_DIM as usize).await?;
    //      - let report = surreal_migrate::migrate_from_sqlite(&store, &embedding_model, &target).await?;
    //      - print: "agent <id>: migrated <report.memories> memories, <report.associations> associations".
    // 5. Print a final summary; return Ok.
}
```
Match the exact async-runtime + config-loading + error-context style of the existing handlers (`cmd_secrets`/`cmd_auth`). Use `Db::connect(&agent.data_dir)` to open the source (it runs migrations + gives `.sqlite`). The daemon-running guard is mandatory.

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

The repeated block is: build `MemoryStore` (NOTE: `main.rs` uses `with_agent_id`, `agents.rs` uses `new`), `EmbeddingTable::open_or_create(&db.lance)`, `ensure_fts_index().await` (warn on err), `Arc::new(SqliteBackend::new(store, embedding_table))`. Extract:
```rust
// in backend.rs (or a memory:: helper), NOT gated:
pub async fn sqlite_backend_arc(
    store: Arc<crate::memory::store::MemoryStore>,
    lance: &lancedb::Connection,
) -> Result<Arc<dyn MemoryBackend>> {
    let embeddings = crate::memory::lance::EmbeddingTable::open_or_create(lance).await?;
    if let Err(error) = embeddings.ensure_fts_index().await {
        tracing::warn!(%error, "failed to ensure FTS index");
    }
    Ok(Arc::new(SqliteBackend::new(store, embeddings)))
}
```
The caller passes the already-constructed `MemoryStore` (so each site keeps its own `with_agent_id` vs `new` — the difference stays at the call site, only the embedding-table+fts+wrap is shared). This collapses the 4 arms to: build `store` (site-specific), then `sqlite_backend_arc(store, &db.lance).await?`.

- [ ] **Step 2: Apply at both sites, both cfg arms**

In `main.rs` and `agents.rs`, replace each SQLite arm's `EmbeddingTable::open_or_create` + `ensure_fts_index` + `Arc::new(SqliteBackend::new(...))` with `sqlite_backend_arc(store, &db.lance).await?`, keeping the site-specific `MemoryStore` constructor (`with_agent_id` in main.rs, `new` in agents.rs). The Surreal arm is unchanged.

- [ ] **Step 3: Dual-build check**

Feature OFF: `cargo check -p spacebot --bin spacebot` clean (behaviour unchanged — same calls, factored). Feature ON: clippy `--features surreal-memory --bin spacebot` clean.

- [ ] **Step 4: Commit** — `refactor(memory): extract sqlite_backend_arc helper (dedup cfg-split construction, #14)`

---

### Task 3: Gates + docs

- [ ] **Step 1: Dual-build gates** — feature-off `systemd-run … ./scripts/gate-pr.sh` ALL GREEN; **clean target**; feature-on `clippy --all-targets --features surreal-memory` (ORT bypass) clean; feature-on tests RUN real (`--lib memory` + `--test surreal_memory`) green.
- [ ] **Step 2: Docs** — `followups.md`: #15 RESOLVED (CLI wired), #14 RESOLVED (helper); #12 (migrate not transactional) note it's now reachable via the CLI but still idempotent/acceptable. `handoff.md`: add the `migrate-memory` command to the build/test section + remove #14/#15 from "what's NOT done". Commit.

---

## Self-Review

- **Spec coverage:** migration CLI (#15, Task 1), construction dedup (#14, Task 2), gates+docs (Task 3).
- **Default-build safety:** the migration subcommand is fully `#[cfg]`-gated (absent feature-off); the dedup helper is non-gated but is a pure factoring of existing calls (no behaviour change) — gates verify both configs.
- **Risks:** (a) feature-gating a clap `Command` enum variant + match arm must be consistent (both gated) or the match is non-exhaustive feature-off — Task 1 gates both; (b) the daemon-running guard is a correctness requirement (concurrent writes) — must not be skipped; (c) `cmd_migrate_memory` builds an `EmbeddingModel` (ONNX) — heavy but necessary (migration regenerates embeddings); (d) the dedup helper must preserve each site's distinct `MemoryStore` constructor — do NOT unify `with_agent_id`/`new`.
- **Out of scope:** #9 (slim build, deferred), the live-instance end-to-end migration test (needs a real `~/.spacebot`).
