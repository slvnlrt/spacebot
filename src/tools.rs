//! Tools available to agents.
//!
//! Tools are organized by function, not by consumer. Which agents get which tools
//! is configured via the ToolServer factory functions below.
//!
//! ## ToolServer Topology
//!
//! **Channel ToolServer** (one per agent, shared across channels):
//! - `memory_save` — registered at startup (stateless, uses `Arc<MemorySearch>`)
//! - `reply`, `branch`, `spawn_worker`, `route`, `cancel` — added dynamically
//!   per conversation turn via `add_channel_tools()` / `remove_channel_tools()`
//!   because they hold per-channel state.
//!
//! **Branch ToolServer** (shared with channel):
//! - Uses the same ToolServer as the channel. Branch tools (`memory_recall`,
//!   `memory_save`, `spawn_worker`) are registered at startup.
//!
//! **Worker ToolServer** (one per worker, created at spawn time):
//! - `shell`, `file`, `exec` — stateless, registered at creation
//! - `set_status` — per-worker instance, registered at creation
//!
//! **Cortex ToolServer** (one per agent):
//! - `memory_save` — registered at startup

pub mod reply;
pub mod branch_tool;
pub mod spawn_worker;
pub mod route;
pub mod cancel;
pub mod memory_save;
pub mod memory_recall;
pub mod set_status;
pub mod shell;
pub mod file;
pub mod exec;
pub mod browser;

pub use reply::{ReplyTool, ReplyArgs, ReplyOutput, ReplyError};
pub use branch_tool::{BranchTool, BranchArgs, BranchOutput, BranchError};
pub use spawn_worker::{SpawnWorkerTool, SpawnWorkerArgs, SpawnWorkerOutput, SpawnWorkerError};
pub use route::{RouteTool, RouteArgs, RouteOutput, RouteError};
pub use cancel::{CancelTool, CancelArgs, CancelOutput, CancelError};
pub use memory_save::{MemorySaveTool, MemorySaveArgs, MemorySaveOutput, MemorySaveError, AssociationInput};
pub use memory_recall::{MemoryRecallTool, MemoryRecallArgs, MemoryRecallOutput, MemoryRecallError, MemoryOutput};
pub use set_status::{SetStatusTool, SetStatusArgs, SetStatusOutput, SetStatusError};
pub use shell::{ShellTool, ShellArgs, ShellOutput, ShellError, ShellResult};
pub use file::{FileTool, FileArgs, FileOutput, FileError, FileEntryOutput, FileEntry, FileType};
pub use exec::{ExecTool, ExecArgs, ExecOutput, ExecError, ExecResult, EnvVar};
pub use browser::{BrowserTool, BrowserArgs, BrowserOutput, BrowserError, BrowserAction, ActKind, ElementSummary, TabInfo};

use crate::agent::channel::ChannelState;
use crate::config::BrowserConfig;
use crate::memory::MemorySearch;
use crate::{AgentId, ChannelId, OutboundResponse, ProcessEvent, WorkerId};
use rig::tool::Tool as _;
use rig::tool::server::{ToolServer, ToolServerHandle};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc};

/// Create the shared ToolServer for channels and branches.
///
/// Registers tools that are available at agent startup (memory tools). Channel-specific
/// tools (reply, branch, spawn_worker, route, cancel) are added dynamically per
/// conversation turn because they hold per-channel state.
pub fn create_channel_tool_server(memory_search: Arc<MemorySearch>) -> ToolServerHandle {
    ToolServer::new()
        .tool(MemorySaveTool::new(memory_search.clone()))
        .tool(MemoryRecallTool::new(memory_search))
        .run()
}

/// Add per-channel tools to a running ToolServer.
///
/// Called when a conversation turn begins. These tools hold per-channel state
/// (channel history, active branches/workers, response sender) that can't be
/// known at agent startup. Cleaned up via `remove_channel_tools()` when the
/// turn ends.
pub async fn add_channel_tools(
    handle: &ToolServerHandle,
    state: ChannelState,
    response_tx: mpsc::Sender<OutboundResponse>,
    conversation_id: impl Into<String>,
) -> Result<(), rig::tool::server::ToolServerError> {
    handle.add_tool(ReplyTool::new(
        response_tx,
        conversation_id,
        state.conversation_logger.clone(),
        state.channel_id.clone(),
    )).await?;
    handle.add_tool(BranchTool::new(state.clone())).await?;
    handle.add_tool(SpawnWorkerTool::new(state.clone())).await?;
    handle.add_tool(RouteTool::new(state.clone())).await?;
    handle.add_tool(CancelTool::new(state)).await?;
    Ok(())
}

/// Remove per-channel tools from a running ToolServer.
///
/// Called when a conversation turn ends or a channel is torn down. Prevents stale
/// tools from being invoked with dead senders.
pub async fn remove_channel_tools(
    handle: &ToolServerHandle,
) -> Result<(), rig::tool::server::ToolServerError> {
    handle.remove_tool(ReplyTool::NAME).await?;
    handle.remove_tool(BranchTool::NAME).await?;
    handle.remove_tool(SpawnWorkerTool::NAME).await?;
    handle.remove_tool(RouteTool::NAME).await?;
    handle.remove_tool(CancelTool::NAME).await?;
    Ok(())
}

/// Create a per-worker ToolServer with task-appropriate tools.
///
/// Each worker gets its own isolated ToolServer. The `set_status` tool is bound to
/// the specific worker's ID so status updates route correctly. The browser tool
/// is included when browser automation is enabled in the agent config.
pub fn create_worker_tool_server(
    agent_id: AgentId,
    worker_id: WorkerId,
    channel_id: Option<ChannelId>,
    event_tx: broadcast::Sender<ProcessEvent>,
    browser_config: BrowserConfig,
    screenshot_dir: PathBuf,
) -> ToolServerHandle {
    let mut server = ToolServer::new()
        .tool(ShellTool::new())
        .tool(FileTool::new())
        .tool(ExecTool::new())
        .tool(SetStatusTool::new(agent_id, worker_id, channel_id, event_tx));

    if browser_config.enabled {
        server = server.tool(BrowserTool::new(browser_config, screenshot_dir));
    }

    server.run()
}

/// Create a ToolServer for the cortex process.
///
/// The cortex only needs memory_save for consolidation. Additional tools can be
/// added later as cortex capabilities expand.
pub fn create_cortex_tool_server(memory_search: Arc<MemorySearch>) -> ToolServerHandle {
    ToolServer::new()
        .tool(MemorySaveTool::new(memory_search))
        .run()
}
