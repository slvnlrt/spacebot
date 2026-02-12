# Tools

How Spacebot gives LLM processes the ability to act.

## Overview

Every tool implements Rig's `Tool` trait and lives in `src/tools/`. Tools are organized by function, not by consumer. Which process gets which tools is configured via ToolServer factory functions in `src/tools.rs`.

All 12 tools:

| Tool | Purpose | Consumers |
|------|---------|-----------|
| `reply` | Send a message to the user | Channel |
| `branch` | Fork context to think independently | Channel |
| `spawn_worker` | Create a new worker process | Channel, Branch |
| `route` | Send follow-up to an active interactive worker | Channel |
| `cancel` | Stop a running worker or branch | Channel |
| `memory_save` | Write a memory to the store | Channel, Branch, Cortex |
| `memory_recall` | Search memories via hybrid search | Branch |
| `set_status` | Report worker progress to the channel | Worker |
| `shell` | Execute shell commands | Worker |
| `file` | Read, write, and list files | Worker |
| `exec` | Run subprocesses with specific args/env | Worker |
| `browser` | Headless Chrome automation (navigate, click, screenshot) | Worker |

## ToolServer Topology

Rig's `ToolServer` runs as a tokio task. You register tools on it, call `.run()` to get a `ToolServerHandle`, and pass that handle to agents. The handle is `Clone` — all clones point to the same server task.

Spacebot uses three ToolServer configurations:

### Channel/Branch ToolServer (shared)

One per agent, shared across all channels and branches for that agent.

```
┌─────────────────────────────────────────┐
│            Channel ToolServer            │
├─────────────────────────────────────────┤
│ Registered at startup:                  │
│   memory_save    (Arc<MemorySearch>)    │
│   memory_recall  (Arc<MemorySearch>)    │
│                                         │
│ Added/removed per conversation turn:    │
│   reply          (response_tx, conv_id) │
│   branch         (channel_id, event_tx) │
│   spawn_worker   (channel_id, event_tx) │
│   route          (channel_id, event_tx) │
│   cancel         (channel_id, event_tx) │
└─────────────────────────────────────────┘
```

Memory tools are stateless relative to conversations — they only need `Arc<MemorySearch>` which is per-agent and known at startup. They get registered once via `create_channel_tool_server()`.

Channel-specific tools hold per-conversation state (the response sender, the channel ID). They're added dynamically via `add_channel_tools()` when a conversation turn starts and removed via `remove_channel_tools()` when it ends. This prevents stale senders from being invoked after a turn is done.

### Worker ToolServer (per-worker)

Each worker gets its own isolated ToolServer, created at spawn time via `create_worker_tool_server()`.

```
┌──────────────────────────────────────────┐
│          Worker ToolServer (per-worker)   │
├──────────────────────────────────────────┤
│   shell                                  │
│   file                                   │
│   exec                                   │
│   set_status  (agent_id, worker_id, ...) │
│   browser     (if browser.enabled)       │
└──────────────────────────────────────────┘
```

`shell`, `file`, and `exec` are stateless unit structs. `set_status` is bound to a specific worker's ID so status updates route to the right place in the channel's status block. `browser` is conditionally registered based on the agent's `browser.enabled` config -- see [docs/browser.md](browser.md) for details.

Workers don't get memory tools or channel tools. They can't talk to the user, can't recall memories, can't spawn branches. They execute their task and report status.

### Cortex ToolServer

One per agent, minimal.

```
┌──────────────────────────────┐
│      Cortex ToolServer       │
├──────────────────────────────┤
│   memory_save                │
└──────────────────────────────┘
```

The cortex writes consolidated memories. It doesn't need recall (it's the consolidator, not the recaller) or any channel/worker tools.

## Factory Functions

All in `src/tools.rs`:

```rust
// Agent startup — creates the shared channel/branch ToolServer
create_channel_tool_server(memory_search) -> ToolServerHandle

// Per conversation turn — add/remove channel-specific tools
add_channel_tools(handle, channel_id, response_tx, conversation_id, event_tx)
remove_channel_tools(handle)

// Per worker spawn — creates an isolated ToolServer (browser conditionally included)
create_worker_tool_server(agent_id, worker_id, channel_id, event_tx, browser_config, screenshot_dir) -> ToolServerHandle

// Agent startup — creates the cortex ToolServer
create_cortex_tool_server(memory_search) -> ToolServerHandle
```

## Tool Lifecycle

### Static tools (registered at startup)

`memory_save`, `memory_recall` on the channel ToolServer. `shell`, `file`, `exec` on worker ToolServers. These are registered before `.run()` via the builder pattern and live for the lifetime of the ToolServer.

### Dynamic tools (added/removed at runtime)

`reply`, `branch`, `spawn_worker`, `route`, `cancel` on the channel ToolServer. Added via `handle.add_tool()` and removed via `handle.remove_tool()`. The add/remove cycle is per conversation turn:

```
1. Message arrives on channel
2. Channel creates response_tx for this turn
3. add_channel_tools(handle, channel_id, response_tx, ...)
4. Agent processes the message (LLM calls tools)
5. remove_channel_tools(handle)
6. Response sender drops, turn is complete
```

### Per-worker tools (created and destroyed with the worker)

The entire ToolServer is created when a worker spawns and dropped when the worker finishes. The `set_status` tool on each worker ToolServer is bound to that worker's ID.

## Tool Design Patterns

### Error as result

Tool errors are returned as structured results, not panics. The LLM sees the error and can decide to retry or take a different approach.

```rust
async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
    // Errors become tool results the LLM can read
    let content = tokio::fs::read_to_string(path)
        .await
        .map_err(|e| FileError(format!("Failed to read file: {e}")))?;
    // ...
}
```

### Protected paths

The `file` tool rejects reads and writes to identity/memory paths (`prompts/`, `identity/`, `data/`, `SOUL.md`, `IDENTITY.md`, `USER.md`). Workers should use `memory_save` for that content, not raw file writes.

### Status reporting

Workers report progress via `set_status`. The channel sees these in its status block. Status updates use `try_send` (non-blocking) so a slow event bus never blocks tool execution.

### Fire-and-forget sends

`set_status` uses `try_send` instead of `.await` on the event channel. If the channel is full, the update is dropped rather than blocking the worker.

## What Each Tool Does

### reply

Sends text to the user via the response channel. The channel process creates an `mpsc::Sender<OutboundResponse>` per turn and the tool pushes responses through it.

### branch

Spawns a branch process — a fork of the channel's context that thinks independently. Returns immediately with a `branch_id`. The branch result arrives later via ProcessEvent.

### spawn_worker

Creates a worker process for a specific task. Supports both fire-and-forget (do a job, return result) and interactive (accepts follow-up messages) modes. Returns immediately with a `worker_id`.

### route

Sends a follow-up message to an active interactive worker. The channel uses this to continue a multi-turn task without spawning a new worker.

### cancel

Terminates a running worker or branch. Immediate — the process is aborted.

### memory_save

Writes a structured memory to SQLite + generates an embedding in LanceDB. Supports typed memories (fact, preference, decision, identity, event, observation), importance scores, source attribution, and explicit associations to other memories.

### memory_recall

Hybrid search across the memory store. Combines vector similarity (semantic), full-text search (keyword), and graph traversal (connected memories) via Reciprocal Rank Fusion. Records access on found memories (affects importance decay).

### set_status

Reports the worker's current progress. The status string appears in the channel's status block so the user-facing process knows what's happening without polling.

### shell

Runs a shell command via `sh -c` (Unix) or `cmd /C` (Windows). Captures stdout, stderr, exit code. Has a configurable timeout (default 60s).

### file

Read, write, or list files. Protects identity/memory paths. Creates parent directories on write by default.

### exec

Runs a specific program with explicit arguments and environment variables. More precise than `shell` for running compilers, test runners, etc. Configurable timeout.

### browser

Headless Chrome automation via chromiumoxide. Single tool with an `action` discriminator: `launch`, `navigate`, `snapshot`, `act`, `screenshot`, `evaluate`, `content`, `close`, plus tab management (`open`, `tabs`, `focus`, `close_tab`). Uses an accessibility-tree ref system for LLM-friendly element addressing. See [docs/browser.md](browser.md).
