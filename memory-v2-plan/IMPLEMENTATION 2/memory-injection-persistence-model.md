# Memory Injection Persistence Model

Architecture Decision Record for the memory injection persistence strategy. Supersedes the preliminary version of this document.

**Decision date:** 2026-02-23
**Status:** DECIDED — Bounded persistence + compactor-aware filtering.

---

## 1. The Problem

Memory injection (`compute_memory_injection`) retrieves relevant memories and injects them into the LLM context as a `[Context from memory]` block before each turn. The question: **what happens to that block after the LLM responds?**

The code comment says *"not persisted to history"* but the runtime behavior is different. On successful turns, `apply_history_after_turn` does `*guard = history`, which writes back the full augmented history — including the injected block.

This mismatch between documented intent and actual behavior needed an explicit architectural decision.

## 2. Current State (Pre-Decision)

### What the code does

In `run_agent_turn` (`src/agent/channel.rs`):
1. Channel history is cloned from shared state
2. `[Context from memory]: ...` is pushed into the clone
3. Rig's agentic loop runs with `agent.prompt(user_text).with_history(&mut history)`
4. On success, `apply_history_after_turn` replaces shared history: `*guard = history`

**Result:** the injected block persists in RAM history indefinitely. It is:
- Visible to the LLM on all subsequent turns
- Included when `render_messages_as_transcript` builds compaction input
- Counted in `estimate_history_tokens` for compaction threshold checks
- Cloned into branches when `spawn_branch` does `h.clone()`
- **Not** written to SQLite by `ConversationLogger` (which only logs real user/assistant messages)

### Existing controls

- `context_window_depth`: cooldown turns before a memory can be re-injected (via `injected_ids` map)
- Semantic dedup buffer with turn tracking (`VecDeque<(Vec<f32>, usize)>`)
- `max_total`: hard cap on memories per injection pass
- `contextual_min_score`: minimum hybrid score for contextual candidates
- `ambient_enabled`: runtime master switch for pinned type retrieval

### Why this matters

| Impact area | Effect of uncapped persistence |
|---|---|
| **Token budget** | Each turn adds ~1-3k tokens of injection. 10 turns = 10-30k tokens of stale blocks |
| **Compaction quality** | Compactor summarizes injection blocks as if they were dialogue. Polluted summaries |
| **Reinforcement loop** | Compaction summary encodes injected facts → retrieval finds them again → re-injected |
| **Fossilization** | Memory updated in DB, but stale version persists in old injection blocks |

## 3. Options Analyzed

### Option A — Ephemeral (remove after each turn)

Strip injected blocks from history before write-back. Each turn gets a fresh injection.

**Rejected.** Three critical failure modes in real conversations:

**3a. Implicit follow-ups lack semantic signal for retrieval.**
Natural conversation uses pronouns, references, and ellipsis:
```
Turn 1: "What do we know about the auth system?"  → RAG finds auth memories
Turn 2: "Why that instead of sessions?"            → "that" = nothing for hybrid search
```
"that" doesn't contain "JWT" or "auth". The hybrid search on the raw query cannot bridge the implicit reference. The LLM loses context on the exact topic it was just discussing.

**3b. Not all injected memories surface in the LLM's response.**
If RAG injects 10 memories and the LLM explicitly mentions 3, the other 7 provided tacit background context (informing comprehension, tone, nuance). In ephemeral mode, those 7 vanish — even though they were actively shaping the LLM's understanding. They're the notes on the desk you don't quote but still need.

**3c. Fossilization argument is symmetric.**
"Stale injection blocks contain outdated info" is true. But so do the LLM's own responses — if the LLM said "we use JWT" at turn 5 and the decision changes at turn 20, the turn-5 response is equally stale. This argument doesn't favor ephemeral over persistent.

### Option B — Unbounded persistent (current behavior)

Keep all injected blocks in history forever.

**Rejected.** Accumulation, compaction pollution, and reinforcement loops as described in section 2. A 15-turn conversation can contain 15 injection blocks, creating a parallel memory store fossilizing inside the transcript.

### Option C — TTL-based bounded

Tag messages with metadata, keep for N turns, purge after TTL expiry.

**Rejected in favor of simpler solution.** Requires:
- Machine-readable metadata tags embedded in message content (fragile prefix parsing or Rig Message extension)
- Per-block turn counters and TTL tracking
- 3 new config parameters (`injection_persistence_mode`, `injection_ttl_turns`, `max_injected_blocks_in_history`)
- Compaction pollution still exists during the TTL window

The cap-based approach achieves the same result with less complexity and no TTL tracking.

### Option D — Bounded persistence + compactor-aware filtering ✅ CHOSEN

Keep injected blocks in history with a hard cap on the number of live blocks. The compactor ignores injection blocks entirely. One new setting.

## 4. Chosen Solution

### Core principles

1. **Injected blocks persist in history** — preserves conversational continuity
2. **Hard cap on block count** — before each new injection, purge oldest blocks beyond limit
3. **Compactor ignores injection blocks** — no summarization of raw memory data
4. **One new setting** — `max_injected_blocks_in_history` (default: 3). Setting 0 = ephemeral mode.

### Behavior walk-through

**Normal multi-turn conversation:**
```
Turn 1: User asks about auth
  → inject block #1 (10 memories about auth)
  → history: [... block#1, user_msg, assistant_response]

Turn 2: User follow-up "why JWT?"
  → inject block #2 (5 memories, some overlap deduped)
  → block#1 still present — LLM has full auth context
  → history: [... block#1, ..., block#2, ...]

Turn 3: User asks about the database
  → inject block #3 (8 memories about DB)
  → blocks #1, #2 still present (cap=3, ok)

Turn 4: User continues on DB topic
  → inject block #4 (DB follow-up memories)
  → cap exceeded → purge block #1 (oldest)
  → auth context naturally fades as conversation moves on
```

**Multi-user scenario (Discord):**
```
Alice: "How does rate limiting work?"   → inject block #1 (rate limiting)
Bob:   "Did you see the game?"          → inject block #2 (nothing relevant)
Alice: "What about Redis for that?"     → inject block #3 (Redis + rate limiting)

Block #1 (rate limiting) is still in history. Alice's follow-up benefits from
it despite Bob's intervening message. Cap=3, no eviction yet.
```

**Compaction fires:**
```
Compactor reads history → render_messages_as_transcript skips [Context from memory] blocks
→ summary reflects actual dialogue only
→ injection blocks in the compacted zone are simply dropped
→ no reinforcement loop
```

### Injection block identification

Blocks are identified by a stable text prefix — no metadata parsing needed:

```rust
/// Shared prefix for injected memory context blocks.
const INJECTION_BLOCK_PREFIX: &str = "[Context from memory]";

/// Check if a Rig message is an injected memory context block.
fn is_injection_block(message: &Message) -> bool {
    match message {
        Message::User { content } => content.iter().any(|item| {
            matches!(item, UserContent::Text(t) if t.text.starts_with(INJECTION_BLOCK_PREFIX))
        }),
        _ => false,
    }
}
```

Reliable because: we control the prefix, users can't send messages with this prefix through the messaging adapter, and the prefix has been stable since V1.

### Purge logic

Before each new injection in `run_agent_turn`:

```rust
fn prune_old_injection_blocks(history: &mut Vec<Message>, max_keep: usize) {
    if max_keep == 0 {
        // Ephemeral mode: remove all injection blocks
        history.retain(|m| !is_injection_block(m));
        return;
    }

    let injection_indices: Vec<usize> = history.iter()
        .enumerate()
        .filter(|(_, m)| is_injection_block(m))
        .map(|(i, _)| i)
        .collect();

    if injection_indices.len() >= max_keep {
        let to_remove = injection_indices.len() - max_keep + 1; // +1 to make room for new block
        // Remove oldest first, iterate in reverse to preserve indices
        for &idx in injection_indices.iter().take(to_remove).rev() {
            history.remove(idx);
        }
    }
}
```

### Compactor filter

In `render_messages_as_transcript` (`src/agent/compactor.rs`):

```rust
fn render_messages_as_transcript(messages: &[Message]) -> String {
    let mut output = String::new();
    for message in messages {
        // Skip injected memory context — not part of the dialogue
        if is_injection_block(message) { continue; }
        // ... existing rendering logic ...
    }
    output
}
```

### Config changes

**New field:**
```toml
[memory_injection]
max_injected_blocks_in_history = 3  # 0 = ephemeral mode
```

**Default change:**
```toml
context_window_depth = 10  # was 50 — with persistent blocks, less re-injection delay needed
```

### Interaction with existing systems

| System | Interaction | Action needed |
|---|---|---|
| **Compactor (transcript)** | Injection blocks appear in compaction transcript | Filter them out in `render_messages_as_transcript` |
| **Compactor (token estimate)** | `estimate_history_tokens` counts injection blocks | No change — conservative estimate triggers earlier compaction (safe direction) |
| **Emergency truncation** | `drain(..N)` removes whatever is oldest, including blocks | No change — acceptable. Next turn re-injects |
| **Memory persistence branch** | Branch clones history including injection blocks | Monitor — branch could re-save existing memories. Add filter if excessive |
| **Regular branch fork** | Branch sees injection blocks in its context clone | Correct behavior — branch should have same context as the LLM |
| **`ConversationLogger`** | Only logs explicit user/bot messages | No change — injection blocks aren't logged to DB |
| **`ChannelInjectionState` dedup** | `context_window_depth` controls re-injection delay | Reduce default from 50 to 10 |

## 5. Future Improvement: Context-Aware Retrieval

Complementary to the persistence model — improves retrieval quality independently.

**Idea:** Enrich the hybrid search query with a summary of the last 2-3 exchanges so that implicit follow-ups ("why that?") benefit from conversational context in the search query itself.

```
Raw query:     "Why that instead of sessions?"
Enriched:      "Why that instead of sessions? [Context: discussing auth system,
                JWT token choice, OAuth integration]"
```

**Why not now:**
- Requires experimentation (how much context? full exchanges or summary?)
- Long enriched queries may dilute embedding quality or add FTS noise
- The bounded persistence solution handles the immediate need

**If it works well later:**
- `max_injected_blocks_in_history` can be reduced
- Ephemeral mode (cap=0) becomes fully viable
- The persistence model becomes a tuning knob, not a hard requirement

## 6. Validation Checklist

After implementation:
- [ ] Cap enforced: never more than N injection blocks in history
- [ ] `max_injected_blocks_in_history = 0` gives ephemeral behavior
- [ ] Compaction summaries contain zero `[Context from memory]` content
- [ ] Multi-turn follow-ups on same topic retain context (blocks visible)
- [ ] Topic change naturally evicts old injection blocks
- [ ] `context_window_depth = 10` prevents excessive re-injection
- [ ] Memory persistence branches don't re-save existing memories at excessive rates
- [ ] Branches forked during injection see the injected context
- [ ] Emergency truncation doesn't crash on injection blocks

## 7. Risks to Monitor

1. **Memory persistence branch duplication** — could re-save memories seen in injection blocks. Monitor; add filter to persistence branch prompt if needed.
2. **Emergency truncation** — drops injection blocks without awareness. Next turn re-injects. Acceptable.
3. **Token estimation** — counts injection blocks. With cap=3, ~3-9k extra tokens. Compaction triggers slightly earlier. Safe direction.
4. **Prefix stability** — `is_injection_block` relies on `INJECTION_BLOCK_PREFIX` constant. Format change in `run_agent_turn` without updating the constant breaks the filter. Using a shared constant prevents this.
