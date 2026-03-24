# Memory Injection Visualization — Detailed Implementation Plan

**Date:** 2026-02-25
**Feature:** Display memory injection blocks in the conversation timeline UI
**Branch:** merge/upstream-main-2026-02-24

## Overview

Add a compact, collapsible timeline item showing which memories were injected before each LLM turn.
Uses the same pattern as branch_runs / worker_runs: backend emits SSE event + persists to DB for history reload, frontend renders in timeline.

**Safety constraint:** The `memory_injection_events` table is for **UI display only**. It must NEVER be read back into LLM context, compactor input, or any process that feeds the model. Add defensive comments at every relevant location.

---

## Color Theme

- **Branches:** violet (`bg-violet-500/10`, `text-violet-300`)
- **Workers:** amber (`bg-amber-500/10`, `text-amber-300`)
- **Memory injection:** cyan (`bg-cyan-500/10`, `text-cyan-300`)

---

## Step-by-step Implementation

### STEP 1 — Migration: `migrations/20260225000001_memory_injection_events.sql`

```sql
-- Memory injection events for channel timeline display.
--
-- SAFETY: This table is for UI visualization ONLY. It must NEVER be read back
-- into LLM context, compactor input, cortex analysis, or any process that feeds
-- the model. Doing so would create a reinforcement loop where injected memories
-- get re-encoded into conversation context. See memory-injection-persistence-model.md.

CREATE TABLE IF NOT EXISTS memory_injection_events (
    id TEXT PRIMARY KEY,
    channel_id TEXT NOT NULL,
    pinned_json TEXT NOT NULL DEFAULT '[]',
    contextual_json TEXT NOT NULL DEFAULT '[]',
    pinned_count INTEGER NOT NULL DEFAULT 0,
    contextual_count INTEGER NOT NULL DEFAULT 0,
    injected_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    FOREIGN KEY (channel_id) REFERENCES channels(id) ON DELETE CASCADE
);

CREATE INDEX idx_memory_injection_events_channel
    ON memory_injection_events(channel_id, injected_at);
```

`pinned_json` and `contextual_json` store JSON arrays of `{memory_id, memory_type, content}` objects.
`pinned_count` and `contextual_count` are denormalized for the collapsed view (avoids parsing JSON just to show counts).

---

### STEP 2 — Backend types: `src/lib.rs`

Add struct + ProcessEvent variant after `AgentMessageReceived`:

```rust
/// Metadata for a single memory injected into channel context.
/// Used only for UI timeline display — never fed back to the LLM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InjectedMemoryInfo {
    pub memory_id: String,
    pub memory_type: String,
    pub content: String,
}

// Add to ProcessEvent enum:
MemoryInjected {
    agent_id: AgentId,
    channel_id: ChannelId,
    pinned: Vec<InjectedMemoryInfo>,
    contextual: Vec<InjectedMemoryInfo>,
},
```

---

### STEP 3 — Backend: Modify `compute_memory_injection` return type

**File:** `src/agent/channel.rs`

Change return from `Option<String>` to `Option<(String, Vec<InjectedMemoryInfo>, Vec<InjectedMemoryInfo>)>`:
- First element: the formatted text block (unchanged, for the LLM)
- Second: pinned memory infos (for event/persistence)
- Third: contextual memory infos (for event/persistence)

Build the `InjectedMemoryInfo` vectors from `final_memories` at the same place we already build `pinned_lines` / `contextual_lines`:

```rust
let pinned_infos: Vec<InjectedMemoryInfo> = final_memories
    .iter()
    .filter(|(source, _)| matches!(source, InjectionSource::Pinned))
    .map(|(_, memory)| InjectedMemoryInfo {
        memory_id: memory.id.clone(),
        memory_type: memory.memory_type.to_string(),
        content: memory.content.clone(),
    })
    .collect();

let contextual_infos: Vec<InjectedMemoryInfo> = final_memories
    .iter()
    .filter(|(source, _)| matches!(source, InjectionSource::Contextual))
    .map(|(_, memory)| InjectedMemoryInfo {
        memory_id: memory.id.clone(),
        memory_type: memory.memory_type.to_string(),
        content: memory.content.clone(),
    })
    .collect();
```

Update the 2 call sites (`handle_message_batch` and `handle_message`) to destructure the new tuple.

---

### STEP 4 — Backend: Emit event + persist in `run_agent_turn`

**File:** `src/agent/channel.rs`, in `run_agent_turn`, right after injecting the block into history (around line 1825):

```rust
if let Some((ref context, ref pinned_infos, ref contextual_infos)) = injected_context {
    // ... existing pruning + push code ...

    // Emit SSE event for UI timeline
    let injection_id = uuid::Uuid::new_v4().to_string();
    self.deps.event_tx.send(ProcessEvent::MemoryInjected {
        agent_id: self.deps.agent_id.clone(),
        channel_id: self.id.clone(),
        pinned: pinned_infos.clone(),
        contextual: contextual_infos.clone(),
    }).ok();

    // Persist for timeline history reload (fire-and-forget)
    self.process_run_logger.log_memory_injection(
        &self.id,
        &injection_id,
        pinned_infos,
        contextual_infos,
    );
}
```

---

### STEP 5 — Backend: ProcessRunLogger persistence

**File:** `src/conversation/history.rs`

Add to `ProcessRunLogger`:

```rust
/// Record a memory injection event. Fire-and-forget.
///
/// SAFETY: This data is for UI timeline display ONLY. It must never be loaded
/// back into LLM context or compactor input. See memory-injection-persistence-model.md.
pub fn log_memory_injection(
    &self,
    channel_id: &ChannelId,
    injection_id: &str,
    pinned: &[crate::InjectedMemoryInfo],
    contextual: &[crate::InjectedMemoryInfo],
) {
    let pool = self.pool.clone();
    let id = injection_id.to_string();
    let channel_id = channel_id.to_string();
    let pinned_json = serde_json::to_string(pinned).unwrap_or_default();
    let contextual_json = serde_json::to_string(contextual).unwrap_or_default();
    let pinned_count = pinned.len() as i64;
    let contextual_count = contextual.len() as i64;

    tokio::spawn(async move {
        if let Err(error) = sqlx::query(
            "INSERT OR IGNORE INTO memory_injection_events \
             (id, channel_id, pinned_json, contextual_json, pinned_count, contextual_count) \
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(&channel_id)
        .bind(&pinned_json)
        .bind(&contextual_json)
        .bind(pinned_count)
        .bind(contextual_count)
        .execute(&pool)
        .await
        {
            tracing::warn!(%error, "failed to persist memory injection event");
        }
    });
}
```

---

### STEP 6 — Backend: TimelineItem enum + timeline query

**File:** `src/conversation/history.rs`

Add variant to `TimelineItem`:

```rust
/// Memory injection visualization. UI-only — never fed back to the LLM.
MemoryInjection {
    id: String,
    pinned: Vec<crate::InjectedMemoryInfo>,
    contextual: Vec<crate::InjectedMemoryInfo>,
    pinned_count: i64,
    contextual_count: i64,
    injected_at: String,
},
```

Update the UNION ALL query in `load_channel_timeline` to include memory_injection_events:

```sql
UNION ALL
SELECT 'memory_injection' AS item_type, id, NULL, NULL, NULL, NULL,
       NULL, NULL, NULL, NULL, NULL,
       injected_at AS timestamp, NULL AS completed_at,
       pinned_json, contextual_json, pinned_count, contextual_count
FROM memory_injection_events WHERE channel_id = ?1
```

(Need to add NULL columns for pinned_json/contextual_json/counts to the other SELECT branches, or — simpler — handle with separate columns.)

Add row parsing in the filter_map:

```rust
"memory_injection" => {
    let pinned_json: String = row.try_get("pinned_json").unwrap_or_default();
    let contextual_json: String = row.try_get("contextual_json").unwrap_or_default();
    let pinned: Vec<crate::InjectedMemoryInfo> =
        serde_json::from_str(&pinned_json).unwrap_or_default();
    let contextual: Vec<crate::InjectedMemoryInfo> =
        serde_json::from_str(&contextual_json).unwrap_or_default();
    Some(TimelineItem::MemoryInjection {
        id: row.try_get("id").unwrap_or_default(),
        pinned,
        contextual,
        pinned_count: row.try_get("pinned_count").unwrap_or(0),
        contextual_count: row.try_get("contextual_count").unwrap_or(0),
        injected_at: row
            .try_get::<chrono::DateTime<chrono::Utc>, _>("timestamp")
            .map(|t| t.to_rfc3339())
            .unwrap_or_default(),
    })
}
```

---

### STEP 7 — Backend: ApiEvent + SSE forwarding

**File:** `src/api/state.rs`

Add variant to `ApiEvent`:

```rust
/// Memory injection completed for a channel (UI visualization only).
MemoryInjected {
    agent_id: String,
    channel_id: String,
    pinned: Vec<crate::InjectedMemoryInfo>,
    contextual: Vec<crate::InjectedMemoryInfo>,
},
```

Add forwarding in `register_agent_events` match:

```rust
ProcessEvent::MemoryInjected {
    channel_id,
    pinned,
    contextual,
    ..
} => {
    api_tx
        .send(ApiEvent::MemoryInjected {
            agent_id: agent_id.clone(),
            channel_id: channel_id.to_string(),
            pinned,
            contextual,
        })
        .ok();
}
```

---

### STEP 8 — Frontend types: `interface/src/api/client.ts`

Add types:

```typescript
export interface InjectedMemoryInfo {
    memory_id: string;
    memory_type: string;
    content: string;
}

export interface TimelineMemoryInjection {
    type: "memory_injection";
    id: string;
    pinned: InjectedMemoryInfo[];
    contextual: InjectedMemoryInfo[];
    pinned_count: number;
    contextual_count: number;
    injected_at: string;
}

export interface MemoryInjectedEvent {
    type: "memory_injected";
    agent_id: string;
    channel_id: string;
    pinned: InjectedMemoryInfo[];
    contextual: InjectedMemoryInfo[];
}
```

Update unions:

```typescript
export type TimelineItem = TimelineMessage | TimelineBranchRun | TimelineWorkerRun | TimelineMemoryInjection;

export type ApiEvent = /* existing */ | MemoryInjectedEvent;
```

---

### STEP 9 — Frontend SSE handler: `interface/src/hooks/useChannelLiveState.ts`

Add to `itemTimestamp`:
```typescript
case "memory_injection": return item.injected_at;
```

Add handler:
```typescript
const handleMemoryInjected = useCallback((data: unknown) => {
    const event = data as MemoryInjectedEvent;
    pushItem(event.channel_id, {
        type: "memory_injection",
        id: `inj-${Date.now()}-${crypto.randomUUID()}`,
        pinned: event.pinned,
        contextual: event.contextual,
        pinned_count: event.pinned.length,
        contextual_count: event.contextual.length,
        injected_at: new Date().toISOString(),
    });
}, [pushItem]);
```

Add to handlers map:
```typescript
memory_injected: handleMemoryInjected,
```

Import `MemoryInjectedEvent` from client.

---

### STEP 10 — Frontend UI component: `interface/src/routes/ChannelDetail.tsx`

Add `MemoryInjectionItem` component:

```tsx
function MemoryInjectionItem({ item }: { item: TimelineMemoryInjection }) {
    const [expanded, setExpanded] = useState(false);
    const totalCount = item.pinned_count + item.contextual_count;

    return (
        <div className="flex gap-3 px-3 py-1">
            <span className="flex-shrink-0 pt-0.5 text-tiny text-ink-faint">
                {formatTimestamp(new Date(item.injected_at).getTime())}
            </span>
            <div className="min-w-0 flex-1">
                <button
                    type="button"
                    onClick={() => setExpanded(!expanded)}
                    className="w-full rounded-md bg-cyan-500/10 px-3 py-1.5 text-left transition-colors hover:bg-cyan-500/15 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-cyan-400/60"
                >
                    <div className="flex items-center gap-2">
                        <div className="h-2 w-2 rounded-full bg-cyan-400/50" />
                        <span className="text-sm font-medium text-cyan-300">Memory</span>
                        <span className="text-tiny text-ink-faint">
                            {totalCount} memor{totalCount !== 1 ? "ies" : "y"}
                            {item.pinned_count > 0 && ` · ${item.pinned_count} pinned`}
                        </span>
                        <span className="ml-auto text-tiny text-ink-faint">
                            {expanded ? "▾" : "▸"}
                        </span>
                    </div>
                </button>
                {expanded && (
                    <div className="mt-1 rounded-md border border-cyan-500/10 bg-cyan-500/5 px-3 py-2 text-sm text-ink-dull">
                        {item.pinned.length > 0 && (
                            <>
                                <div className="mb-1 text-tiny font-medium text-cyan-400/70">Pinned context</div>
                                {item.pinned.map((m) => (
                                    <div key={m.memory_id} className="flex gap-2 py-0.5">
                                        <span className="flex-shrink-0 rounded bg-cyan-500/15 px-1 text-tiny font-medium text-cyan-300">
                                            {m.memory_type}
                                        </span>
                                        <span className="text-ink-dull">{m.content}</span>
                                    </div>
                                ))}
                            </>
                        )}
                        {item.contextual.length > 0 && (
                            <>
                                {item.pinned.length > 0 && <div className="my-1.5 border-t border-cyan-500/10" />}
                                <div className="mb-1 text-tiny font-medium text-cyan-400/70">Relevant to this message</div>
                                {item.contextual.map((m) => (
                                    <div key={m.memory_id} className="flex gap-2 py-0.5">
                                        <span className="flex-shrink-0 rounded bg-cyan-500/15 px-1 text-tiny font-medium text-cyan-300">
                                            {m.memory_type}
                                        </span>
                                        <span className="text-ink-dull">{m.content}</span>
                                    </div>
                                ))}
                            </>
                        )}
                    </div>
                )}
            </div>
        </div>
    );
}
```

Add to `TimelineEntry` switch:
```typescript
case "memory_injection":
    return <MemoryInjectionItem item={item} />;
```

Import `TimelineMemoryInjection` in the imports.

---

### STEP 11 — Safety comments

Add defensive comments at these locations:

1. **`src/conversation/history.rs`** — on `load_channel_timeline`:
   ```
   // WARNING: This function returns data for UI display only. If you ever need to
   // load conversation data for LLM context, compactor, or cortex, use
   // ConversationLogger::load_recent() which contains only user/assistant messages.
   // Memory injection events (memory_injection_events table) must NEVER be fed
   // back to the LLM — this would create a reinforcement loop.
   // See memory-v2-plan/IMPLEMENTATION 2/memory-injection-persistence-model.md.
   ```

2. **`src/conversation/history.rs`** — on `ConversationLogger` (top of struct):
   ```
   // NOTE: ConversationLogger deliberately logs only user and assistant messages.
   // Memory injection blocks, branch/worker metadata, and other timeline display
   // data are handled by ProcessRunLogger and stored in separate tables that are
   // NEVER read back into LLM context. This separation prevents memory injection
   // data from creating reinforcement loops. Do not merge these concerns.
   ```

3. **Migration file** — already has the comment (see step 1).

4. **`src/agent/channel.rs`** — on `compute_memory_injection`:
   ```
   // NOTE: The structured metadata (InjectedMemoryInfo) returned alongside the
   // text block is for UI visualization only (SSE event + DB persistence in
   // memory_injection_events). Only the text block is injected into LLM context.
   ```

---

## Files Changed Summary

| # | File | Type | Change |
|---|------|------|--------|
| 1 | `migrations/20260225000001_memory_injection_events.sql` | **NEW** | Migration for timeline DB table |
| 2 | `src/lib.rs` | EDIT | `InjectedMemoryInfo` struct + `ProcessEvent::MemoryInjected` variant |
| 3 | `src/agent/channel.rs` | EDIT | Return structured data from `compute_memory_injection`, emit event + persist in `run_agent_turn` |
| 4 | `src/conversation/history.rs` | EDIT | `log_memory_injection`, `TimelineItem::MemoryInjection`, updated query, safety comments |
| 5 | `src/api/state.rs` | EDIT | `ApiEvent::MemoryInjected` + SSE forwarding |
| 6 | `interface/src/api/client.ts` | EDIT | New TS types + union extensions |
| 7 | `interface/src/hooks/useChannelLiveState.ts` | EDIT | `handleMemoryInjected` + handler registration |
| 8 | `interface/src/routes/ChannelDetail.tsx` | EDIT | `MemoryInjectionItem` component + TimelineEntry routing |

## Key Patterns Followed

- Same fire-and-forget DB pattern as branch_runs/worker_runs
- Same SSE event → pushItem → timeline pattern
- Same collapsible component pattern (useState toggle, chevron, conditional render)
- Same data flow: RAM → DB → API (one-way, never back to LLM)
- Distinct color theme (cyan) for visual differentiation
