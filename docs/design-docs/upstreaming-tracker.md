# Upstreaming tracker — PRs split out of `feat/surrealdb-memory`

The SurrealDB-memory work is being split into a **stack of focused PRs** for upstream
(`spacedriveapp/spacebot`), smallest/safest first. The full work stays integrated on
`feat/surrealdb-memory` (the fork mainline). Docs are **fork-only** (never upstream);
the PR branches are **code-only**.

## The stack

```
main ──▶ PR A (fixes) ──▶ PR B (abstraction) ──▶ PR C (SurrealDB backend)
```

Each PR depends on the one before it. A is independent; B uses nothing of C; C uses the trait B introduces.

## Status

| PR | What | Branch | Base / target | Link | Status |
|----|------|--------|---------------|------|--------|
| **A** | Ingestion reliability + merge-bloat fixes (deterministic chunk completion, retry budget+quarantine, ingest delete cleanup, canonical merge) | `pr/ingestion-fixes` | upstream `main` | **spacedriveapp/spacebot#604** | **OPEN** — CI green; bot reviews (CodeRabbit/tembo) addressed (delete-order, merge size-cap, ingest path-guard, rustfmt). Awaiting human review. |
| **B** | `MemoryBackend` trait — pluggable / storage-agnostic memory layer | `pr/memory-abstraction` | now: fork `pr/ingestion-fixes`; later: upstream `main` | **slvnlrt/spacebot#1** (draft) | **DRAFT in fork**, stacked on A. CodeRabbit reviews addressed (agent_id, deterministic edge ordering, IN-list chunking, FTS retry) folded into the single commit; 1 "Major" (cross-store `save` atomicity) declined with rationale + doc note. |
| **C** | SurrealDB unified memory backend (structured+vector+graph in one embedded store) | *(future)* `pr/surreal-backend` off B | TBD (fork draft first) | — | **NOT STARTED** — deferred until B lands / upstream interest |

## Per-PR detail

**PR A** — 4 fix commits + `style: apply rustfmt`. Cross-backend bugs in upstream's own code. Code-only. Verified: `cargo test --lib` green (881), fmt-clean. No internal jargon, no surreal, no commit trailers.

**PR B** — 1 abstraction commit, 15 `src/*.rs` files. Behaviour-preserving: no schema change, no new dependency, default build unchanged. Verified: `cargo test --lib` green (889), fmt-clean, **zero surreal in code**. Opened as a **draft in the fork with base = `pr/ingestion-fixes`** so the diff is *only* the abstraction (1 commit) instead of also showing #604's commits. Description states the value (pluggable/agnostic memory layer) and notes a SurrealDB backend is maintained in the fork as a possible future dedicated PR. **When A merges:** rebase B onto `main`, retarget to upstream, mark ready → upstream diff becomes the single abstraction commit.

**PR C** — the surreal-specific delta only: `surreal_store.rs`, `surreal_migrate.rs`, the `surreal-memory` feature + optional `surrealdb` dep, the `#[cfg(feature="surreal-memory")]` arms, the `MemoryBackendKind` config selector, the `migrate-memory` CLI (and optionally the `spikes/surreal-memory` reference). Extractable off `pr/memory-abstraction` when needed. Heaviest + least likely to be accepted upstream (new backend + heavy dep) → deliberately deferred.

## Canonical fork branch

`feat/surrealdb-memory` — integration / deployment line. Carries everything (abstraction + SurrealDB backend + fixes) **plus all design docs** (this tracker, the consolidation cartography + kickoff, gap-analysis, big-players comparison, emerging-research notes, the ingestion bug report, handoff, followups…). These docs never go upstream.

**Review-fix back-port.** The fixes raised on #604 and #1 by the bot reviews are real cross-backend improvements (delete-order data integrity, merge size-cap, ingest path-guard, agent_id, deterministic edge ordering, IN-list chunking, FTS retry on `set_embedding`). They are propagated back onto `feat` so the deployed branch is not left running the bugs the reviews caught. (Until #604/#1 land upstream and `feat` is rebased onto the merged stack, this back-port is manual.)

## Conventions for the upstream PR branches

- Branch off `main` (= `origin/main` = `upstream/main`).
- **Code-only** — no `docs/`, no `.superpowers/`, no `spikes/`.
- No internal jargon (bug-number tags, "Lot", gap-analysis references); **no surreal mentions in code**.
- No commit trailers (no `Co-Authored-By`, no session link).
- Must pass what CI enforces: `cargo fmt --all -- --check`, `cargo test --lib`, and the feature-on `clippy` job.

## Next steps

1. **A (#604)** — await upstream review/merge.
2. **On A merge** — update fork `main`; rebase B onto `main`; retarget B to upstream; mark B ready.
3. **On B land / interest** — extract C off B.
4. **Long-term** — once the stack lands upstream, rebase `feat/surrealdb-memory` onto the new `main` so it carries only the surreal delta (de-duplicates history).
