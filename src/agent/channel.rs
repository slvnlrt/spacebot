//! Channel: User-facing conversation process.

use crate::agent::branch::Branch;
use crate::agent::compactor::Compactor;
use crate::agent::status::StatusBlock;
use crate::agent::worker::Worker;
use crate::config::ApiType;
use crate::conversation::{ChannelStore, ConversationLogger, ProcessRunLogger};
use crate::error::{AgentError, Result};
use crate::hooks::SpacebotHook;
use crate::llm::SpacebotModel;
use crate::memory::{
    cosine_similarity, is_semantically_duplicate, MemoryType, SearchConfig, SearchMode, SearchSort,
    SourceSignal,
};
use crate::{
    AgentDeps, BranchId, ChannelId, InboundMessage, OutboundResponse, ProcessEvent, ProcessId,
    ProcessType, WorkerId,
};

use chrono::{DateTime, Local, Utc};
use chrono_tz::Tz;
use futures::future::join_all;
use rig::agent::AgentBuilder;
use rig::completion::{CompletionModel, Prompt};
use rig::message::{ImageMediaType, MimeType, UserContent};
use rig::one_or_many::OneOrMany;
use rig::tool::server::ToolServer;
use tokio::sync::broadcast;
use tokio::sync::{RwLock, mpsc};
use tracing::Instrument as _;

use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::sync::Arc;

/// Debounce window for retriggers: coalesce rapid branch/worker completions
/// into a single retrigger instead of firing one per event.
const RETRIGGER_DEBOUNCE_MS: u64 = 500;

/// Maximum retriggers allowed since the last real user message. Prevents
/// infinite retrigger cascades where each retrigger spawns more work.
const MAX_RETRIGGERS_PER_TURN: usize = 3;

/// Stable prefix for injected memory context blocks.
pub(crate) const INJECTION_BLOCK_PREFIX: &str = "[Context from memory]";

/// Check whether a message is a memory-injection context block.
pub(crate) fn is_injection_block(message: &rig::message::Message) -> bool {
    match message {
        rig::message::Message::User { content } => content.iter().any(|item| {
            matches!(item, UserContent::Text(t) if t.text.starts_with(INJECTION_BLOCK_PREFIX))
        }),
        _ => false,
    }
}

/// Keep at most `max_keep` injection blocks in history.
///
/// If `max_keep == 0`, all injection blocks are removed (ephemeral mode).
/// Called before adding a new block so there is room for the incoming one.
fn prune_old_injection_blocks(history: &mut Vec<rig::message::Message>, max_keep: usize) {
    if max_keep == 0 {
        history.retain(|message| !is_injection_block(message));
        return;
    }

    let injection_indices: Vec<usize> = history
        .iter()
        .enumerate()
        .filter_map(|(index, message)| is_injection_block(message).then_some(index))
        .collect();

    if injection_indices.len() >= max_keep {
        let to_remove = injection_indices.len() - max_keep + 1;
        for &index in injection_indices[..to_remove].iter().rev() {
            history.remove(index);
        }
    }
}

#[derive(Debug, Clone)]
enum TemporalTimezone {
    Named { timezone_name: String, timezone: Tz },
    SystemLocal,
}

#[derive(Debug, Clone)]
struct TemporalContext {
    now_utc: DateTime<Utc>,
    timezone: TemporalTimezone,
}

impl TemporalContext {
    fn from_runtime(runtime_config: &crate::config::RuntimeConfig) -> Self {
        let now_utc = Utc::now();
        let user_timezone = runtime_config.user_timezone.load().as_ref().clone();
        let cron_timezone = runtime_config.cron_timezone.load().as_ref().clone();

        Self {
            now_utc,
            timezone: Self::resolve_timezone_from_names(user_timezone, cron_timezone),
        }
    }

    fn resolve_timezone_from_names(
        user_timezone: Option<String>,
        cron_timezone: Option<String>,
    ) -> TemporalTimezone {
        if let Some(timezone_name) = user_timezone {
            match timezone_name.parse::<Tz>() {
                Ok(timezone) => {
                    return TemporalTimezone::Named {
                        timezone_name,
                        timezone,
                    };
                }
                Err(_) => {
                    let cron_timezone_candidate =
                        cron_timezone.as_deref().unwrap_or("none configured");
                    tracing::warn!(
                        timezone = %timezone_name,
                        cron_timezone = %cron_timezone_candidate,
                        "invalid runtime timezone for channel temporal context, will try cron_timezone then fall back to system local"
                    );
                }
            }
        }

        if let Some(timezone_name) = cron_timezone {
            match timezone_name.parse::<Tz>() {
                Ok(timezone) => {
                    return TemporalTimezone::Named {
                        timezone_name,
                        timezone,
                    };
                }
                Err(error) => {
                    tracing::warn!(
                        timezone = %timezone_name,
                        error = %error,
                        "invalid cron_timezone for channel temporal context, falling back to system local"
                    );
                }
            }
        }

        TemporalTimezone::SystemLocal
    }

    fn format_timestamp(&self, timestamp: DateTime<Utc>) -> String {
        match &self.timezone {
            TemporalTimezone::Named {
                timezone_name,
                timezone,
            } => {
                let local_timestamp = timestamp.with_timezone(timezone);
                format!(
                    "{} ({}, UTC{})",
                    local_timestamp.format("%Y-%m-%d %H:%M:%S %Z"),
                    timezone_name,
                    local_timestamp.format("%:z")
                )
            }
            TemporalTimezone::SystemLocal => {
                let local_timestamp = timestamp.with_timezone(&Local);
                format!(
                    "{} (system local, UTC{})",
                    local_timestamp.format("%Y-%m-%d %H:%M:%S %Z"),
                    local_timestamp.format("%:z")
                )
            }
        }
    }

    fn current_time_line(&self) -> String {
        format!(
            "{}; UTC {}",
            self.format_timestamp(self.now_utc),
            self.now_utc.format("%Y-%m-%d %H:%M:%S UTC")
        )
    }

    fn worker_task_preamble(&self, prompt_engine: &crate::prompts::PromptEngine) -> Result<String> {
        let local_time = self.format_timestamp(self.now_utc);
        let utc_time = self.now_utc.format("%Y-%m-%d %H:%M:%S UTC").to_string();
        prompt_engine.render_system_worker_time_context(&local_time, &utc_time)
    }
}

fn build_worker_task_with_temporal_context(
    task: &str,
    temporal_context: &TemporalContext,
    prompt_engine: &crate::prompts::PromptEngine,
) -> Result<String> {
    let preamble = temporal_context.worker_task_preamble(prompt_engine)?;
    Ok(format!("{preamble}\n\n{task}"))
}

/// A background process result waiting to be relayed to the user via retrigger.
///
/// Instead of injecting raw result text into history as a fake "User" message
/// (where it can be confused with prior results), pending results are accumulated
/// here and embedded directly into the retrigger message text. This gives the
/// LLM unambiguous, ID-tagged results to relay.
#[derive(Clone, Debug)]
struct PendingResult {
    /// "branch" or "worker"
    process_type: &'static str,
    /// The branch or worker ID (short UUID).
    process_id: String,
    /// The result/conclusion text from the process.
    result: String,
    /// Whether the process completed successfully.
    success: bool,
}

/// Shared state that channel tools need to act on the channel.
///
/// Wrapped in Arc and passed to tools (branch, spawn_worker, route, cancel)
/// so they can create real Branch/Worker processes when the LLM invokes them.
#[derive(Clone)]
pub struct ChannelState {
    pub channel_id: ChannelId,
    pub history: Arc<RwLock<Vec<rig::message::Message>>>,
    pub active_branches: Arc<RwLock<HashMap<BranchId, tokio::task::JoinHandle<()>>>>,
    pub active_workers: Arc<RwLock<HashMap<WorkerId, Worker>>>,
    /// Tokio task handles for running workers, used for cancellation via abort().
    pub worker_handles: Arc<RwLock<HashMap<WorkerId, tokio::task::JoinHandle<()>>>>,
    /// Input senders for interactive workers, keyed by worker ID.
    /// Used by the route tool to deliver follow-up messages.
    pub worker_inputs: Arc<RwLock<HashMap<WorkerId, tokio::sync::mpsc::Sender<String>>>>,
    pub status_block: Arc<RwLock<StatusBlock>>,
    pub deps: AgentDeps,
    pub conversation_logger: ConversationLogger,
    pub process_run_logger: ProcessRunLogger,
    /// Discord message ID to reply to for work spawned in the current turn.
    pub reply_target_message_id: Arc<RwLock<Option<u64>>>,
    pub channel_store: ChannelStore,
    pub screenshot_dir: std::path::PathBuf,
    pub logs_dir: std::path::PathBuf,
}

impl ChannelState {
    /// Cancel a running worker by aborting its tokio task and cleaning up state.
    /// Returns an error message if the worker is not found.
    pub async fn cancel_worker(&self, worker_id: WorkerId) -> std::result::Result<(), String> {
        let handle = self.worker_handles.write().await.remove(&worker_id);
        let removed = self
            .active_workers
            .write()
            .await
            .remove(&worker_id)
            .is_some();
        self.worker_inputs.write().await.remove(&worker_id);

        if let Some(handle) = handle {
            handle.abort();
            // Mark the DB row as cancelled since the abort prevents WorkerComplete from firing
            self.process_run_logger
                .log_worker_completed(worker_id, "Worker cancelled", false);
            Ok(())
        } else if removed {
            self.process_run_logger
                .log_worker_completed(worker_id, "Worker cancelled", false);
            Ok(())
        } else {
            Err(format!("Worker {worker_id} not found"))
        }
    }

    /// Cancel a running branch by aborting its tokio task.
    /// Returns an error message if the branch is not found.
    pub async fn cancel_branch(&self, branch_id: BranchId) -> std::result::Result<(), String> {
        let handle = self.active_branches.write().await.remove(&branch_id);
        if let Some(handle) = handle {
            handle.abort();
            Ok(())
        } else {
            Err(format!("Branch {branch_id} not found"))
        }
    }
}

impl std::fmt::Debug for ChannelState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChannelState")
            .field("channel_id", &self.channel_id)
            .finish_non_exhaustive()
    }
}

/// State for memory injection deduplication within a channel.
///
/// Stored in RAM directly in Channel (not in ChannelState) because:
/// - It's specific to each channel (avoids conflicts between Discord, Slack, etc.)
/// - Methods like `handle_message` take `&mut self`, allowing direct mutation
/// - No need for async locks
///
/// The state is NOT reset during compaction because injected memories influence
/// the conversation and their essence is captured in the compaction summary.
/// Resetting would cause duplicate injection of raw memories.
pub struct ChannelInjectionState {
    /// Maps memory_id to the turn number when it was injected.
    /// Bounded to prevent memory leaks (oldest entries pruned when exceeding MAX_ENTRIES).
    pub injected_ids: HashMap<String, usize>,
    /// Buffer of embeddings for semantic deduplication with insertion turn.
    /// When a new memory's embedding is too similar (cosine > threshold) to any
    /// in this buffer, the memory is skipped.
    pub semantic_buffer: VecDeque<(Vec<f32>, usize)>,
}

impl ChannelInjectionState {
    /// Maximum number of entries in injected_ids before pruning.
    const MAX_ENTRIES: usize = 100;

    /// Create a new empty injection state.
    pub fn new() -> Self {
        Self {
            injected_ids: HashMap::new(),
            semantic_buffer: VecDeque::new(),
        }
    }

    /// Check if a memory should be re-injected based on context window depth.
    ///
    /// Returns `true` if:
    /// - The memory was never injected, OR
    /// - The memory was injected but has fallen out of the context window
    ///
    /// Returns `false` if the memory is still within the context window.
    pub fn should_reinject(
        &self,
        memory_id: &str,
        current_turn: usize,
        context_window_depth: usize,
    ) -> bool {
        match self.injected_ids.get(memory_id) {
            Some(&injected_turn) => {
                injected_turn < current_turn.saturating_sub(context_window_depth)
            }
            None => true,
        }
    }

    /// Record that a memory was injected at the given turn.
    pub fn record_injection(&mut self, memory_id: String, turn: usize) {
        self.injected_ids.insert(memory_id, turn);
        self.prune_if_needed();
    }

    /// Add an embedding to the semantic buffer for future deduplication.
    pub fn add_embedding(&mut self, embedding: Vec<f32>, turn: usize) {
        self.semantic_buffer.push_back((embedding, turn));
        // Keep buffer bounded
        if self.semantic_buffer.len() > Self::MAX_ENTRIES {
            self.semantic_buffer.pop_front();
        }
    }

    /// Prune semantic embeddings older than the active context window.
    pub fn prune_semantic_buffer(&mut self, current_turn: usize, context_window_depth: usize) {
        let oldest_kept_turn = current_turn.saturating_sub(context_window_depth);
        self.semantic_buffer
            .retain(|(_, inserted_turn)| *inserted_turn >= oldest_kept_turn);
    }

    /// Prune old entries if the map exceeds MAX_ENTRIES.
    fn prune_if_needed(&mut self) {
        if self.injected_ids.len() <= Self::MAX_ENTRIES {
            return;
        }

        // Collect IDs with their turn numbers, then sort to find oldest
        let mut turns: Vec<(String, usize)> = self
            .injected_ids
            .iter()
            .map(|(id, &turn)| (id.clone(), turn))
            .collect();
        turns.sort_by_key(|(_, turn)| *turn);

        // Remove entries with the lowest turn numbers
        let to_remove = turns.len() - Self::MAX_ENTRIES;
        for (id, _) in turns.into_iter().take(to_remove) {
            self.injected_ids.remove(&id);
        }
    }
}

impl Default for ChannelInjectionState {
    fn default() -> Self {
        Self::new()
    }
}

/// User-facing conversation process.
pub struct Channel {
    pub id: ChannelId,
    pub title: Option<String>,
    pub deps: AgentDeps,
    pub hook: SpacebotHook,
    pub state: ChannelState,
    /// Per-channel tool server (isolated from other channels).
    pub tool_server: rig::tool::server::ToolServerHandle,
    /// Input channel for receiving messages.
    pub message_rx: mpsc::Receiver<InboundMessage>,
    /// Event receiver for process events.
    pub event_rx: broadcast::Receiver<ProcessEvent>,
    /// Outbound response sender for the messaging layer.
    pub response_tx: mpsc::Sender<OutboundResponse>,
    /// Self-sender for re-triggering the channel after background process completion.
    pub self_tx: mpsc::Sender<InboundMessage>,
    /// Conversation ID from the first message (for synthetic re-trigger messages).
    pub conversation_id: Option<String>,
    /// Adapter source captured from the first non-system message.
    pub source_adapter: Option<String>,
    /// Conversation context (platform, channel name, server) captured from the first message.
    pub conversation_context: Option<String>,
    /// Context monitor that triggers background compaction.
    pub compactor: Compactor,
    /// Count of user messages since last memory persistence branch.
    message_count: usize,
    /// Branch IDs for silent memory persistence branches (results not injected into history).
    memory_persistence_branches: HashSet<BranchId>,
    /// Optional Discord reply target captured when each branch was started.
    branch_reply_targets: HashMap<BranchId, u64>,
    /// Buffer for coalescing rapid-fire messages.
    coalesce_buffer: Vec<InboundMessage>,
    /// Deadline for flushing the coalesce buffer.
    coalesce_deadline: Option<tokio::time::Instant>,
    /// Current turn number (incremented after each user message).
    current_turn: usize,
    /// State for memory injection deduplication.
    injection_state: ChannelInjectionState,
    /// Number of retriggers fired since the last real user message.
    retrigger_count: usize,
    /// Whether a retrigger is pending (debounce window active).
    pending_retrigger: bool,
    /// Metadata for the pending retrigger (e.g. Discord reply target).
    pending_retrigger_metadata: HashMap<String, serde_json::Value>,
    /// Deadline for firing the pending retrigger (debounce timer).
    retrigger_deadline: Option<tokio::time::Instant>,
    /// Background process results waiting to be embedded in the next retrigger.
    /// Accumulated during the debounce window and drained when the retrigger fires.
    pending_results: Vec<PendingResult>,
    /// Optional send_agent_message tool (only when agent has active links).
    send_agent_message_tool: Option<crate::tools::SendAgentMessageTool>,
}

impl Channel {
    /// Create a new channel.
    ///
    /// All tunable config (prompts, routing, thresholds, browser, skills) is read
    /// from `deps.runtime_config` on each use, so changes propagate to running
    /// channels without restart.
    pub fn new(
        id: ChannelId,
        deps: AgentDeps,
        response_tx: mpsc::Sender<OutboundResponse>,
        event_rx: broadcast::Receiver<ProcessEvent>,
        screenshot_dir: std::path::PathBuf,
        logs_dir: std::path::PathBuf,
    ) -> (Self, mpsc::Sender<InboundMessage>) {
        let process_id = ProcessId::Channel(id.clone());
        let hook = SpacebotHook::new(
            deps.agent_id.clone(),
            process_id,
            ProcessType::Channel,
            Some(id.clone()),
            deps.event_tx.clone(),
        );
        let status_block = Arc::new(RwLock::new(StatusBlock::new()));
        let history = Arc::new(RwLock::new(Vec::new()));
        let active_branches = Arc::new(RwLock::new(HashMap::new()));
        let active_workers = Arc::new(RwLock::new(HashMap::new()));
        let (message_tx, message_rx) = mpsc::channel(64);

        let conversation_logger = ConversationLogger::new(deps.sqlite_pool.clone());
        let process_run_logger = ProcessRunLogger::new(deps.sqlite_pool.clone());
        let channel_store = ChannelStore::new(deps.sqlite_pool.clone());

        let compactor = Compactor::new(id.clone(), deps.clone(), history.clone());

        let state = ChannelState {
            channel_id: id.clone(),
            history: history.clone(),
            active_branches: active_branches.clone(),
            active_workers: active_workers.clone(),
            worker_handles: Arc::new(RwLock::new(HashMap::new())),
            worker_inputs: Arc::new(RwLock::new(HashMap::new())),
            status_block: status_block.clone(),
            deps: deps.clone(),
            conversation_logger,
            process_run_logger,
            reply_target_message_id: Arc::new(RwLock::new(None)),
            channel_store: channel_store.clone(),
            screenshot_dir,
            logs_dir,
        };

        // Each channel gets its own isolated tool server to avoid races between
        // concurrent channels sharing per-turn add/remove cycles.
        let tool_server = ToolServer::new().run();

        // Construct the send_agent_message tool if this agent has links.
        let send_agent_message_tool = {
            let has_links =
                !crate::links::links_for_agent(&deps.links.load(), &deps.agent_id).is_empty();
            if has_links {
                Some(crate::tools::SendAgentMessageTool::new(
                    deps.agent_id.clone(),
                    deps.links.clone(),
                    deps.agent_names.clone(),
                ))
            } else {
                None
            }
        };

        let self_tx = message_tx.clone();
        let channel = Self {
            id: id.clone(),
            title: None,
            deps,
            hook,
            state,
            tool_server,
            message_rx,
            event_rx,
            response_tx,
            self_tx,
            conversation_id: None,
            source_adapter: None,
            conversation_context: None,
            compactor,
            message_count: 0,
            memory_persistence_branches: HashSet::new(),
            branch_reply_targets: HashMap::new(),
            coalesce_buffer: Vec::new(),
            coalesce_deadline: None,
            current_turn: 0,
            injection_state: ChannelInjectionState::new(),
            retrigger_count: 0,
            pending_retrigger: false,
            pending_retrigger_metadata: HashMap::new(),
            retrigger_deadline: None,
            pending_results: Vec::new(),
            send_agent_message_tool,
        };

        (channel, message_tx)
    }

    /// Get the agent's display name (falls back to agent ID).
    fn agent_display_name(&self) -> &str {
        self.deps
            .agent_names
            .get(self.deps.agent_id.as_ref())
            .map(String::as_str)
            .unwrap_or(self.deps.agent_id.as_ref())
    }

    fn current_adapter(&self) -> Option<&str> {
        self.source_adapter
            .as_deref()
            .or_else(|| {
                self.conversation_id
                    .as_deref()
                    .and_then(|conversation_id| conversation_id.split(':').next())
            })
            .filter(|adapter| !adapter.is_empty())
    }

    fn suppress_plaintext_fallback(&self) -> bool {
        matches!(self.current_adapter(), Some("email"))
    }

    /// Run the channel event loop.
    pub async fn run(mut self) -> Result<()> {
        let channel_id = self.id.clone();
        tracing::info!(channel_id = %channel_id, "channel started");

        loop {
            // Compute next deadline from coalesce and retrigger timers
            let next_deadline = match (self.coalesce_deadline, self.retrigger_deadline) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (Some(a), None) => Some(a),
                (None, Some(b)) => Some(b),
                (None, None) => None,
            };
            let sleep_duration = next_deadline
                .map(|deadline| {
                    let now = tokio::time::Instant::now();
                    if deadline > now {
                        deadline - now
                    } else {
                        std::time::Duration::from_millis(1)
                    }
                })
                .unwrap_or(std::time::Duration::from_secs(3600)); // Default long timeout if no deadline

            tokio::select! {
                Some(message) = self.message_rx.recv() => {
                    let config = self.deps.runtime_config.coalesce.load();
                    if self.should_coalesce(&message, &config) {
                        self.coalesce_buffer.push(message);
                        self.update_coalesce_deadline(&config).await;
                    } else {
                        // Flush any pending buffer before handling this message
                        if let Err(error) = self.flush_coalesce_buffer().await {
                            tracing::error!(%error, channel_id = %channel_id, "error flushing coalesce buffer");
                        }
                        if let Err(error) = self.handle_message(message).await {
                            tracing::error!(%error, channel_id = %channel_id, "error handling message");
                        }
                    }
                }
                Ok(event) = self.event_rx.recv() => {
                    // Events bypass coalescing - flush buffer first if needed
                    if let Err(error) = self.flush_coalesce_buffer().await {
                        tracing::error!(%error, channel_id = %channel_id, "error flushing coalesce buffer");
                    }
                    if let Err(error) = self.handle_event(event).await {
                        tracing::error!(%error, channel_id = %channel_id, "error handling event");
                    }
                }
                _ = tokio::time::sleep(sleep_duration), if next_deadline.is_some() => {
                    let now = tokio::time::Instant::now();
                    // Check coalesce deadline
                    if self.coalesce_deadline.is_some_and(|d| d <= now)
                        && let Err(error) = self.flush_coalesce_buffer().await
                    {
                        tracing::error!(%error, channel_id = %self.id, "error flushing coalesce buffer on deadline");
                    }
                    // Check retrigger deadline
                    if self.retrigger_deadline.is_some_and(|d| d <= now) {
                        self.flush_pending_retrigger().await;
                    }
                }
                else => break,
            }
        }

        // Flush any remaining buffer before shutting down
        if let Err(error) = self.flush_coalesce_buffer().await {
            tracing::error!(%error, channel_id = %channel_id, "error flushing coalesce buffer on shutdown");
        }

        tracing::info!(channel_id = %channel_id, "channel stopped");
        Ok(())
    }

    /// Determine if a message should be coalesced (batched with other messages).
    ///
    /// Returns false for:
    /// - System re-trigger messages (always process immediately)
    /// - Messages when coalescing is disabled
    /// - Messages in DMs when multi_user_only is true
    fn should_coalesce(
        &self,
        message: &InboundMessage,
        config: &crate::config::CoalesceConfig,
    ) -> bool {
        if !config.enabled {
            return false;
        }
        if message.source == "system" {
            return false;
        }
        if config.multi_user_only && self.is_dm() {
            return false;
        }
        true
    }

    /// Check if this is a DM (direct message) conversation based on conversation_id.
    fn is_dm(&self) -> bool {
        // Check conversation_id pattern for DM indicators
        if let Some(ref conv_id) = self.conversation_id {
            conv_id.contains(":dm:")
                || conv_id.starts_with("discord:dm:")
                || conv_id.starts_with("slack:dm:")
        } else {
            // If no conversation_id set yet, default to not DM (safer)
            false
        }
    }

    /// Update the coalesce deadline based on buffer size and config.
    async fn update_coalesce_deadline(&mut self, config: &crate::config::CoalesceConfig) {
        let now = tokio::time::Instant::now();

        if let Some(first_message) = self.coalesce_buffer.first() {
            let elapsed_since_first =
                chrono::Utc::now().signed_duration_since(first_message.timestamp);
            let elapsed_millis = elapsed_since_first.num_milliseconds().max(0) as u64;

            let max_wait_ms = config.max_wait_ms;
            let debounce_ms = config.debounce_ms;

            // If we have enough messages to trigger coalescing (min_messages threshold)
            if self.coalesce_buffer.len() >= config.min_messages {
                // Cap at max_wait from the first message
                let remaining_wait_ms = max_wait_ms.saturating_sub(elapsed_millis);
                let max_deadline = now + std::time::Duration::from_millis(remaining_wait_ms);

                // If no deadline set yet, use debounce window
                // Otherwise, keep existing deadline (don't extend past max_wait)
                if self.coalesce_deadline.is_none() {
                    let new_deadline = now + std::time::Duration::from_millis(debounce_ms);
                    self.coalesce_deadline = Some(new_deadline.min(max_deadline));
                } else {
                    // Already have a deadline, cap it at max_wait
                    self.coalesce_deadline = self.coalesce_deadline.map(|d| d.min(max_deadline));
                }
            } else {
                // Not enough messages yet - set a short debounce window
                let new_deadline = now + std::time::Duration::from_millis(debounce_ms);
                self.coalesce_deadline = Some(new_deadline);
            }
        }
    }

    /// Flush the coalesce buffer by processing all buffered messages.
    ///
    /// If there's only one message, process it normally.
    /// If there are multiple messages, batch them into a single turn.
    async fn flush_coalesce_buffer(&mut self) -> Result<()> {
        if self.coalesce_buffer.is_empty() {
            return Ok(());
        }

        self.coalesce_deadline = None;

        let messages: Vec<InboundMessage> = std::mem::take(&mut self.coalesce_buffer);

        if messages.len() == 1 {
            // Single message - process normally
            let message = messages
                .into_iter()
                .next()
                .ok_or_else(|| anyhow::anyhow!("empty iterator after length check"))?;
            self.handle_message(message).await
        } else {
            // Multiple messages - batch them
            self.handle_message_batch(messages).await
        }
    }

    /// Handle a batch of messages as a single LLM turn.
    ///
    /// Formats all messages with attribution and timestamps, persists each
    /// individually to conversation history, then presents them as one user turn
    /// with a coalesce hint telling the LLM this is a fast-moving conversation.
    #[tracing::instrument(skip(self, messages), fields(channel_id = %self.id, agent_id = %self.deps.agent_id, message_count = messages.len()))]
    async fn handle_message_batch(&mut self, messages: Vec<InboundMessage>) -> Result<()> {
        let message_count = messages.len();
        let batch_start_timestamp = messages
            .iter()
            .map(|message| message.timestamp)
            .min()
            .unwrap_or_else(chrono::Utc::now);
        let batch_tail_timestamp = messages
            .iter()
            .map(|message| message.timestamp)
            .max()
            .unwrap_or(batch_start_timestamp);
        let elapsed = batch_tail_timestamp.signed_duration_since(batch_start_timestamp);
        let elapsed_secs = elapsed.num_milliseconds() as f64 / 1000.0;

        tracing::info!(message_count, elapsed_secs, "handling batched messages");

        // Count unique senders for the hint
        let unique_senders: std::collections::HashSet<_> =
            messages.iter().map(|m| &m.sender_id).collect();
        let unique_sender_count = unique_senders.len();

        // Track conversation_id from the first message
        if self.conversation_id.is_none()
            && let Some(first) = messages.first()
        {
            self.conversation_id = Some(first.conversation_id.clone());
        }

        if self.source_adapter.is_none()
            && let Some(first) = messages.first()
            && first.source != "system"
        {
            self.source_adapter = Some(first.source.clone());
        }

        // Capture conversation context from the first message
        if self.conversation_context.is_none()
            && let Some(first) = messages.first()
        {
            let prompt_engine = self.deps.runtime_config.prompts.load();
            let server_name = first
                .metadata
                .get("discord_guild_name")
                .and_then(|v| v.as_str())
                .or_else(|| {
                    first
                        .metadata
                        .get("telegram_chat_title")
                        .and_then(|v| v.as_str())
                });
            let channel_name = first
                .metadata
                .get("discord_channel_name")
                .and_then(|v| v.as_str())
                .or_else(|| {
                    first
                        .metadata
                        .get("telegram_chat_type")
                        .and_then(|v| v.as_str())
                });
            self.conversation_context = Some(prompt_engine.render_conversation_context(
                &first.source,
                server_name,
                channel_name,
            )?);
        }

        // Persist each message to conversation log (individual audit trail)
        let mut user_contents: Vec<UserContent> = Vec::new();
        let mut conversation_id = String::new();
        let temporal_context = TemporalContext::from_runtime(self.deps.runtime_config.as_ref());

        for message in &messages {
            if message.source != "system" {
                let sender_name = message
                    .metadata
                    .get("sender_display_name")
                    .and_then(|v| v.as_str())
                    .unwrap_or(&message.sender_id);

                let (raw_text, attachments) = match &message.content {
                    crate::MessageContent::Text(text) => (text.clone(), Vec::new()),
                    crate::MessageContent::Media { text, attachments } => {
                        (text.clone().unwrap_or_default(), attachments.clone())
                    }
                    // Render interactions as their Display form so the LLM sees plain text.
                    crate::MessageContent::Interaction { .. } => {
                        (message.content.to_string(), Vec::new())
                    }
                };

                self.state.conversation_logger.log_user_message(
                    &self.state.channel_id,
                    sender_name,
                    &message.sender_id,
                    &raw_text,
                    &message.metadata,
                );
                self.state
                    .channel_store
                    .upsert(&message.conversation_id, &message.metadata);

                conversation_id = message.conversation_id.clone();

                // Include both absolute and relative time context.
                let relative_secs = batch_tail_timestamp
                    .signed_duration_since(message.timestamp)
                    .num_seconds()
                    .max(0);
                let relative_text = if relative_secs < 1 {
                    "just now".to_string()
                } else if relative_secs < 60 {
                    format!("{}s ago", relative_secs)
                } else {
                    format!("{}m ago", relative_secs / 60)
                };
                let absolute_timestamp = temporal_context.format_timestamp(message.timestamp);

                let display_name = message_display_name(message);

                let formatted_text = format_batched_user_message(
                    display_name,
                    &absolute_timestamp,
                    &relative_text,
                    &raw_text,
                );

                // Download attachments for this message
                if !attachments.is_empty() {
                    let attachment_content = download_attachments(&self.deps, &attachments).await;
                    for content in attachment_content {
                        user_contents.push(content);
                    }
                }

                user_contents.push(UserContent::text(formatted_text));
            }
        }
        // Separate text and non-text (image/audio) content
        let mut text_parts = Vec::new();
        let mut attachment_parts = Vec::new();
        for content in user_contents {
            match content {
                UserContent::Text(t) => text_parts.push(t.text.clone()),
                other => attachment_parts.push(other),
            }
        }

        let combined_text = format!(
            "[{} messages arrived rapidly in this channel]\n\n{}",
            message_count,
            text_parts.join("\n")
        );

        // Build system prompt with coalesce hint
        let system_prompt = self
            .build_system_prompt_with_coalesce(message_count, elapsed_secs, unique_sender_count)
            .await?;

        {
            let mut reply_target = self.state.reply_target_message_id.write().await;
            *reply_target = messages.iter().rev().find_map(extract_discord_message_id);
        }

        // Pre-hook: Compute memory injection on combined text
        let injected_context = self.compute_memory_injection(&combined_text).await;

        // Run agent turn with any image/audio attachments preserved
        let (result, skip_flag, replied_flag) = self
            .run_agent_turn(
                &combined_text,
                &system_prompt,
                &conversation_id,
                attachment_parts,
                injected_context,
                false, // not a retrigger
            )
            .await?;

        self.handle_agent_result(result, &skip_flag, &replied_flag, false)
            .await;
        // Check compaction
        if let Err(error) = self.compactor.check_and_compact().await {
            tracing::warn!(%error, "compaction check failed");
        }

        // Increment message counter for memory persistence
        self.message_count += message_count;
        self.current_turn += 1;
        self.check_memory_persistence().await;

        Ok(())
    }

    /// Build system prompt with coalesce hint for batched messages.
    async fn build_system_prompt_with_coalesce(
        &self,
        message_count: usize,
        elapsed_secs: f64,
        unique_senders: usize,
    ) -> Result<String> {
        let rc = &self.deps.runtime_config;
        let prompt_engine = rc.prompts.load();

        let identity_context = rc.identity.load().render();
        let memory_bulletin = rc.memory_bulletin.load();
        let skills = rc.skills.load();
        let skills_prompt = skills.render_channel_prompt(&prompt_engine)?;

        let browser_enabled = rc.browser_config.load().enabled;
        let web_search_enabled = rc.brave_search_key.load().is_some();
        let opencode_enabled = rc.opencode.load().enabled;
        let worker_capabilities = prompt_engine.render_worker_capabilities(
            browser_enabled,
            web_search_enabled,
            opencode_enabled,
        )?;

        let temporal_context = TemporalContext::from_runtime(rc.as_ref());
        let current_time_line = temporal_context.current_time_line();
        let status_text = {
            let status = self.state.status_block.read().await;
            status.render_with_time_context(Some(&current_time_line))
        };

        // Render coalesce hint
        let elapsed_str = format!("{:.1}s", elapsed_secs);
        let coalesce_hint = prompt_engine
            .render_coalesce_hint(message_count, &elapsed_str, unique_senders)
            .ok();

        let available_channels = self.build_available_channels().await;

        let org_context = self.build_org_context(&prompt_engine);

        let adapter_prompt = self
            .current_adapter()
            .and_then(|adapter| prompt_engine.render_channel_adapter_prompt(adapter));

        let empty_to_none = |s: String| if s.is_empty() { None } else { Some(s) };

        prompt_engine.render_channel_prompt_with_links(
            empty_to_none(identity_context),
            empty_to_none(memory_bulletin.to_string()),
            empty_to_none(skills_prompt),
            worker_capabilities,
            self.conversation_context.clone(),
            empty_to_none(status_text),
            coalesce_hint,
            available_channels,
            org_context,
            adapter_prompt,
        )
    }

    /// Handle an incoming message by running the channel's LLM agent loop.
    ///
    /// The LLM decides which tools to call: reply (to respond), branch (to think),
    /// spawn_worker (to delegate), route (to follow up with a worker), cancel, or
    /// memory_save. The tools act on the channel's shared state directly.
    #[tracing::instrument(skip(self, message), fields(channel_id = %self.id, agent_id = %self.deps.agent_id, message_id = %message.id))]
    async fn handle_message(&mut self, message: InboundMessage) -> Result<()> {
        tracing::info!("handling message");

        // Track conversation_id for synthetic re-trigger messages
        if self.conversation_id.is_none() {
            self.conversation_id = Some(message.conversation_id.clone());
        }

        if self.source_adapter.is_none() && message.source != "system" {
            self.source_adapter = Some(message.source.clone());
        }

        let (raw_text, attachments) = match &message.content {
            crate::MessageContent::Text(text) => (text.clone(), Vec::new()),
            crate::MessageContent::Media { text, attachments } => {
                (text.clone().unwrap_or_default(), attachments.clone())
            }
            // Render interactions as their Display form so the LLM sees plain text.
            crate::MessageContent::Interaction { .. } => (message.content.to_string(), Vec::new()),
        };

        let temporal_context = TemporalContext::from_runtime(self.deps.runtime_config.as_ref());
        let message_timestamp = temporal_context.format_timestamp(message.timestamp);
        let user_text = format_user_message(&raw_text, &message, &message_timestamp);

        let attachment_content = if !attachments.is_empty() {
            download_attachments(&self.deps, &attachments).await
        } else {
            Vec::new()
        };

        // Persist user messages (skip system re-triggers)
        if message.source != "system" {
            let sender_name = message
                .metadata
                .get("sender_display_name")
                .and_then(|v| v.as_str())
                .unwrap_or(&message.sender_id);
            self.state.conversation_logger.log_user_message(
                &self.state.channel_id,
                sender_name,
                &message.sender_id,
                &raw_text,
                &message.metadata,
            );
            self.state
                .channel_store
                .upsert(&message.conversation_id, &message.metadata);
        }

        // Capture conversation context from the first message (platform, channel, server)
        if self.conversation_context.is_none() {
            let prompt_engine = self.deps.runtime_config.prompts.load();
            let server_name = message
                .metadata
                .get("discord_guild_name")
                .and_then(|v| v.as_str())
                .or_else(|| {
                    message
                        .metadata
                        .get("telegram_chat_title")
                        .and_then(|v| v.as_str())
                });
            let channel_name = message
                .metadata
                .get("discord_channel_name")
                .and_then(|v| v.as_str())
                .or_else(|| {
                    message
                        .metadata
                        .get("telegram_chat_type")
                        .and_then(|v| v.as_str())
                });
            self.conversation_context = Some(prompt_engine.render_conversation_context(
                &message.source,
                server_name,
                channel_name,
            )?);
        }

        let system_prompt = self.build_system_prompt().await?;

        {
            let mut reply_target = self.state.reply_target_message_id.write().await;
            *reply_target = extract_discord_message_id(&message);
        }

    let is_retrigger = message.source == "system";

    // Pre-hook: Compute memory injection (skip for system re-triggers)
    let injected_context = if !is_retrigger {
            self.compute_memory_injection(&user_text).await
        } else {
            None
        };

        let (result, skip_flag, replied_flag) = self
            .run_agent_turn(
                &user_text,
                &system_prompt,
                &message.conversation_id,
                attachment_content,
                injected_context,
                is_retrigger,
            )
            .await?;

        self.handle_agent_result(result, &skip_flag, &replied_flag, is_retrigger)
            .await;

        // After a successful retrigger relay, inject a compact record into
        // history so the conversation has context about what was relayed.
        // The retrigger turn itself is rolled back by apply_history_after_turn
        // (PromptCancelled leaves dangling tool calls), so without this the
        // LLM would have no memory of the background result on subsequent turns.
        if is_retrigger && replied_flag.load(std::sync::atomic::Ordering::Relaxed) {
            // Extract the result summaries from the metadata we attached in
            // flush_pending_retrigger, so we record only the substance (not
            // the retrigger instructions/template scaffolding).
            let summary = message
                .metadata
                .get("retrigger_result_summary")
                .and_then(|v| v.as_str())
                .unwrap_or("[background work completed and result relayed to user]");

            let mut history = self.state.history.write().await;
            history.push(rig::message::Message::Assistant {
                id: None,
                content: OneOrMany::one(rig::message::AssistantContent::text(summary)),
            });
        }

        // Check context size and trigger compaction if needed
        if let Err(error) = self.compactor.check_and_compact().await {
            tracing::warn!(%error, "compaction check failed");
        }

        // Increment message counter and spawn memory persistence branch if threshold reached
        if !is_retrigger {
            self.retrigger_count = 0;
            self.message_count += 1;
            self.current_turn += 1;
            self.check_memory_persistence().await;
        }

        Ok(())
    }

    /// Build the rendered available channels fragment for cross-channel awareness.
    async fn build_available_channels(&self) -> Option<String> {
        self.deps.messaging_manager.as_ref()?;

        let channels = match self.state.channel_store.list_active().await {
            Ok(channels) => channels,
            Err(error) => {
                tracing::warn!(%error, "failed to list channels for system prompt");
                return None;
            }
        };

        // Filter out the current channel and cron channels
        let entries: Vec<crate::prompts::engine::ChannelEntry> = channels
            .into_iter()
            .filter(|channel| {
                channel.id.as_str() != self.id.as_ref()
                    && channel.platform != "cron"
                    && channel.platform != "webhook"
            })
            .map(|channel| crate::prompts::engine::ChannelEntry {
                name: channel.display_name.unwrap_or_else(|| channel.id.clone()),
                platform: channel.platform,
                id: channel.id,
            })
            .collect();

        if entries.is_empty() {
            return None;
        }

        let prompt_engine = self.deps.runtime_config.prompts.load();
        prompt_engine.render_available_channels(entries).ok()
    }

    /// Build org context showing the agent's position in the communication hierarchy.
    fn build_org_context(&self, prompt_engine: &crate::prompts::PromptEngine) -> Option<String> {
        let agent_id = self.deps.agent_id.as_ref();
        let all_links = self.deps.links.load();
        let links = crate::links::links_for_agent(&all_links, agent_id);

        if links.is_empty() {
            return None;
        }

        let mut superiors = Vec::new();
        let mut subordinates = Vec::new();
        let mut peers = Vec::new();

        for link in &links {
            let is_from = link.from_agent_id == agent_id;
            let other_id = if is_from {
                &link.to_agent_id
            } else {
                &link.from_agent_id
            };

            let is_human = !self.deps.agent_names.contains_key(other_id.as_str());
            let name = self
                .deps
                .agent_names
                .get(other_id.as_str())
                .cloned()
                .unwrap_or_else(|| other_id.clone());

            let info = crate::prompts::engine::LinkedAgent {
                name,
                id: other_id.clone(),
                is_human,
            };

            match link.kind {
                crate::links::LinkKind::Hierarchical => {
                    // from is above to: if we're `from`, the other is our subordinate
                    if is_from {
                        subordinates.push(info);
                    } else {
                        superiors.push(info);
                    }
                }
                crate::links::LinkKind::Peer => peers.push(info),
            }
        }

        if superiors.is_empty() && subordinates.is_empty() && peers.is_empty() {
            return None;
        }

        let org_context = crate::prompts::engine::OrgContext {
            superiors,
            subordinates,
            peers,
        };

        prompt_engine.render_org_context(org_context).ok()
    }

    /// Assemble the full system prompt using the PromptEngine.
    async fn build_system_prompt(&self) -> crate::error::Result<String> {
        let rc = &self.deps.runtime_config;
        let prompt_engine = rc.prompts.load();

        let identity_context = rc.identity.load().render();
        let memory_bulletin = rc.memory_bulletin.load();
        let skills = rc.skills.load();
        let skills_prompt = skills.render_channel_prompt(&prompt_engine)?;

        let browser_enabled = rc.browser_config.load().enabled;
        let web_search_enabled = rc.brave_search_key.load().is_some();
        let opencode_enabled = rc.opencode.load().enabled;
        let worker_capabilities = prompt_engine.render_worker_capabilities(
            browser_enabled,
            web_search_enabled,
            opencode_enabled,
        )?;

        let temporal_context = TemporalContext::from_runtime(rc.as_ref());
        let current_time_line = temporal_context.current_time_line();
        let status_text = {
            let status = self.state.status_block.read().await;
            status.render_with_time_context(Some(&current_time_line))
        };

        let available_channels = self.build_available_channels().await;

        let org_context = self.build_org_context(&prompt_engine);

        let adapter_prompt = self
            .current_adapter()
            .and_then(|adapter| prompt_engine.render_channel_adapter_prompt(adapter));

        let empty_to_none = |s: String| if s.is_empty() { None } else { Some(s) };

        prompt_engine.render_channel_prompt_with_links(
            empty_to_none(identity_context),
            empty_to_none(memory_bulletin.to_string()),
            empty_to_none(skills_prompt),
            worker_capabilities,
            self.conversation_context.clone(),
            empty_to_none(status_text),
            None, // coalesce_hint - only set for batched messages
            available_channels,
            org_context,
            adapter_prompt,
        )
    }

    /// Compute memories to inject before the LLM turn (pre-hook).
    ///
    /// Pipeline:
    /// 1) Optional pinned-type retrieval (ambient context)
    /// 2) Contextual hybrid search on the user message
    /// 3) Deduplication by context-window ID, batch ID, and semantic similarity
    /// 4) Budget enforcement (pinned first, contextual second)
    /// 5) Structured context formatting for prompt injection
    #[tracing::instrument(skip(self, user_text), fields(channel_id = %self.id))]
    async fn compute_memory_injection(&mut self, user_text: &str) -> Option<String> {
        #[derive(Clone, Copy, Debug, PartialEq)]
        enum InjectionSource {
            Pinned,
            Contextual,
        }

        #[derive(Clone, Debug)]
        struct InjectionCandidate {
            memory: crate::memory::Memory,
            source: InjectionSource,
            /// Which retrieval signals produced this candidate (hybrid search only).
            source_signal: Option<SourceSignal>,
        }

        let parse_memory_type = |value: &str| -> Option<MemoryType> {
            match value {
                "fact" => Some(MemoryType::Fact),
                "preference" => Some(MemoryType::Preference),
                "decision" => Some(MemoryType::Decision),
                "identity" => Some(MemoryType::Identity),
                "event" => Some(MemoryType::Event),
                "observation" => Some(MemoryType::Observation),
                "goal" => Some(MemoryType::Goal),
                "todo" => Some(MemoryType::Todo),
                _ => None,
            }
        };

        let memory_search = self.deps.memory_search();
        let config = self.deps.runtime_config.memory_injection.load();

        if !config.enabled {
            tracing::info!(channel_id = %self.id, "memory injection skipped (disabled)");
            return None;
        }

        let started_at = std::time::Instant::now();

        let context_window_depth = config.context_window_depth;
        let semantic_threshold = config.semantic_threshold;
        let search_limit = config.search_limit;
        let contextual_min_score = config.contextual_min_score;
        let max_total = config.max_total;


        let pinned_sort = if config.pinned_sort == "importance" {
            SearchSort::Importance
        } else {
            SearchSort::Recent
        };

        let pinned_limit = config.pinned_limit;
        let pinned_types = if config.ambient_enabled {
            config.pinned_types.clone()
        } else {
            Vec::new()
        };

        let pinned_tasks = pinned_types
            .iter()
            .cloned()
            .map(|memory_type_name| {
                let memory_search = memory_search.clone();
                async move {
                    let Some(memory_type) = parse_memory_type(&memory_type_name) else {
                        tracing::warn!(memory_type = %memory_type_name, "unknown pinned memory type");
                        return Vec::new();
                    };

                    memory_search
                        .store()
                        .get_sorted(pinned_sort, pinned_limit, Some(memory_type))
                        .await
                        .unwrap_or_else(|error| {
                            tracing::warn!(%error, memory_type = %memory_type_name, "failed pinned type fetch");
                            Vec::new()
                        })
                }
            })
            .collect::<Vec<_>>();

        let search_config = SearchConfig {
            mode: SearchMode::Hybrid,
            max_results: search_limit,
            max_results_per_source: search_limit,
            // Don't filter on RRF score — it measures cross-source consensus,
            // not relevance. Actual relevance filtering happens below via
            // cosine-similarity post-filter using contextual_min_score.
            min_score: 0.0,
            // Disable graph traversal for injection: vector + FTS give precise
            // semantic results; the graph's naive keyword matching floods results
            // with high-importance but irrelevant memories (stop-word matches).
            graph_seed_limit: 0,
            ..Default::default()
        };

        let (pinned_results, contextual_results) = tokio::join!(
            join_all(pinned_tasks),
            memory_search.search(user_text, &search_config)
        );

        let mut all_candidates = pinned_results
            .into_iter()
            .flatten()
            .map(|memory| InjectionCandidate {
                memory,
                source: InjectionSource::Pinned,
                source_signal: None,
            })
            .collect::<Vec<_>>();

        match contextual_results {
            Ok(results) => {
                all_candidates.extend(results.into_iter().map(|result| InjectionCandidate {
                    source_signal: result.source_signal,
                    memory: result.memory,
                    source: InjectionSource::Contextual,
                }));
            }
            Err(error) => {
                tracing::warn!(%error, "failed contextual hybrid pre-hook search");
            }
        }

        let candidate_count = all_candidates.len();

        // Embed the query for cosine-similarity post-filtering.
        // If embedding fails we have no signal to rank candidates, so skip injection
        // entirely rather than letting every memory pass an unconstrained filter.
        let query_embedding = match memory_search
            .embedding_model_arc()
            .embed_one(user_text)
            .await
        {
            Ok(emb) => emb,
            Err(error) => {
                tracing::warn!(
                    %error,
                    channel_id = %self.id,
                    "failed to embed query for memory injection, skipping"
                );
                return None;
            }
        };

        let mut deduped_count = 0usize;
        let mut unique_candidates = Vec::new();
        let mut seen_ids = HashSet::new();

        self.injection_state
            .prune_semantic_buffer(self.current_turn, context_window_depth);

        // === Pass 1: resolve embeddings, compute cosine similarities ===
        struct ScoredCandidate {
            memory: crate::memory::Memory,
            source: InjectionSource,
            embedding: Vec<f32>,
            /// Cosine similarity to the user query (None for pinned).
            cosine: Option<f32>,
            /// Which retrieval signals produced this candidate.
            source_signal: Option<SourceSignal>,
        }

        let mut scored_candidates = Vec::new();
        let mut max_cosine: f32 = 0.0;

        for candidate in all_candidates {
            let memory = candidate.memory;

            if !self.injection_state.should_reinject(
                &memory.id,
                self.current_turn,
                context_window_depth,
            ) {
                deduped_count += 1;
                continue;
            }

            if seen_ids.contains(&memory.id) {
                deduped_count += 1;
                continue;
            }
            seen_ids.insert(memory.id.clone());

            let embedding = match memory_search.embedding_table().get_embedding(&memory.id).await {
                Ok(Some(embedding)) => embedding,
                Ok(None) => {
                    tracing::debug!(memory_id = %memory.id, "embedding not found in LanceDB, computing");
                    match memory_search.embedding_model_arc().embed_one(&memory.content).await {
                        Ok(embedding) => embedding,
                        Err(error) => {
                            tracing::warn!(%error, memory_id = %memory.id, "failed to compute embedding");
                            scored_candidates.push(ScoredCandidate {
                                source_signal: candidate.source_signal,
                                memory,
                                source: candidate.source,
                                embedding: Vec::new(),
                                cosine: None,
                            });
                            continue;
                        }
                    }
                }
                Err(error) => {
                    tracing::warn!(%error, memory_id = %memory.id, "failed to get embedding from LanceDB, computing");
                    match memory_search.embedding_model_arc().embed_one(&memory.content).await {
                        Ok(embedding) => embedding,
                        Err(error) => {
                            tracing::warn!(%error, memory_id = %memory.id, "failed to compute embedding");
                            scored_candidates.push(ScoredCandidate {
                                source_signal: candidate.source_signal,
                                memory,
                                source: candidate.source,
                                embedding: Vec::new(),
                                cosine: None,
                            });
                            continue;
                        }
                    }
                }
            };

            let cosine = if candidate.source == InjectionSource::Contextual {
                Some(cosine_similarity(&query_embedding, &embedding))
            } else {
                None // Pinned — always kept
            };

            if let Some(sim) = cosine {
                max_cosine = max_cosine.max(sim);
            }

            scored_candidates.push(ScoredCandidate {
                source_signal: candidate.source_signal,
                memory,
                source: candidate.source,
                embedding,
                cosine,
            });
        }

        // === Pass 2: relative cosine filter + semantic dedup ===
        // contextual_min_score is a *ratio* (0.0–1.0): a candidate must score
        // at least max_cosine × ratio to be kept. This adapts to message
        // length — long messages compress the cosine spread, so a relative
        // threshold stays meaningful regardless.
        //
        // ABSOLUTE_MIN_COSINE is a hard floor: even when max_cosine is low
        // (generic/off-topic message), irrelevant memories with cosine < 0.60
        // are still discarded. Calibrated for all-MiniLM-L6-v2 (384-dim).
        // TODO: expose as `absolute_min_cosine` in MemoryInjectionConfig.
        const ABSOLUTE_MIN_COSINE: f32 = 0.60;
        let dynamic_threshold = (max_cosine * contextual_min_score).max(ABSOLUTE_MIN_COSINE);

        tracing::debug!(
            channel_id = %self.id,
            max_cosine,
            ratio = contextual_min_score,
            dynamic_threshold,
            scored = scored_candidates.len(),
            "cosine relative threshold"
        );

        // Reset seen_ids for the dedup pass (already used for pass-1 dedup).
        seen_ids.clear();

        for scored in scored_candidates {
            // Differentiated cosine floor by retrieval signal.
            //
            // MiniLM is unreliable for rare proper nouns — a precise FTS/BM25
            // match may have a low cosine even when it is the correct result.
            // We therefore apply a softer floor for FTS-backed candidates.
            // The BM25 top-50% pre-filter in hybrid_search already removed the
            // weakest lexical matches before they reached RRF, so FtsOnly
            // candidates here are already pre-qualified by BM25.
            //
            //   FtsOnly       → 0.45  (absolute floor only — BM25 already qualified)
            //   Both          → 0.50  (absolute floor only — FTS + vector agree)
            //   VectorOnly/∅  → dynamic_threshold (adapts to message, >= 0.60)
            //
            // dynamic_threshold must NOT apply to FTS-backed candidates: BM25
            // already provides a relevance signal, and MiniLM cosine is noisy
            // for proper nouns / short entities that FTS handles well.
            if let Some(sim) = scored.cosine {
                let effective_threshold: f32 = match scored.source_signal {
                    Some(SourceSignal::FtsOnly) => 0.45,
                    Some(SourceSignal::Both) => 0.50,
                    _ => dynamic_threshold, // >= ABSOLUTE_MIN_COSINE (0.60)
                };
                tracing::debug!(
                    memory_id = %scored.memory.id,
                    similarity = sim,
                    effective_threshold,
                    source_signal = ?scored.source_signal,
                    content_preview = %scored.memory.content.chars().take(60).collect::<String>(),
                    "cosine filter check"
                );
                if sim < effective_threshold {
                    deduped_count += 1;
                    continue;
                }
            }

            // Skip semantic dedup if embedding is empty (error fallback).
            if !scored.embedding.is_empty() {
                if is_semantically_duplicate(
                    &scored.embedding,
                    self.injection_state
                        .semantic_buffer
                        .iter()
                        .map(|(buffer_embedding, _)| buffer_embedding),
                    semantic_threshold,
                ) {
                    deduped_count += 1;
                    continue;
                }
                self.injection_state
                    .add_embedding(scored.embedding, self.current_turn);
            }

            self.injection_state.record_injection(scored.memory.id.clone(), self.current_turn);
            unique_candidates.push(InjectionCandidate {
                source_signal: scored.source_signal,
                memory: scored.memory,
                source: scored.source,
            });
        }

        if unique_candidates.is_empty() {
            let elapsed = started_at.elapsed();
            tracing::info!(
                channel_id = %self.id,
                candidates = candidate_count,
                deduped = deduped_count,
                elapsed_ms = elapsed.as_millis() as u64,
                "memory injection skipped (no candidates after dedup)"
            );
            return None;
        }

        let mut pinned_selected = Vec::new();
        let mut contextual_selected = Vec::new();
        for candidate in unique_candidates {
            match candidate.source {
                InjectionSource::Pinned => pinned_selected.push(candidate.memory),
                InjectionSource::Contextual => contextual_selected.push(candidate.memory),
            }
        }

        let mut final_memories = Vec::new();
        for memory in pinned_selected {
            if final_memories.len() >= max_total {
                break;
            }
            final_memories.push((InjectionSource::Pinned, memory));
        }
        for memory in contextual_selected {
            if final_memories.len() >= max_total {
                break;
            }
            final_memories.push((InjectionSource::Contextual, memory));
        }

        if final_memories.is_empty() {
            let elapsed = started_at.elapsed();
            tracing::info!(
                channel_id = %self.id,
                candidates = candidate_count,
                deduped = deduped_count,
                budget = max_total,
                elapsed_ms = elapsed.as_millis() as u64,
                "memory injection skipped (empty after budget)",
            );
            return None;
        }

        let pinned_count = final_memories
            .iter()
            .filter(|(source, _)| matches!(source, InjectionSource::Pinned))
            .count();
        let contextual_count = final_memories
            .iter()
            .filter(|(source, _)| matches!(source, InjectionSource::Contextual))
            .count();

        for (source, memory) in &final_memories {
            tracing::debug!(
                memory_id = %memory.id,
                memory_type = %memory.memory_type,
                source = %match source {
                    InjectionSource::Pinned => "pinned",
                    InjectionSource::Contextual => "contextual",
                },
                "memory injected"
            );
        }

        let mut lines = Vec::new();
        let pinned_lines = final_memories
            .iter()
            .filter_map(|(source, memory)| {
                if matches!(source, InjectionSource::Pinned) {
                    Some(format!("[{}] {}", memory.memory_type, memory.content))
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        let contextual_lines = final_memories
            .iter()
            .filter_map(|(source, memory)| {
                if matches!(source, InjectionSource::Contextual) {
                    Some(format!("[{}] {}", memory.memory_type, memory.content))
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();

        if !pinned_lines.is_empty() {
            lines.push("[Pinned context]".to_string());
            lines.extend(pinned_lines);
        }

        if !contextual_lines.is_empty() {
            if !lines.is_empty() {
                lines.push(String::new());
            }
            lines.push("[Relevant to this message]".to_string());
            lines.extend(contextual_lines);
        }

        let elapsed = started_at.elapsed();
        tracing::info!(
            channel_id = %self.id,
            pinned = pinned_count,
            contextual = contextual_count,
            total = final_memories.len(),
            deduped = deduped_count,
            elapsed_ms = elapsed.as_millis() as u64,
            "memory injection complete"
        );

        Some(lines.join("\n"))
    }

    /// Register per-turn tools, run the LLM agentic loop, and clean up.
    ///
    /// Returns the prompt result and skip flag for the caller to dispatch.
    #[allow(clippy::type_complexity)]
    #[tracing::instrument(skip(self, user_text, system_prompt, attachment_content, injected_context), fields(channel_id = %self.id, agent_id = %self.deps.agent_id))]
    async fn run_agent_turn(
        &self,
        user_text: &str,
        system_prompt: &str,
        conversation_id: &str,
        attachment_content: Vec<UserContent>,
        injected_context: Option<String>,
        is_retrigger: bool,
    ) -> Result<(
        std::result::Result<String, rig::completion::PromptError>,
        crate::tools::SkipFlag,
        crate::tools::RepliedFlag,
    )> {
        let skip_flag = crate::tools::new_skip_flag();
        let replied_flag = crate::tools::new_replied_flag();
        let allow_direct_reply = !self.suppress_plaintext_fallback();

        if let Err(error) = crate::tools::add_channel_tools(
            &self.tool_server,
            self.state.clone(),
            self.response_tx.clone(),
            conversation_id,
            skip_flag.clone(),
            replied_flag.clone(),
            self.deps.cron_tool.clone(),
            self.send_agent_message_tool.clone(),
            allow_direct_reply,
        )
        .await
        {
            tracing::error!(%error, "failed to add channel tools");
            return Err(AgentError::Other(error.into()).into());
        }

        let rc = &self.deps.runtime_config;
        let routing = rc.routing.load();
        let max_turns = **rc.max_turns.load();
        let model_name = routing.resolve(ProcessType::Channel, None);
        let model = SpacebotModel::make(&self.deps.llm_manager, model_name)
            .with_context(&*self.deps.agent_id, "channel")
            .with_routing((**routing).clone());

        let agent = AgentBuilder::new(model)
            .preamble(system_prompt)
            .default_max_turns(max_turns)
            .tool_server_handle(self.tool_server.clone())
            .build();

        let _ = self
            .response_tx
            .send(OutboundResponse::Status(crate::StatusUpdate::Thinking))
            .await;

        // Inject attachments as a user message before the text prompt
        if !attachment_content.is_empty() {
            let mut history = self.state.history.write().await;
            let content = OneOrMany::many(attachment_content).unwrap_or_else(|_| {
                OneOrMany::one(UserContent::text("[attachment processing failed]"))
            });
            history.push(rig::message::Message::User { content });
            drop(history);
        }

        let history_len_before = {
            let mut guard = self.state.history.write().await;

            // Prune stale injection blocks every turn, regardless of whether new memories
            // are injected. Without this, blocks from previous turns accumulate indefinitely
            // when injection returns None (all candidates deduped / below threshold).
            {
                let max_keep = self
                    .deps
                    .runtime_config
                    .memory_injection
                    .load()
                    .max_injected_blocks_in_history;
                prune_old_injection_blocks(&mut guard, max_keep);
            }

            // Inject memory context block into canonical history before cloning.
            // This guarantees the block persists through apply_history_after_turn(),
            // which merges only newly appended entries after `history_len_before`.
            if let Some(ref context) = injected_context {
                // Use a user message format with clear prefix so LLM understands this is context.
                let context_message = format!("{INJECTION_BLOCK_PREFIX}:\n{}", context);
                guard.push(rig::message::Message::from(context_message));
            }

            guard.len()
        };

        // Clone history out so the write lock is released before the agentic loop.
        // The branch tool needs a read lock on history to clone it for the branch,
        // and holding a write lock across the entire agentic loop would deadlock.
        let mut history = {
            let guard = self.state.history.read().await;
            guard.clone()
        };

        let mut result = agent
            .prompt(user_text)
            .with_history(&mut history)
            .with_hook(self.hook.clone())
            .await;

        // If the LLM responded with text that looks like tool call syntax, it failed
        // to use the tool calling API. Inject a correction and retry a couple
        // times so the model can recover by calling `reply` or `skip`.
        const TOOL_SYNTAX_RECOVERY_MAX_ATTEMPTS: usize = 2;
        let mut recovery_attempts = 0;
        while let Ok(ref response) = result {
            if !crate::tools::should_block_user_visible_text(response)
                || recovery_attempts >= TOOL_SYNTAX_RECOVERY_MAX_ATTEMPTS
            {
                break;
            }

            recovery_attempts += 1;
            tracing::warn!(
                channel_id = %self.id,
                attempt = recovery_attempts,
                "LLM emitted blocked structured output, retrying with correction"
            );


            let prompt_engine = self.deps.runtime_config.prompts.load();
            let correction = prompt_engine.render_system_tool_syntax_correction()?;
            result = agent
                .prompt(&correction)
                .with_history(&mut history)
                .with_hook(self.hook.clone())
                .await;
        }

        {
            let mut guard = self.state.history.write().await;
            apply_history_after_turn(
                &result,
                &mut guard,
                history,
                history_len_before,
                &self.id,
                is_retrigger,
            );
        }

        if let Err(error) =
            crate::tools::remove_channel_tools(&self.tool_server, allow_direct_reply).await
        {
            tracing::warn!(%error, "failed to remove channel tools");
        }

        Ok((result, skip_flag, replied_flag))
    }

    /// Dispatch the LLM result: send fallback text, log errors, clean up typing.
    ///
    /// On retrigger turns (`is_retrigger = true`), fallback text is suppressed
    /// unless the LLM called `skip` — in that case, any text the LLM produced
    /// is sent as a fallback to ensure worker/branch results reach the user.
    /// The LLM sometimes incorrectly skips on retrigger turns thinking the
    /// result was "already processed" when the user hasn't seen it yet.
    async fn handle_agent_result(
        &self,
        result: std::result::Result<String, rig::completion::PromptError>,
        skip_flag: &crate::tools::SkipFlag,
        replied_flag: &crate::tools::RepliedFlag,
        is_retrigger: bool,
    ) {
        match result {
            Ok(response) => {
                let skipped = skip_flag.load(std::sync::atomic::Ordering::Relaxed);
                let replied = replied_flag.load(std::sync::atomic::Ordering::Relaxed);
                let suppress_plaintext_fallback = self.suppress_plaintext_fallback();
                let adapter = self.current_adapter().unwrap_or("unknown");

                if skipped && is_retrigger {
                    // The LLM skipped on a retrigger turn. This means a worker
                    // or branch completed but the LLM decided not to relay the
                    // result. If the LLM also produced text, send it as a
                    // fallback since the user hasn't seen the result yet.
                    let text = response.trim();
                    if !text.is_empty() {
                        if crate::tools::should_block_user_visible_text(text) {
                            tracing::warn!(
                                channel_id = %self.id,
                                "blocked retrigger fallback output containing structured or tool syntax"
                            );
                        } else if suppress_plaintext_fallback {
                            tracing::info!(
                                channel_id = %self.id,
                                adapter,
                                "suppressing retrigger plaintext fallback for adapter; explicit reply tool call required"
                            );
                        } else {
                            tracing::info!(
                                channel_id = %self.id,
                                response_len = text.len(),
                                "LLM skipped on retrigger but produced text, sending as fallback"
                            );
                            let extracted = extract_reply_from_tool_syntax(text);
                            let source = self
                                .conversation_id
                                .as_deref()
                                .and_then(|conversation_id| conversation_id.split(':').next())
                                .unwrap_or("unknown");
                            let final_text = crate::tools::reply::normalize_discord_mention_tokens(
                                extracted.as_deref().unwrap_or(text),
                                source,
                            );
                            if !final_text.is_empty() {
                                if extracted.is_some() {
                                    tracing::warn!(channel_id = %self.id, "extracted reply from malformed tool syntax in retrigger fallback");
                                }
                                self.state
                                    .conversation_logger
                                    .log_bot_message(&self.state.channel_id, &final_text);
                                if let Err(error) = self
                                    .response_tx
                                    .send(OutboundResponse::Text(final_text))
                                    .await
                                {
                                    tracing::error!(%error, channel_id = %self.id, "failed to send retrigger fallback reply");
                                }
                            }
                        }
                    } else {
                        tracing::warn!(
                            channel_id = %self.id,
                            "LLM skipped on retrigger with no text — worker/branch result may not have been relayed"
                        );
                    }
                } else if skipped {
                    tracing::debug!(channel_id = %self.id, "channel turn skipped (no response)");
                } else if replied {
                    tracing::debug!(channel_id = %self.id, "channel turn replied via tool (fallback suppressed)");
                } else if is_retrigger {
                    // On retrigger turns the LLM should use the reply tool, but
                    // some models return the result as raw text instead. Send it
                    // as a fallback so the user still gets the worker/branch output.
                    let text = response.trim();
                    if !text.is_empty() {
                        if crate::tools::should_block_user_visible_text(text) {
                            tracing::warn!(
                                channel_id = %self.id,
                                "blocked retrigger output containing structured or tool syntax"
                            );
                        } else if suppress_plaintext_fallback {
                            tracing::info!(
                                channel_id = %self.id,
                                adapter,
                                "suppressing retrigger plaintext output for adapter; explicit reply tool call required"
                            );
                        } else {
                            tracing::info!(
                                channel_id = %self.id,
                                response_len = text.len(),
                                "retrigger produced text without reply tool, sending as fallback"
                            );
                            let extracted = extract_reply_from_tool_syntax(text);
                            let source = self
                                .conversation_id
                                .as_deref()
                                .and_then(|conversation_id| conversation_id.split(':').next())
                                .unwrap_or("unknown");
                            let final_text = crate::tools::reply::normalize_discord_mention_tokens(
                                extracted.as_deref().unwrap_or(text),
                                source,
                            );
                            if !final_text.is_empty() {
                                self.state
                                    .conversation_logger
                                    .log_bot_message(&self.state.channel_id, &final_text);
                                if let Err(error) = self
                                    .response_tx
                                    .send(OutboundResponse::Text(final_text))
                                    .await
                                {
                                    tracing::error!(%error, channel_id = %self.id, "failed to send retrigger fallback reply");
                                }
                            }
                        }
                    } else {
                        tracing::debug!(
                            channel_id = %self.id,
                            "retrigger turn produced no text and no reply tool call"
                        );
                    }
                } else {
                    // If the LLM returned text without using the reply tool, send it
                    // directly. Some models respond with text instead of tool calls.
                    // When the text looks like tool call syntax (e.g. "[reply]\n{\"content\": \"hi\"}"),
                    // attempt to extract the reply content and send that instead.
                    let text = response.trim();
                    if crate::tools::should_block_user_visible_text(text) {
                        tracing::warn!(
                            channel_id = %self.id,
                            "blocked fallback output containing structured or tool syntax"
                        );
                    } else if suppress_plaintext_fallback {
                        tracing::info!(
                            channel_id = %self.id,
                            adapter,
                            "suppressing plaintext fallback for adapter; explicit reply tool call required"
                        );
                    } else {
                        let extracted = extract_reply_from_tool_syntax(text);
                        let source = self
                            .conversation_id
                            .as_deref()
                            .and_then(|conversation_id| conversation_id.split(':').next())
                            .unwrap_or("unknown");
                        let final_text = crate::tools::reply::normalize_discord_mention_tokens(
                            extracted.as_deref().unwrap_or(text),
                            source,
                        );
                        if !final_text.is_empty() {
                            if extracted.is_some() {
                                tracing::warn!(channel_id = %self.id, "extracted reply from malformed tool syntax in LLM text output");
                            }
                            self.state.conversation_logger.log_bot_message_with_name(
                                &self.state.channel_id,
                                &final_text,
                                Some(self.agent_display_name()),
                            );
                            if let Err(error) = self
                                .response_tx
                                .send(OutboundResponse::Text(final_text))
                                .await
                            {
                                tracing::error!(%error, channel_id = %self.id, "failed to send fallback reply");
                            }
                        }
                    }

                    tracing::debug!("channel turn completed");
                }
            }
            Err(rig::completion::PromptError::MaxTurnsError { .. }) => {
                tracing::warn!("channel hit max turns");
            }
            Err(rig::completion::PromptError::PromptCancelled { reason, .. }) => {
                if reason == "reply delivered" {
                    tracing::debug!("channel turn completed via reply tool");
                } else {
                    tracing::info!(%reason, "channel turn cancelled");
                }
            }
            Err(error) => {
                tracing::error!(%error, "channel LLM call failed");
            }
        }

        // Ensure typing indicator is always cleaned up, even on error paths
        let _ = self
            .response_tx
            .send(OutboundResponse::Status(crate::StatusUpdate::StopTyping))
            .await;
    }

    /// Handle a process event (branch results, worker completions, status updates).
    async fn handle_event(&mut self, event: ProcessEvent) -> Result<()> {
        // Only process events targeted at this channel
        if !event_is_for_channel(&event, &self.id) {
            return Ok(());
        }

        // Update status block
        {
            let mut status = self.state.status_block.write().await;
            status.update(&event);
        }

        let mut should_retrigger = false;
        let mut retrigger_metadata = std::collections::HashMap::new();
        let run_logger = &self.state.process_run_logger;

        match &event {
            ProcessEvent::BranchStarted {
                branch_id,
                channel_id,
                description,
                reply_to_message_id,
                ..
            } => {
                run_logger.log_branch_started(channel_id, *branch_id, description);
                if let Some(message_id) = reply_to_message_id {
                    self.branch_reply_targets.insert(*branch_id, *message_id);
                }
            }
            ProcessEvent::BranchResult {
                branch_id,
                conclusion,
                ..
            } => {
                run_logger.log_branch_completed(*branch_id, conclusion);

                // Remove from active branches
                let mut branches = self.state.active_branches.write().await;
                branches.remove(branch_id);

                #[cfg(feature = "metrics")]
                crate::telemetry::Metrics::global()
                    .active_branches
                    .with_label_values(&[&*self.deps.agent_id])
                    .dec();

                // Memory persistence branches complete silently — no history
                // injection, no re-trigger. The work (memory saves) already
                // happened inside the branch via tool calls.
                if self.memory_persistence_branches.remove(branch_id) {
                    self.branch_reply_targets.remove(branch_id);
                    tracing::info!(branch_id = %branch_id, "memory persistence branch completed");
                } else {
                    // Regular branch: accumulate result for the next retrigger.
                    // The result text will be embedded directly in the retrigger
                    // message so the LLM knows exactly which process produced it.
                    self.pending_results.push(PendingResult {
                        process_type: "branch",
                        process_id: branch_id.to_string(),
                        result: conclusion.clone(),
                        success: true,
                    });
                    should_retrigger = true;

                    if let Some(message_id) = self.branch_reply_targets.remove(branch_id) {
                        retrigger_metadata.insert(
                            "discord_reply_to_message_id".to_string(),
                            serde_json::Value::from(message_id),
                        );
                    }

                    tracing::info!(branch_id = %branch_id, "branch result queued for retrigger");
                }
            }
            ProcessEvent::WorkerStarted {
                worker_id,
                channel_id,
                task,
                worker_type,
                ..
            } => {
                run_logger.log_worker_started(
                    channel_id.as_ref(),
                    *worker_id,
                    task,
                    worker_type,
                    &self.deps.agent_id,
                );
            }
            ProcessEvent::WorkerStatus {
                worker_id, status, ..
            } => {
                run_logger.log_worker_status(*worker_id, status);
            }
            ProcessEvent::WorkerComplete {
                worker_id,
                result,
                notify,
                success,
                ..
            } => {
                run_logger.log_worker_completed(*worker_id, result, *success);

                let mut workers = self.state.active_workers.write().await;
                workers.remove(worker_id);
                drop(workers);

                self.state.worker_handles.write().await.remove(worker_id);
                self.state.worker_inputs.write().await.remove(worker_id);

                if *notify {
                    // Accumulate result for the next retrigger instead of
                    // injecting into history as a fake user message.
                    self.pending_results.push(PendingResult {
                        process_type: "worker",
                        process_id: worker_id.to_string(),
                        result: result.clone(),
                        success: *success,
                    });
                    should_retrigger = true;
                }

                tracing::info!(worker_id = %worker_id, "worker completed, result queued for retrigger");
            }
            _ => {}
        }

        // Debounce retriggers: instead of firing immediately, set a deadline.
        // Multiple branch/worker completions within the debounce window are
        // coalesced into a single retrigger to prevent message spam.
        if should_retrigger {
            if self.retrigger_count >= MAX_RETRIGGERS_PER_TURN {
                tracing::warn!(
                    channel_id = %self.id,
                    retrigger_count = self.retrigger_count,
                    max = MAX_RETRIGGERS_PER_TURN,
                    "retrigger cap reached, suppressing further retriggers until next user message"
                );
                // Drain any pending results into history as assistant messages
                // so they aren't silently lost when the cap prevents a retrigger.
                if !self.pending_results.is_empty() {
                    let results = std::mem::take(&mut self.pending_results);
                    let mut history = self.state.history.write().await;
                    for r in &results {
                        let status = if r.success { "completed" } else { "failed" };
                        let summary = format!(
                            "[Background {} {} {}]: {}",
                            r.process_type, r.process_id, status, r.result
                        );
                        history.push(rig::message::Message::Assistant {
                            id: None,
                            content: OneOrMany::one(rig::message::AssistantContent::text(summary)),
                        });
                    }
                    tracing::info!(
                        channel_id = %self.id,
                        count = results.len(),
                        "injected capped results into history as assistant messages"
                    );
                }
            } else {
                self.pending_retrigger = true;
                // Merge metadata (later events override earlier ones for the same key)
                for (key, value) in retrigger_metadata {
                    self.pending_retrigger_metadata.insert(key, value);
                }
                self.retrigger_deadline = Some(
                    tokio::time::Instant::now()
                        + std::time::Duration::from_millis(RETRIGGER_DEBOUNCE_MS),
                );
            }
        }

        Ok(())
    }

    /// Flush the pending retrigger: send a synthetic system message to re-trigger
    /// the channel LLM so it can process background results and respond.
    ///
    /// Drains `pending_results` and embeds them directly in the retrigger message
    /// so the LLM sees exactly which process(es) completed and what they returned.
    /// No result text is left floating in history as an ambiguous user message.
    ///
    /// Results are drained only after the synthetic message is queued
    /// successfully. On transient failures, retrigger state is kept and retried
    /// so background results are not silently lost.
    async fn flush_pending_retrigger(&mut self) {
        self.retrigger_deadline = None;

        if !self.pending_retrigger {
            return;
        }

        let Some(conversation_id) = &self.conversation_id else {
            tracing::warn!(
                channel_id = %self.id,
                "retrigger pending but conversation_id is missing, dropping pending results"
            );
            self.pending_retrigger = false;
            self.pending_retrigger_metadata.clear();
            self.pending_results.clear();
            return;
        };

        if self.pending_results.is_empty() {
            tracing::warn!(
                channel_id = %self.id,
                "retrigger fired but no pending results to relay"
            );
            self.pending_retrigger = false;
            self.pending_retrigger_metadata.clear();
            return;
        }

        let result_count = self.pending_results.len();

        // Build per-result summaries for the template.
        let result_items: Vec<_> = self
            .pending_results
            .iter()
            .map(|r| crate::prompts::engine::RetriggerResult {
                process_type: r.process_type.to_string(),
                process_id: r.process_id.clone(),
                success: r.success,
                result: r.result.clone(),
            })
            .collect();

        let retrigger_message = match self
            .deps
            .runtime_config
            .prompts
            .load()
            .render_system_retrigger(&result_items)
        {
            Ok(message) => message,
            Err(error) => {
                tracing::error!(
                    channel_id = %self.id,
                    %error,
                    "failed to render retrigger message, retrying"
                );
                self.retrigger_deadline = Some(
                    tokio::time::Instant::now()
                        + std::time::Duration::from_millis(RETRIGGER_DEBOUNCE_MS),
                );
                return;
            }
        };

        // Build a compact summary of the results to inject into history after
        // a successful relay. This goes into metadata so handle_message can
        // pull it out without re-parsing the template.
        let result_summary = self
            .pending_results
            .iter()
            .map(|r| {
                let status = if r.success { "completed" } else { "failed" };
                // Truncate very long results for the history record — the user
                // already saw the full version via the reply tool.
                let truncated = if r.result.len() > 500 {
                    let boundary = r.result.floor_char_boundary(500);
                    format!("{}... [truncated]", &r.result[..boundary])
                } else {
                    r.result.clone()
                };
                format!(
                    "[{} {} {}]: {}",
                    r.process_type, r.process_id, status, truncated
                )
            })
            .collect::<Vec<_>>()
            .join("\n");

        let mut metadata = self.pending_retrigger_metadata.clone();
        metadata.insert(
            "retrigger_result_summary".to_string(),
            serde_json::Value::String(result_summary),
        );

        let synthetic = InboundMessage {
            id: uuid::Uuid::new_v4().to_string(),
            source: "system".into(),
            adapter: None,
            conversation_id: conversation_id.clone(),
            sender_id: "system".into(),
            agent_id: None,
            content: crate::MessageContent::Text(retrigger_message),
            timestamp: chrono::Utc::now(),
            metadata,
            formatted_author: None,
        };
        match self.self_tx.try_send(synthetic) {
            Ok(()) => {
                self.retrigger_count += 1;
                tracing::info!(
                    channel_id = %self.id,
                    retrigger_count = self.retrigger_count,
                    result_count,
                    "firing debounced retrigger with {} result(s)",
                    result_count,
                );

                self.pending_retrigger = false;
                self.pending_retrigger_metadata.clear();
                self.pending_results.clear();
            }
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                tracing::warn!(
                    channel_id = %self.id,
                    result_count,
                    "channel self queue is full, retrying retrigger"
                );
                self.retrigger_deadline = Some(
                    tokio::time::Instant::now()
                        + std::time::Duration::from_millis(RETRIGGER_DEBOUNCE_MS),
                );
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                tracing::warn!(
                    channel_id = %self.id,
                    "failed to re-trigger channel: queue is closed, dropping pending results"
                );
                self.pending_retrigger = false;
                self.pending_retrigger_metadata.clear();
                self.pending_results.clear();
            }
        }
    }

    /// Get the current status block as a string.
    pub async fn get_status(&self) -> String {
        let temporal_context = TemporalContext::from_runtime(self.deps.runtime_config.as_ref());
        let current_time_line = temporal_context.current_time_line();
        let status = self.state.status_block.read().await;
        status.render_with_time_context(Some(&current_time_line))
    }

    /// Check if a memory persistence branch should be spawned based on message count.
    async fn check_memory_persistence(&mut self) {
        let config = **self.deps.runtime_config.memory_persistence.load();
        if !config.enabled || config.message_interval == 0 {
            return;
        }

        if self.message_count < config.message_interval {
            return;
        }

        // Reset counter before spawning so subsequent messages don't pile up
        self.message_count = 0;

        match spawn_memory_persistence_branch(&self.state, &self.deps).await {
            Ok(branch_id) => {
                self.memory_persistence_branches.insert(branch_id);
                tracing::info!(
                    channel_id = %self.id,
                    branch_id = %branch_id,
                    interval = config.message_interval,
                    "memory persistence branch spawned"
                );
            }
            Err(error) => {
                tracing::warn!(
                    channel_id = %self.id,
                    %error,
                    "failed to spawn memory persistence branch"
                );
            }
        }
    }
}

/// Spawn a branch from a ChannelState. Used by the BranchTool.
pub async fn spawn_branch_from_state(
    state: &ChannelState,
    description: impl Into<String>,
) -> std::result::Result<BranchId, AgentError> {
    let description = description.into();
    let rc = &state.deps.runtime_config;
    let prompt_engine = rc.prompts.load();
    let system_prompt = prompt_engine
        .render_branch_prompt(
            &rc.instance_dir.display().to_string(),
            &rc.workspace_dir.display().to_string(),
        )
        .map_err(|e| AgentError::Other(anyhow::anyhow!("{e}")))?;

    spawn_branch(
        state,
        &description,
        &description,
        &system_prompt,
        &description,
        "branch",
    )
    .await
}

/// Spawn a silent memory persistence branch.
///
/// Uses the same branching infrastructure as regular branches but with a
/// dedicated prompt focused on memory recall + save. The result is not injected
/// into channel history — the channel handles these branch IDs specially.
async fn spawn_memory_persistence_branch(
    state: &ChannelState,
    deps: &AgentDeps,
) -> std::result::Result<BranchId, AgentError> {
    let prompt_engine = deps.runtime_config.prompts.load();
    let system_prompt = prompt_engine
        .render_static("memory_persistence")
        .map_err(|e| AgentError::Other(anyhow::anyhow!("{e}")))?;
    let prompt = prompt_engine
        .render_system_memory_persistence()
        .map_err(|e| AgentError::Other(anyhow::anyhow!("{e}")))?;

    spawn_branch(
        state,
        "memory persistence",
        &prompt,
        &system_prompt,
        "persisting memories...",
        "memory_persistence_branch",
    )
    .await
}

fn ensure_dispatch_readiness(state: &ChannelState, dispatch_type: &'static str) {
    let readiness = state.deps.runtime_config.work_readiness();
    if readiness.ready {
        return;
    }

    let reason = readiness
        .reason
        .map(|value| value.as_str())
        .unwrap_or("unknown");
    tracing::warn!(
        agent_id = %state.deps.agent_id,
        channel_id = %state.channel_id,
        dispatch_type,
        reason,
        warmup_state = ?readiness.warmup_state,
        embedding_ready = readiness.embedding_ready,
        bulletin_age_secs = ?readiness.bulletin_age_secs,
        stale_after_secs = readiness.stale_after_secs,
        "dispatch requested before readiness contract was satisfied"
    );

    #[cfg(feature = "metrics")]
    crate::telemetry::Metrics::global()
        .dispatch_while_cold_count
        .with_label_values(&[&*state.deps.agent_id, dispatch_type, reason])
        .inc();

    let warmup_config = **state.deps.runtime_config.warmup.load();
    let should_trigger = readiness.warmup_state != crate::config::WarmupState::Warming
        && (readiness.reason != Some(crate::config::WorkReadinessReason::EmbeddingNotReady)
            || warmup_config.eager_embedding_load);

    if should_trigger {
        crate::agent::cortex::trigger_forced_warmup(state.deps.clone(), dispatch_type);
    }
}

/// Shared branch spawning logic.
///
/// Checks the branch limit, clones history, creates a Branch, spawns it as
/// a tokio task, and registers it in the channel's active branches and status block.
async fn spawn_branch(
    state: &ChannelState,
    description: &str,
    prompt: &str,
    system_prompt: &str,
    status_label: &str,
    dispatch_type: &'static str,
) -> std::result::Result<BranchId, AgentError> {
    let max_branches = **state.deps.runtime_config.max_concurrent_branches.load();
    {
        let branches = state.active_branches.read().await;
        if branches.len() >= max_branches {
            return Err(AgentError::BranchLimitReached {
                channel_id: state.channel_id.to_string(),
                max: max_branches,
            });
        }
    }
    ensure_dispatch_readiness(state, dispatch_type);

    let history = {
        let h = state.history.read().await;
        h.clone()
    };

    let tool_server = crate::tools::create_branch_tool_server(
        Some(state.clone()),
        state.deps.agent_id.clone(),
        state.deps.task_store.clone(),
        state.deps.memory_search.clone(),
        state.deps.runtime_config.clone(),
        state.conversation_logger.clone(),
        state.channel_store.clone(),
        crate::conversation::ProcessRunLogger::new(state.deps.sqlite_pool.clone()),
    );
    let branch_max_turns = **state.deps.runtime_config.branch_max_turns.load();

    let branch = Branch::new(
        state.channel_id.clone(),
        description,
        state.deps.clone(),
        system_prompt,
        history,
        tool_server,
        branch_max_turns,
    );

    let branch_id = branch.id;
    let prompt = prompt.to_owned();

    let branch_span = tracing::info_span!(
        "branch.run",
        branch_id = %branch_id,
        channel_id = %state.channel_id,
        description = %description,
    );
    let handle = tokio::spawn(
        async move {
            if let Err(error) = branch.run(&prompt).await {
                tracing::error!(branch_id = %branch_id, %error, "branch failed");
            }
        }
        .instrument(branch_span),
    );

    {
        let mut branches = state.active_branches.write().await;
        branches.insert(branch_id, handle);
    }

    {
        let mut status = state.status_block.write().await;
        status.add_branch(branch_id, status_label);
    }

    #[cfg(feature = "metrics")]
    crate::telemetry::Metrics::global()
        .active_branches
        .with_label_values(&[&*state.deps.agent_id])
        .inc();

    state
        .deps
        .event_tx
        .send(crate::ProcessEvent::BranchStarted {
            agent_id: state.deps.agent_id.clone(),
            branch_id,
            channel_id: state.channel_id.clone(),
            description: status_label.to_string(),
            reply_to_message_id: *state.reply_target_message_id.read().await,
        })
        .ok();

    tracing::info!(branch_id = %branch_id, description = %status_label, "branch spawned");

    Ok(branch_id)
}

/// Check whether the channel has capacity for another worker.
async fn check_worker_limit(state: &ChannelState) -> std::result::Result<(), AgentError> {
    let max_workers = **state.deps.runtime_config.max_concurrent_workers.load();
    let workers = state.active_workers.read().await;
    if workers.len() >= max_workers {
        return Err(AgentError::WorkerLimitReached {
            channel_id: state.channel_id.to_string(),
            max: max_workers,
        });
    }
    Ok(())
}

/// Spawn a worker from a ChannelState. Used by the SpawnWorkerTool.
pub async fn spawn_worker_from_state(
    state: &ChannelState,
    task: impl Into<String>,
    interactive: bool,
    suggested_skills: &[&str],
) -> std::result::Result<WorkerId, AgentError> {
    check_worker_limit(state).await?;
    ensure_dispatch_readiness(state, "worker");
    let task = task.into();

    let rc = &state.deps.runtime_config;
    let prompt_engine = rc.prompts.load();
    let temporal_context = TemporalContext::from_runtime(rc.as_ref());
    let worker_task =
        build_worker_task_with_temporal_context(&task, &temporal_context, &prompt_engine)
            .map_err(|error| AgentError::Other(anyhow::anyhow!("{error}")))?;
    let worker_system_prompt = prompt_engine
        .render_worker_prompt(
            &rc.instance_dir.display().to_string(),
            &rc.workspace_dir.display().to_string(),
        )
        .map_err(|e| AgentError::Other(anyhow::anyhow!("{e}")))?;
    let skills = rc.skills.load();
    let browser_config = (**rc.browser_config.load()).clone();
    let brave_search_key = (**rc.brave_search_key.load()).clone();

    // Append skills listing to worker system prompt. Suggested skills are
    // flagged so the worker knows the channel's intent, but it can read any
    // skill it decides is relevant via the read_skill tool.
    let system_prompt = match skills.render_worker_skills(suggested_skills, &prompt_engine) {
        Ok(skills_prompt) if !skills_prompt.is_empty() => {
            format!("{worker_system_prompt}\n\n{skills_prompt}")
        }
        Ok(_) => worker_system_prompt,
        Err(error) => {
            tracing::warn!(%error, "failed to render worker skills listing, spawning without skills context");
            worker_system_prompt
        }
    };

    let worker = if interactive {
        let (worker, input_tx) = Worker::new_interactive(
            Some(state.channel_id.clone()),
            &worker_task,
            &system_prompt,
            state.deps.clone(),
            browser_config.clone(),
            state.screenshot_dir.clone(),
            brave_search_key.clone(),
            state.logs_dir.clone(),
        );
        let worker_id = worker.id;
        state
            .worker_inputs
            .write()
            .await
            .insert(worker_id, input_tx);
        worker
    } else {
        Worker::new(
            Some(state.channel_id.clone()),
            &worker_task,
            &system_prompt,
            state.deps.clone(),
            browser_config,
            state.screenshot_dir.clone(),
            brave_search_key,
            state.logs_dir.clone(),
        )
    };

    let worker_id = worker.id;

    let worker_span = tracing::info_span!(
        "worker.run",
        worker_id = %worker_id,
        channel_id = %state.channel_id,
    );
    let handle = spawn_worker_task(
        worker_id,
        state.deps.event_tx.clone(),
        state.deps.agent_id.clone(),
        Some(state.channel_id.clone()),
        worker.run().instrument(worker_span),
    );

    state.worker_handles.write().await.insert(worker_id, handle);

    {
        let mut status = state.status_block.write().await;
        status.add_worker(worker_id, &task, false);
    }

    state
        .deps
        .event_tx
        .send(crate::ProcessEvent::WorkerStarted {
            agent_id: state.deps.agent_id.clone(),
            worker_id,
            channel_id: Some(state.channel_id.clone()),
            task: task.clone(),
            worker_type: "builtin".into(),
        })
        .ok();

    tracing::info!(worker_id = %worker_id, "worker spawned");

    Ok(worker_id)
}

/// Spawn an OpenCode-backed worker for coding tasks.
///
/// Instead of a Rig agent loop, this spawns an OpenCode subprocess that has its
/// own codebase exploration, context management, and tool suite. The worker
/// communicates with OpenCode via HTTP + SSE.
pub async fn spawn_opencode_worker_from_state(
    state: &ChannelState,
    task: impl Into<String>,
    directory: &str,
    interactive: bool,
) -> std::result::Result<crate::WorkerId, AgentError> {
    check_worker_limit(state).await?;
    ensure_dispatch_readiness(state, "opencode_worker");
    let task = task.into();
    let directory = std::path::PathBuf::from(directory);

    let rc = &state.deps.runtime_config;
    let prompt_engine = rc.prompts.load();
    let temporal_context = TemporalContext::from_runtime(rc.as_ref());
    let worker_task =
        build_worker_task_with_temporal_context(&task, &temporal_context, &prompt_engine)
            .map_err(|error| AgentError::Other(anyhow::anyhow!("{error}")))?;
    let opencode_config = rc.opencode.load();

    if !opencode_config.enabled {
        return Err(AgentError::Other(anyhow::anyhow!(
            "OpenCode workers are not enabled in config"
        )));
    }

    let server_pool = rc.opencode_server_pool.clone();

    let worker = if interactive {
        let (worker, input_tx) = crate::opencode::OpenCodeWorker::new_interactive(
            Some(state.channel_id.clone()),
            state.deps.agent_id.clone(),
            &worker_task,
            directory,
            server_pool,
            state.deps.event_tx.clone(),
        );
        let worker_id = worker.id;
        state
            .worker_inputs
            .write()
            .await
            .insert(worker_id, input_tx);
        worker
    } else {
        crate::opencode::OpenCodeWorker::new(
            Some(state.channel_id.clone()),
            state.deps.agent_id.clone(),
            &worker_task,
            directory,
            server_pool,
            state.deps.event_tx.clone(),
        )
    };

    let worker_id = worker.id;

    let worker_span = tracing::info_span!(
        "worker.run",
        worker_id = %worker_id,
        channel_id = %state.channel_id,
        worker_type = "opencode",
    );
    let handle = spawn_worker_task(
        worker_id,
        state.deps.event_tx.clone(),
        state.deps.agent_id.clone(),
        Some(state.channel_id.clone()),
        async move {
            let result = worker.run().await?;
            Ok::<String, anyhow::Error>(result.result_text)
        }
        .instrument(worker_span),
    );

    state.worker_handles.write().await.insert(worker_id, handle);

    let opencode_task = format!("[opencode] {task}");
    {
        let mut status = state.status_block.write().await;
        status.add_worker(worker_id, &opencode_task, false);
    }

    state
        .deps
        .event_tx
        .send(crate::ProcessEvent::WorkerStarted {
            agent_id: state.deps.agent_id.clone(),
            worker_id,
            channel_id: Some(state.channel_id.clone()),
            task: opencode_task,
            worker_type: "opencode".into(),
        })
        .ok();

    tracing::info!(worker_id = %worker_id, "OpenCode worker spawned");

    Ok(worker_id)
}

/// Spawn a future as a tokio task that sends a `WorkerComplete` event on completion.
///
/// Handles both success and error cases, logging failures and sending the
/// appropriate event. Used by both builtin workers and OpenCode workers.
/// Returns the JoinHandle so the caller can store it for cancellation.
fn spawn_worker_task<F, E>(
    worker_id: WorkerId,
    event_tx: broadcast::Sender<ProcessEvent>,
    agent_id: crate::AgentId,
    channel_id: Option<ChannelId>,
    future: F,
) -> tokio::task::JoinHandle<()>
where
    F: std::future::Future<Output = std::result::Result<String, E>> + Send + 'static,
    E: std::fmt::Display + Send + 'static,
{
    tokio::spawn(async move {
        #[cfg(feature = "metrics")]
        let worker_start = std::time::Instant::now();

        #[cfg(feature = "metrics")]
        crate::telemetry::Metrics::global()
            .active_workers
            .with_label_values(&[&*agent_id])
            .inc();

        let (result_text, notify, success) = match future.await {
            Ok(text) => (text, true, true),
            Err(error) => {
                tracing::error!(worker_id = %worker_id, %error, "worker failed");
                (format!("Worker failed: {error}"), true, false)
            }
        };
        #[cfg(feature = "metrics")]
        {
            let metrics = crate::telemetry::Metrics::global();
            metrics
                .active_workers
                .with_label_values(&[&*agent_id])
                .dec();
            metrics
                .worker_duration_seconds
                .with_label_values(&[&*agent_id, "builtin"])
                .observe(worker_start.elapsed().as_secs_f64());
        }

        let _ = event_tx.send(ProcessEvent::WorkerComplete {
            agent_id,
            worker_id,
            channel_id,
            result: result_text,
            notify,
            success,
        });
    })
}

/// Some models emit tool call syntax as plain text instead of making actual tool calls.
/// When the text starts with a tool-like prefix (e.g. `[reply]`, `(reply)`), try to
/// extract the reply content so we can send it cleanly instead of showing raw JSON.
/// Returns `None` if the text doesn't match or can't be parsed — the caller falls
/// back to sending the original text as-is.
fn extract_reply_from_tool_syntax(text: &str) -> Option<String> {
    // Match patterns like "[reply]\n{...}" or "(reply)\n{...}" (with optional whitespace)
    let tool_prefixes = [
        "[reply]",
        "(reply)",
        "[react]",
        "(react)",
        "[skip]",
        "(skip)",
        "[branch]",
        "(branch)",
        "[spawn_worker]",
        "(spawn_worker)",
        "[route]",
        "(route)",
        "[cancel]",
        "(cancel)",
    ];

    let lower = text.to_lowercase();
    let matched_prefix = tool_prefixes.iter().find(|p| lower.starts_with(*p))?;
    let is_reply = matched_prefix.contains("reply");
    let is_skip = matched_prefix.contains("skip");

    // For skip, just return empty — the user shouldn't see anything
    if is_skip {
        return Some(String::new());
    }

    // For non-reply tools (react, branch, etc.), suppress entirely
    if !is_reply {
        return Some(String::new());
    }

    // Try to extract "content" from the JSON payload after the prefix
    let rest = text[matched_prefix.len()..].trim();
    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(rest)
        && let Some(content) = parsed.get("content").and_then(|v| v.as_str())
    {
        return Some(content.to_string());
    }

    // If we can't parse JSON, the rest might just be the message itself (no JSON wrapper)
    if !rest.is_empty() && !rest.starts_with('{') {
        return Some(rest.to_string());
    }

    None
}

/// Format a user message with sender attribution from message metadata.
///
/// In multi-user channels, this lets the LLM distinguish who said what.
/// System-generated messages (re-triggers) are passed through as-is.
fn message_display_name(message: &InboundMessage) -> &str {
    message
        .formatted_author
        .as_deref()
        .or_else(|| {
            message
                .metadata
                .get("sender_display_name")
                .and_then(|v| v.as_str())
        })
        .unwrap_or(&message.sender_id)
}

fn format_user_message(raw_text: &str, message: &InboundMessage, timestamp_text: &str) -> String {
    if message.source == "system" {
        // System messages should never be empty, but guard against it
        return if raw_text.trim().is_empty() {
            "[system event]".to_string()
        } else {
            raw_text.to_string()
        };
    }

    let display_name = message_display_name(message);

    let bot_tag = if message
        .metadata
        .get("sender_is_bot")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        " (bot)"
    } else {
        ""
    };

    let reply_context = message
        .metadata
        .get("reply_to_author")
        .and_then(|v| v.as_str())
        .map(|author| {
            let content_preview = message
                .metadata
                .get("reply_to_text")
                .or_else(|| message.metadata.get("reply_to_content"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if content_preview.is_empty() {
                format!(" (replying to {author})")
            } else {
                format!(" (replying to {author}: \"{content_preview}\")")
            }
        })
        .unwrap_or_default();

    // If raw_text is empty or just whitespace, use a placeholder to avoid
    // sending empty text content blocks to the LLM API.
    let text_content = if raw_text.trim().is_empty() {
        "[attachment or empty message]"
    } else {
        raw_text
    };

    format!("{display_name}{bot_tag}{reply_context} [{timestamp_text}]: {text_content}")
}

fn format_batched_user_message(
    display_name: &str,
    absolute_timestamp: &str,
    relative_text: &str,
    raw_text: &str,
) -> String {
    let text_content = if raw_text.trim().is_empty() {
        "[attachment or empty message]"
    } else {
        raw_text
    };
    format!("[{display_name}] ({absolute_timestamp}; {relative_text}): {text_content}")
}

fn extract_discord_message_id(message: &InboundMessage) -> Option<u64> {
    if message.source != "discord" {
        return None;
    }

    message
        .metadata
        .get("discord_message_id")
        .and_then(|value| value.as_u64())
}

/// Check if a ProcessEvent is targeted at a specific channel.
///
/// Events from branches and workers carry a channel_id. We only process events
/// that originated from this channel — otherwise broadcast events from one
/// channel's workers would leak into sibling channels (e.g. threads).
fn event_is_for_channel(event: &ProcessEvent, channel_id: &ChannelId) -> bool {
    match event {
        ProcessEvent::BranchResult {
            channel_id: event_channel,
            ..
        } => event_channel == channel_id,
        ProcessEvent::WorkerComplete {
            channel_id: event_channel,
            ..
        } => event_channel.as_ref() == Some(channel_id),
        ProcessEvent::WorkerStatus {
            channel_id: event_channel,
            ..
        } => event_channel.as_ref() == Some(channel_id),
        // Status block updates, tool events, etc. — match on agent_id which
        // is already filtered by the event bus subscription. Let them through.
        _ => true,
    }
}

/// Image MIME types we support for vision.
const IMAGE_MIME_PREFIXES: &[&str] = &["image/jpeg", "image/png", "image/gif", "image/webp"];

/// Text-based MIME types where we inline the content.
const TEXT_MIME_PREFIXES: &[&str] = &[
    "text/",
    "application/json",
    "application/xml",
    "application/javascript",
    "application/typescript",
    "application/toml",
    "application/yaml",
];

/// Download attachments and convert them to LLM-ready UserContent parts.
///
/// Images become `UserContent::Image` (base64). Text files get inlined.
/// Other file types get a metadata-only description.
async fn download_attachments(
    deps: &AgentDeps,
    attachments: &[crate::Attachment],
) -> Vec<UserContent> {
    let http = deps.llm_manager.http_client();
    let mut parts = Vec::new();

    for attachment in attachments {
        let is_image = IMAGE_MIME_PREFIXES
            .iter()
            .any(|p| attachment.mime_type.starts_with(p));
        let is_text = TEXT_MIME_PREFIXES
            .iter()
            .any(|p| attachment.mime_type.starts_with(p));

        let content = if is_image {
            download_image_attachment(http, attachment).await
        } else if is_text {
            download_text_attachment(http, attachment).await
        } else if attachment.mime_type.starts_with("audio/") {
            transcribe_audio_attachment(deps, http, attachment).await
        } else {
            let size_str = attachment
                .size_bytes
                .map(|s| format!("{:.1} KB", s as f64 / 1024.0))
                .unwrap_or_else(|| "unknown size".into());
            UserContent::text(format!(
                "[Attachment: {} ({}, {})]",
                attachment.filename, attachment.mime_type, size_str
            ))
        };

        parts.push(content);
    }

    parts
}

/// Download raw bytes from an attachment URL, including auth if present.
///
/// When `auth_header` is set (Slack), uses a no-redirect client and manually
/// follows redirects so the `Authorization` header isn't silently stripped on
/// cross-origin redirects. For public URLs (Discord/Telegram), uses a plain GET.
async fn download_attachment_bytes(
    http: &reqwest::Client,
    attachment: &crate::Attachment,
) -> std::result::Result<Vec<u8>, String> {
    if attachment.auth_header.is_some() {
        download_attachment_bytes_with_auth(attachment).await
    } else {
        let response = http
            .get(&attachment.url)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !response.status().is_success() {
            return Err(format!("HTTP {}", response.status()));
        }
        response
            .bytes()
            .await
            .map(|b| b.to_vec())
            .map_err(|e| e.to_string())
    }
}

/// Slack-specific download: manually follows redirects, only forwarding the
/// Authorization header when the redirect target shares the same host as the
/// original URL. This prevents credential leakage on cross-origin redirects.
async fn download_attachment_bytes_with_auth(
    attachment: &crate::Attachment,
) -> std::result::Result<Vec<u8>, String> {
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .map_err(|e| format!("failed to build HTTP client: {e}"))?;

    let auth = attachment.auth_header.as_deref().unwrap_or_default();
    let original_url =
        reqwest::Url::parse(&attachment.url).map_err(|e| format!("invalid attachment URL: {e}"))?;
    let original_host = original_url.host_str().unwrap_or_default().to_owned();
    let mut current_url = original_url;

    for hop in 0..5 {
        let same_host = current_url.host_str().unwrap_or_default() == original_host;

        let mut request = client.get(current_url.clone());
        if same_host {
            request = request.header(reqwest::header::AUTHORIZATION, auth);
        }

        tracing::debug!(hop, url = %current_url, same_host, "following attachment redirect");

        let response = request.send().await.map_err(|e| e.to_string())?;
        let status = response.status();

        if status.is_redirection() {
            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .ok_or_else(|| format!("redirect without Location header ({status})"))?;
            let location_str = location
                .to_str()
                .map_err(|e| format!("invalid Location header: {e}"))?;
            current_url = current_url
                .join(location_str)
                .map_err(|e| format!("invalid redirect URL: {e}"))?;
            continue;
        }

        if !status.is_success() {
            return Err(format!("HTTP {}", status));
        }

        return response
            .bytes()
            .await
            .map(|b| b.to_vec())
            .map_err(|e| e.to_string());
    }

    Err("too many redirects".into())
}

/// Download an image attachment and encode it as base64 for the LLM.
async fn download_image_attachment(
    http: &reqwest::Client,
    attachment: &crate::Attachment,
) -> UserContent {
    let bytes = match download_attachment_bytes(http, attachment).await {
        Ok(b) => b,
        Err(error) => {
            tracing::warn!(%error, filename = %attachment.filename, "failed to download image");
            return UserContent::text(format!(
                "[Failed to download image: {}]",
                attachment.filename
            ));
        }
    };

    use base64::Engine as _;
    let base64_data = base64::engine::general_purpose::STANDARD.encode(&bytes);
    let media_type = ImageMediaType::from_mime_type(&attachment.mime_type);

    tracing::info!(
        filename = %attachment.filename,
        mime = %attachment.mime_type,
        size = bytes.len(),
        "downloaded image attachment"
    );

    UserContent::image_base64(base64_data, media_type, None)
}

/// Download an audio attachment and transcribe it with the configured voice model.
async fn transcribe_audio_attachment(
    deps: &AgentDeps,
    http: &reqwest::Client,
    attachment: &crate::Attachment,
) -> UserContent {
    let bytes = match download_attachment_bytes(http, attachment).await {
        Ok(b) => b,
        Err(error) => {
            tracing::warn!(%error, filename = %attachment.filename, "failed to download audio");
            return UserContent::text(format!(
                "[Failed to download audio: {}]",
                attachment.filename
            ));
        }
    };

    tracing::info!(
        filename = %attachment.filename,
        mime = %attachment.mime_type,
        size = bytes.len(),
        "downloaded audio attachment"
    );

    let routing = deps.runtime_config.routing.load();
    let voice_model = routing.voice.trim();
    if voice_model.is_empty() {
        return UserContent::text(format!(
            "[Audio attachment received but no voice model is configured in routing.voice: {}]",
            attachment.filename
        ));
    }

    let (provider_id, model_name) = match deps.llm_manager.resolve_model(voice_model) {
        Ok(parts) => parts,
        Err(error) => {
            tracing::warn!(%error, model = %voice_model, "invalid voice model route");
            return UserContent::text(format!(
                "[Audio transcription failed for {}: invalid voice model '{}']",
                attachment.filename, voice_model
            ));
        }
    };

    let provider = match deps.llm_manager.get_provider(&provider_id) {
        Ok(provider) => provider,
        Err(error) => {
            tracing::warn!(%error, provider = %provider_id, "voice provider not configured");
            return UserContent::text(format!(
                "[Audio transcription failed for {}: provider '{}' is not configured]",
                attachment.filename, provider_id
            ));
        }
    };

    if provider.api_type == ApiType::Anthropic {
        return UserContent::text(format!(
            "[Audio transcription failed for {}: provider '{}' does not support input_audio on this endpoint]",
            attachment.filename, provider_id
        ));
    }

    let format = audio_format_for_attachment(attachment);
    use base64::Engine as _;
    let base64_audio = base64::engine::general_purpose::STANDARD.encode(&bytes);

    let endpoint = format!(
        "{}/v1/chat/completions",
        provider.base_url.trim_end_matches('/')
    );
    let body = serde_json::json!({
        "model": model_name,
        "messages": [{
            "role": "user",
            "content": [
                {
                    "type": "text",
                    "text": "Transcribe this audio verbatim. Return only the transcription text."
                },
                {
                    "type": "input_audio",
                    "input_audio": {
                        "data": base64_audio,
                        "format": format,
                    }
                }
            ]
        }],
        "temperature": 0
    });

    let response = match deps
        .llm_manager
        .http_client()
        .post(&endpoint)
        .header("authorization", format!("Bearer {}", provider.api_key))
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .await
    {
        Ok(response) => response,
        Err(error) => {
            tracing::warn!(%error, model = %voice_model, "voice transcription request failed");
            return UserContent::text(format!(
                "[Audio transcription failed for {}]",
                attachment.filename
            ));
        }
    };

    let status = response.status();
    let response_body = match response.json::<serde_json::Value>().await {
        Ok(body) => body,
        Err(error) => {
            tracing::warn!(%error, model = %voice_model, "invalid transcription response");
            return UserContent::text(format!(
                "[Audio transcription failed for {}]",
                attachment.filename
            ));
        }
    };

    if !status.is_success() {
        let message = response_body["error"]["message"]
            .as_str()
            .unwrap_or("unknown error");
        tracing::warn!(
            status = %status,
            model = %voice_model,
            error = %message,
            "voice transcription provider returned error"
        );
        return UserContent::text(format!(
            "[Audio transcription failed for {}: {}]",
            attachment.filename, message
        ));
    }

    let transcript = extract_transcript_text(&response_body);
    if transcript.is_empty() {
        tracing::warn!(model = %voice_model, "empty transcription returned");
        return UserContent::text(format!(
            "[Audio transcription returned empty text for {}]",
            attachment.filename
        ));
    }

    UserContent::text(format!(
        "<voice_transcript name=\"{}\" mime=\"{}\">\n{}\n</voice_transcript>",
        attachment.filename, attachment.mime_type, transcript
    ))
}

fn audio_format_for_attachment(attachment: &crate::Attachment) -> &'static str {
    let mime = attachment.mime_type.to_lowercase();
    if mime.contains("mpeg") || mime.contains("mp3") {
        return "mp3";
    }
    if mime.contains("wav") {
        return "wav";
    }
    if mime.contains("flac") {
        return "flac";
    }
    if mime.contains("aac") {
        return "aac";
    }
    if mime.contains("ogg") {
        return "ogg";
    }
    if mime.contains("mp4") || mime.contains("m4a") {
        return "m4a";
    }

    match attachment
        .filename
        .rsplit('.')
        .next()
        .unwrap_or_default()
        .to_lowercase()
        .as_str()
    {
        "mp3" => "mp3",
        "wav" => "wav",
        "flac" => "flac",
        "aac" => "aac",
        "m4a" | "mp4" => "m4a",
        "oga" | "ogg" => "ogg",
        _ => "ogg",
    }
}

fn extract_transcript_text(body: &serde_json::Value) -> String {
    if let Some(text) = body["choices"][0]["message"]["content"].as_str() {
        return text.trim().to_string();
    }

    let Some(parts) = body["choices"][0]["message"]["content"].as_array() else {
        return String::new();
    };

    parts
        .iter()
        .filter_map(|part| {
            if part["type"].as_str() == Some("text") {
                part["text"].as_str().map(str::trim)
            } else {
                None
            }
        })
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Download a text attachment and inline its content for the LLM.
async fn download_text_attachment(
    http: &reqwest::Client,
    attachment: &crate::Attachment,
) -> UserContent {
    let bytes = match download_attachment_bytes(http, attachment).await {
        Ok(b) => b,
        Err(error) => {
            tracing::warn!(%error, filename = %attachment.filename, "failed to download text file");
            return UserContent::text(format!(
                "[Failed to download file: {}]",
                attachment.filename
            ));
        }
    };

    let content = String::from_utf8_lossy(&bytes).into_owned();

    // Truncate very large files to avoid blowing up context
    let truncated = if content.len() > 50_000 {
        format!(
            "{}...\n[truncated — {} bytes total]",
            &content[..50_000],
            content.len()
        )
    } else {
        content
    };

    tracing::info!(
        filename = %attachment.filename,
        mime = %attachment.mime_type,
        "downloaded text attachment"
    );

    UserContent::text(format!(
        "<file name=\"{}\" mime=\"{}\">\n{}\n</file>",
        attachment.filename, attachment.mime_type, truncated
    ))
}

/// Write history back after the agentic loop completes.
///
/// On success or `MaxTurnsError`, the history Rig built is consistent and safe
/// to keep.
///
/// On `PromptCancelled` (e.g. reply tool fired), Rig's carried history has
/// the user prompt + the assistant's tool-call message but no tool results.
/// Writing it back wholesale would leave a dangling tool-call that poisons
/// every subsequent turn. Instead, we preserve only the **first user text
/// message** Rig appended (the real user prompt), while discarding assistant
/// tool-call messages and tool-result user messages.
///
/// On hard errors, we truncate to the pre-turn snapshot since the history
/// state is unpredictable.
///
/// `MaxTurnsError` is safe — Rig pushes all tool results into a `User` message
/// before raising it, so history is consistent.
fn apply_history_after_turn(
    result: &std::result::Result<String, rig::completion::PromptError>,
    guard: &mut Vec<rig::message::Message>,
    history: Vec<rig::message::Message>,
    history_len_before: usize,
    channel_id: &str,
    is_retrigger: bool,
) {
    match result {
        Ok(_) | Err(rig::completion::PromptError::MaxTurnsError { .. }) => {
            *guard = history;
        }
        Err(rig::completion::PromptError::PromptCancelled { .. }) => {
            // Rig appended the user prompt and possibly an assistant tool-call
            // message to history before cancellation. We keep only the first
            // user text message (the actual user prompt) and discard everything else
            // (assistant tool-calls without results, tool-result user messages).
            //
            // Exception: retrigger turns. The "user prompt" Rig pushed is actually
            // the synthetic system retrigger message (internal template scaffolding),
            // not a real user message. We inject a proper summary record separately
            // in handle_message, so don't preserve anything from retrigger turns.
            if is_retrigger {
                tracing::debug!(
                    channel_id = %channel_id,
                    rolled_back = history.len().saturating_sub(history_len_before),
                    "discarding retrigger turn history (summary injected separately)"
                );
                return;
            }
            let new_messages = &history[history_len_before..];
            let mut preserved = 0usize;
            if let Some(message) = new_messages.iter().find(|m| is_user_text_message(m)) {
                guard.push(message.clone());
                preserved = 1;
            }
            // Skip: Assistant messages (contain tool calls without results),
            // user ToolResult messages, and internal correction prompts.
            tracing::debug!(
                channel_id = %channel_id,
                total_new = new_messages.len(),
                preserved,
                discarded = new_messages.len() - preserved,
                "selectively preserved first user message after PromptCancelled"
            );
        }
        Err(_) => {
            // Hard errors: history state is unpredictable, truncate to snapshot.
            tracing::debug!(
                channel_id = %channel_id,
                rolled_back = history.len().saturating_sub(history_len_before),
                "rolling back history after failed turn"
            );
            guard.truncate(history_len_before);
        }
    }
}

/// Returns true if a message is a User message containing only text content
/// (i.e., an actual user prompt, not a tool result).
fn is_user_text_message(message: &rig::message::Message) -> bool {
    match message {
        rig::message::Message::User { content } => content
            .iter()
            .all(|c| matches!(c, rig::message::UserContent::Text(_))),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        apply_history_after_turn, is_injection_block, prune_old_injection_blocks,
        ChannelInjectionState, INJECTION_BLOCK_PREFIX,
    };
    use rig::completion::{CompletionError, PromptError};
    use rig::message::Message;
    use rig::tool::ToolSetError;

    fn user_msg(text: &str) -> Message {
        Message::User {
            content: rig::OneOrMany::one(rig::message::UserContent::text(text)),
        }
    }

    fn assistant_msg(text: &str) -> Message {
        Message::Assistant {
            id: None,
            content: rig::OneOrMany::one(rig::message::AssistantContent::text(text)),
        }
    }

    fn injected_context_msg(text: &str) -> Message {
        user_msg(&format!("{INJECTION_BLOCK_PREFIX}:\n{text}"))
    }

    fn make_history(msgs: &[&str]) -> Vec<Message> {
        msgs.iter()
            .enumerate()
            .map(|(i, text)| {
                if i % 2 == 0 {
                    user_msg(text)
                } else {
                    assistant_msg(text)
                }
            })
            .collect()
    }

    /// On success, the full post-turn history is written back.
    #[test]
    fn ok_writes_history_back() {
        let mut guard = make_history(&["hello"]);
        let history = make_history(&["hello", "hi there", "how are you?"]);
        let len_before = 1;

        apply_history_after_turn(
            &Ok("hi there".to_string()),
            &mut guard,
            history.clone(),
            len_before,
            "test",
            false,
        );

        assert_eq!(guard, history);
    }

    /// MaxTurnsError carries consistent history (tool results included) — write it back.
    #[test]
    fn max_turns_writes_history_back() {
        let mut guard = make_history(&["hello"]);
        let history = make_history(&["hello", "hi there", "how are you?"]);
        let len_before = 1;

        let err = Err(PromptError::MaxTurnsError {
            max_turns: 5,
            chat_history: Box::new(history.clone()),
            prompt: Box::new(user_msg("prompt")),
        });

        apply_history_after_turn(&err, &mut guard, history.clone(), len_before, "test", false);

        assert_eq!(guard, history);
    }

    /// PromptCancelled preserves user text messages but discards assistant
    /// tool-call messages (which have no matching tool results).
    #[test]
    fn prompt_cancelled_preserves_user_prompt() {
        let initial = make_history(&["hello", "thinking..."]);
        let mut guard = initial.clone();
        // Simulate what Rig does: push user prompt + assistant tool-call
        let mut history = initial.clone();
        history.push(user_msg("new user prompt")); // should be preserved
        history.push(assistant_msg("tool call without result")); // should be discarded
        let len_before = initial.len();

        let err = Err(PromptError::PromptCancelled {
            chat_history: Box::new(history.clone()),
            reason: "reply delivered".to_string(),
        });

        apply_history_after_turn(&err, &mut guard, history, len_before, "test", false);

        // User prompt should be preserved, assistant tool-call discarded
        let mut expected = initial;
        expected.push(user_msg("new user prompt"));
        assert_eq!(
            guard, expected,
            "user text messages should be preserved, assistant messages discarded"
        );
    }

    /// PromptCancelled discards tool-result User messages (ToolResult content).
    #[test]
    fn prompt_cancelled_discards_tool_results() {
        let initial = make_history(&["hello", "thinking..."]);
        let mut guard = initial.clone();
        let mut history = initial.clone();
        history.push(user_msg("new user prompt")); // preserved
        // Simulate an assistant tool-call followed by a tool-result user message
        history.push(Message::Assistant {
            id: None,
            content: rig::OneOrMany::one(rig::message::AssistantContent::tool_call(
                "call_1",
                "reply",
                serde_json::json!({"content": "hello"}),
            )),
        });
        // A tool-result message is a User message with ToolResult content —
        // is_user_text_message returns false for these, so they get discarded.
        history.push(Message::User {
            content: rig::OneOrMany::one(rig::message::UserContent::ToolResult(
                rig::message::ToolResult {
                    id: "call_1".to_string(),
                    call_id: None,
                    content: rig::OneOrMany::one(rig::message::ToolResultContent::text("ok")),
                },
            )),
        });
        let len_before = initial.len();

        let err = Err(PromptError::PromptCancelled {
            chat_history: Box::new(history.clone()),
            reason: "reply delivered".to_string(),
        });

        apply_history_after_turn(&err, &mut guard, history, len_before, "test", false);

        let mut expected = initial;
        expected.push(user_msg("new user prompt"));
        assert_eq!(
            guard, expected,
            "tool-call and tool-result messages should be discarded"
        );
    }

    /// PromptCancelled preserves only the first user prompt and drops any
    /// internal correction prompts that may have been appended on retry.
    #[test]
    fn prompt_cancelled_preserves_only_first_user_prompt() {
        let initial = make_history(&["hello", "thinking..."]);
        let mut guard = initial.clone();
        let mut history = initial.clone();
        history.push(user_msg("real user prompt")); // preserved
        history.push(assistant_msg("bad tool syntax"));
        history.push(user_msg("Please proceed and use the available tools.")); // dropped
        history.push(assistant_msg("tool call without result"));
        let len_before = initial.len();

        let err = Err(PromptError::PromptCancelled {
            chat_history: Box::new(history.clone()),
            reason: "reply delivered".to_string(),
        });

        apply_history_after_turn(&err, &mut guard, history, len_before, "test", false);

        let mut expected = initial;
        expected.push(user_msg("real user prompt"));
        assert_eq!(
            guard, expected,
            "only the first user prompt should be preserved"
        );
    }

    /// PromptCancelled on retrigger turns discards everything — the synthetic
    /// system message is internal scaffolding, not a real user message.
    /// A summary record is injected separately in handle_message.
    #[test]
    fn prompt_cancelled_retrigger_discards_all() {
        let initial = make_history(&["hello", "thinking..."]);
        let mut guard = initial.clone();
        let mut history = initial.clone();
        history.push(user_msg("[System: 1 background process completed...]"));
        history.push(assistant_msg("relaying result..."));
        let len_before = initial.len();

        let err = Err(PromptError::PromptCancelled {
            chat_history: Box::new(history.clone()),
            reason: "reply delivered".to_string(),
        });

        apply_history_after_turn(&err, &mut guard, history, len_before, "test", true);

        assert_eq!(
            guard, initial,
            "retrigger turns should discard all new messages"
        );
    }

    /// Hard completion errors also roll back to prevent dangling tool-calls.
    #[test]
    fn completion_error_rolls_back() {
        let initial = make_history(&["hello", "thinking..."]);
        let mut guard = initial.clone();
        let mut history = initial.clone();
        history.push(user_msg("[dangling tool-call]"));
        let len_before = initial.len();

        let err = Err(PromptError::CompletionError(
            CompletionError::ResponseError("API error".to_string()),
        ));

        apply_history_after_turn(&err, &mut guard, history, len_before, "test", false);

        assert_eq!(
            guard, initial,
            "history should be rolled back after hard error"
        );
    }

    /// ToolError (tool not found) rolls back — same catch-all arm as hard errors.
    #[test]
    fn tool_error_rolls_back() {
        let initial = make_history(&["hello", "thinking..."]);
        let mut guard = initial.clone();
        let mut history = initial.clone();
        history.push(user_msg("[dangling tool-call]"));
        let len_before = initial.len();

        let err = Err(PromptError::ToolError(ToolSetError::ToolNotFoundError(
            "nonexistent_tool".to_string(),
        )));

        apply_history_after_turn(&err, &mut guard, history, len_before, "test", false);

        assert_eq!(
            guard, initial,
            "history should be rolled back after tool error"
        );
    }

    /// Rollback on empty history is a no-op and must not panic.
    #[test]
    fn rollback_on_empty_history_is_noop() {
        let mut guard: Vec<Message> = vec![];
        let history: Vec<Message> = vec![];
        let len_before = 0;

        let err = Err(PromptError::PromptCancelled {
            chat_history: Box::new(history.clone()),
            reason: "reply delivered".to_string(),
        });

        apply_history_after_turn(&err, &mut guard, history, len_before, "test", false);

        assert!(
            guard.is_empty(),
            "empty history should stay empty after rollback"
        );
    }

    /// Rollback when nothing was appended is also a no-op (len unchanged).
    #[test]
    fn rollback_when_nothing_appended_is_noop() {
        let initial = make_history(&["hello", "thinking..."]);
        let mut guard = initial.clone();
        // history has same length as before — Rig cancelled before appending anything
        let history = initial.clone();
        let len_before = initial.len();

        let err = Err(PromptError::PromptCancelled {
            chat_history: Box::new(history.clone()),
            reason: "skip delivered".to_string(),
        });

        apply_history_after_turn(&err, &mut guard, history, len_before, "test", false);

        assert_eq!(
            guard, initial,
            "history should be unchanged when nothing was appended"
        );
    }

    /// After PromptCancelled, the next turn starts clean with user messages
    /// preserved but no dangling assistant tool-calls.
    #[test]
    fn next_turn_is_clean_after_prompt_cancelled() {
        let initial = make_history(&["hello", "thinking..."]);
        let mut guard = initial.clone();
        let mut poisoned_history = initial.clone();
        // Rig appends: user prompt + assistant tool-call (dangling, no result)
        poisoned_history.push(user_msg("what's up"));
        poisoned_history.push(Message::Assistant {
            id: None,
            content: rig::OneOrMany::one(rig::message::AssistantContent::tool_call(
                "call_1",
                "reply",
                serde_json::json!({"content": "hey!"}),
            )),
        });
        let len_before = initial.len();

        // First turn: cancelled (reply tool fired) — not a retrigger
        apply_history_after_turn(
            &Err(PromptError::PromptCancelled {
                chat_history: Box::new(poisoned_history.clone()),
                reason: "reply delivered".to_string(),
            }),
            &mut guard,
            poisoned_history,
            len_before,
            "test",
            false,
        );

        // User prompt preserved, assistant tool-call discarded
        assert_eq!(
            guard.len(),
            initial.len() + 1,
            "user prompt should be preserved"
        );
        assert!(
            matches!(&guard[guard.len() - 1], Message::User { .. }),
            "last message should be the preserved user prompt"
        );

        // Second turn: new user message appended, successful response
        guard.push(user_msg("follow-up question"));
        let len_before2 = guard.len();
        let mut history2 = guard.clone();
        history2.push(assistant_msg("clean response"));

        apply_history_after_turn(
            &Ok("clean response".to_string()),
            &mut guard,
            history2.clone(),
            len_before2,
            "test",
            false,
        );

        assert_eq!(
            guard, history2,
            "second turn should succeed with clean history"
        );
        // No dangling tool-call assistant messages in history
        let has_dangling = guard.iter().any(|m| {
            if let Message::Assistant { content, .. } = m {
                content
                    .iter()
                    .any(|c| matches!(c, rig::message::AssistantContent::ToolCall(_)))
            } else {
                false
            }
        });
        assert!(
            !has_dangling,
            "no dangling tool-call messages in history after rollback"
        );
    }

    /// A memory with the same ID should be filtered while still in context window.
    #[test]
    fn deduplication_exact_id() {
        let mut state = ChannelInjectionState::new();
        let memory_id = "memory-123";
        let current_turn = 10;
        let context_window_depth = 50;

        assert!(state.should_reinject(memory_id, current_turn, context_window_depth));

        state.record_injection(memory_id.to_string(), current_turn);

        assert!(!state.should_reinject(memory_id, current_turn, context_window_depth));

        let next_turn = current_turn + 10;
        assert!(!state.should_reinject(memory_id, next_turn, context_window_depth));

        let far_turn = current_turn + context_window_depth + 1;
        assert!(state.should_reinject(memory_id, far_turn, context_window_depth));
    }

    /// Injection state records IDs and semantic embeddings correctly.
    #[test]
    fn channel_injection_state_updates() {
        let mut state = ChannelInjectionState::new();

        assert!(state.injected_ids.is_empty());
        assert!(state.semantic_buffer.is_empty());

        let memory_id = "memory-abc";
        let turn = 5;
        state.record_injection(memory_id.to_string(), turn);

        assert_eq!(state.injected_ids.len(), 1);
        assert_eq!(state.injected_ids.get(memory_id), Some(&turn));

        let embedding = vec![1.0, 2.0, 3.0];
        state.add_embedding(embedding.clone(), turn);

        assert_eq!(state.semantic_buffer.len(), 1);
        assert_eq!(state.semantic_buffer.front(), Some(&(embedding, turn)));

        for i in 0..5 {
            state.record_injection(format!("memory-{}", i), turn + i);
        }
        assert_eq!(state.injected_ids.len(), 6);

        for i in 0..5 {
            state.add_embedding(vec![i as f32, (i + 1) as f32], turn + i);
        }
        assert_eq!(state.semantic_buffer.len(), 6);
    }

    /// Semantic buffer is bounded and behaves FIFO.
    #[test]
    fn semantic_buffer_bounded() {
        let mut state = ChannelInjectionState::new();

        for i in 0..ChannelInjectionState::MAX_ENTRIES + 10 {
            state.add_embedding(vec![i as f32], i);
        }

        assert_eq!(state.semantic_buffer.len(), ChannelInjectionState::MAX_ENTRIES);

        let first = state.semantic_buffer.front().expect("buffer should not be empty");
        assert_eq!(first.0[0], 10.0);
        assert_eq!(first.1, 10);
    }

    #[test]
    fn semantic_buffer_turn_pruning() {
        let mut state = ChannelInjectionState::new();

        state.add_embedding(vec![1.0], 1);
        state.add_embedding(vec![2.0], 10);
        state.add_embedding(vec![3.0], 20);

        state.prune_semantic_buffer(25, 10);

        assert_eq!(state.semantic_buffer.len(), 1);
        assert_eq!(state.semantic_buffer.front(), Some(&(vec![3.0], 20)));
    }

    /// Injected ID map is pruned to bounded size.
    #[test]
    fn injected_ids_pruning() {
        let mut state = ChannelInjectionState::new();

        for i in 0..ChannelInjectionState::MAX_ENTRIES + 10 {
            state.record_injection(format!("memory-{}", i), i);
        }

        assert_eq!(state.injected_ids.len(), ChannelInjectionState::MAX_ENTRIES);
        assert!(state.injected_ids.get("memory-5").is_none());
        assert!(state.injected_ids.get("memory-15").is_some());
    }

    /// Edge cases for reinjection logic.
    #[test]
    fn should_reinject_edge_cases() {
        let state = ChannelInjectionState::new();
        assert!(state.should_reinject("", 0, 50));

        let mut state2 = ChannelInjectionState::new();
        state2.record_injection("test".to_string(), 0);
        assert!(!state2.should_reinject("test", 0, 0));
        assert!(state2.should_reinject("test", 1, 0));
    }

    #[test]
    fn detects_injection_block() {
        assert!(is_injection_block(&injected_context_msg("[Fact] hello")));
        assert!(!is_injection_block(&user_msg("regular user message")));
        assert!(!is_injection_block(&assistant_msg("assistant response")));
    }

    #[test]
    fn prune_injection_blocks_under_cap_is_noop() {
        let mut history = vec![
            user_msg("u1"),
            injected_context_msg("[Fact] a"),
            assistant_msg("a1"),
            injected_context_msg("[Fact] b"),
            assistant_msg("a2"),
        ];

        prune_old_injection_blocks(&mut history, 3);

        let injected_count = history.iter().filter(|m| is_injection_block(m)).count();
        assert_eq!(injected_count, 2);
    }

    #[test]
    fn prune_injection_blocks_removes_oldest_when_at_cap() {
        let mut history = vec![
            injected_context_msg("[Fact] oldest"),
            user_msg("u1"),
            injected_context_msg("[Fact] middle"),
            assistant_msg("a1"),
            injected_context_msg("[Fact] newest"),
        ];

        prune_old_injection_blocks(&mut history, 2);

        let injected_blocks: Vec<String> = history
            .iter()
            .filter_map(|message| {
                if let Message::User { content } = message {
                    return content.iter().find_map(|item| {
                        if let rig::message::UserContent::Text(t) = item
                            && t.text.starts_with(INJECTION_BLOCK_PREFIX)
                        {
                            return Some(t.text.clone());
                        }
                        None
                    });
                }
                None
            })
            .collect();

        // We prune before inserting a new block, so when at cap we keep
        // max_keep - 1 existing blocks to make room for the incoming block.
        assert_eq!(injected_blocks.len(), 1);
        assert!(!injected_blocks.iter().any(|text| text.contains("oldest")));
        assert!(injected_blocks.iter().any(|text| text.contains("newest")));
    }

    #[test]
    fn prune_injection_blocks_ephemeral_removes_all() {
        let mut history = vec![
            user_msg("u1"),
            injected_context_msg("[Fact] a"),
            assistant_msg("a1"),
            injected_context_msg("[Fact] b"),
        ];

        prune_old_injection_blocks(&mut history, 0);

        let injected_count = history.iter().filter(|m| is_injection_block(m)).count();
        assert_eq!(injected_count, 0);
        assert_eq!(history.len(), 2);
    }

    #[test]
    fn format_user_message_handles_empty_text() {
        use super::format_user_message;
        use crate::{Arc, InboundMessage};
        use chrono::Utc;
        use std::collections::HashMap;

        // Test empty text with user message
        let message = InboundMessage {
            id: "test".to_string(),
            agent_id: Some(Arc::from("test_agent")),
            sender_id: "user123".to_string(),
            conversation_id: "conv".to_string(),
            content: crate::MessageContent::Text("".to_string()),
            source: "discord".to_string(),
            adapter: Some("discord".to_string()),
            metadata: HashMap::new(),
            formatted_author: Some("TestUser".to_string()),
            timestamp: Utc::now(),
        };

        let formatted = format_user_message("", &message, "2026-02-26 12:00:00 UTC");
        assert!(
            !formatted.trim().is_empty(),
            "formatted message should not be empty"
        );
        assert!(
            formatted.contains("[attachment or empty message]"),
            "should use placeholder for empty text"
        );

        // Test whitespace-only text
        let formatted_ws = format_user_message("   ", &message, "2026-02-26 12:00:00 UTC");
        assert!(
            formatted_ws.contains("[attachment or empty message]"),
            "should use placeholder for whitespace-only text"
        );

        // Test empty system message
        let system_message = InboundMessage {
            id: "test".to_string(),
            agent_id: Some(Arc::from("test_agent")),
            sender_id: "system".to_string(),
            conversation_id: "conv".to_string(),
            content: crate::MessageContent::Text("".to_string()),
            source: "system".to_string(),
            adapter: None,
            metadata: HashMap::new(),
            formatted_author: None,
            timestamp: Utc::now(),
        };

        let formatted_sys = format_user_message("", &system_message, "2026-02-26 12:00:00 UTC");
        assert_eq!(
            formatted_sys, "[system event]",
            "system messages should use [system event] placeholder"
        );

        // Test normal message with text
        let formatted_normal = format_user_message("hello", &message, "2026-02-26 12:00:00 UTC");
        assert!(
            formatted_normal.contains("hello"),
            "normal messages should preserve text"
        );
        assert!(
            formatted_normal.contains("[2026-02-26 12:00:00 UTC]"),
            "normal messages should include absolute timestamp context"
        );
        assert!(
            !formatted_normal.contains("[attachment or empty message]"),
            "normal messages should not use placeholder"
        );
    }

    #[test]
    fn message_display_name_uses_consistent_fallback_order() {
        use super::message_display_name;
        use crate::{Arc, InboundMessage};
        use chrono::Utc;
        use std::collections::HashMap;

        let mut metadata_only = HashMap::new();
        metadata_only.insert(
            "sender_display_name".to_string(),
            serde_json::Value::String("Metadata User".to_string()),
        );
        let metadata_message = InboundMessage {
            id: "metadata".to_string(),
            agent_id: Some(Arc::from("test_agent")),
            sender_id: "sender123".to_string(),
            conversation_id: "conv".to_string(),
            content: crate::MessageContent::Text("hello".to_string()),
            source: "discord".to_string(),
            adapter: Some("discord".to_string()),
            metadata: metadata_only,
            formatted_author: None,
            timestamp: Utc::now(),
        };
        assert_eq!(message_display_name(&metadata_message), "Metadata User");

        let mut both_metadata = HashMap::new();
        both_metadata.insert(
            "sender_display_name".to_string(),
            serde_json::Value::String("Metadata User".to_string()),
        );
        let formatted_author_message = InboundMessage {
            id: "formatted".to_string(),
            agent_id: Some(Arc::from("test_agent")),
            sender_id: "sender123".to_string(),
            conversation_id: "conv".to_string(),
            content: crate::MessageContent::Text("hello".to_string()),
            source: "discord".to_string(),
            adapter: Some("discord".to_string()),
            metadata: both_metadata,
            formatted_author: Some("Formatted Author".to_string()),
            timestamp: Utc::now(),
        };
        assert_eq!(
            message_display_name(&formatted_author_message),
            "Formatted Author"
        );

        let sender_fallback_message = InboundMessage {
            id: "fallback".to_string(),
            agent_id: Some(Arc::from("test_agent")),
            sender_id: "sender123".to_string(),
            conversation_id: "conv".to_string(),
            content: crate::MessageContent::Text("hello".to_string()),
            source: "discord".to_string(),
            adapter: Some("discord".to_string()),
            metadata: HashMap::new(),
            formatted_author: None,
            timestamp: Utc::now(),
        };
        assert_eq!(message_display_name(&sender_fallback_message), "sender123");
    }

    #[test]
    fn worker_task_temporal_context_preamble_includes_absolute_dates() {
        let prompt_engine =
            crate::prompts::PromptEngine::new("en").expect("prompt engine should initialize");
        let temporal_context = super::TemporalContext {
            now_utc: chrono::DateTime::parse_from_rfc3339("2026-02-26T20:30:00Z")
                .expect("valid RFC3339 timestamp")
                .with_timezone(&chrono::Utc),
            timezone: super::TemporalTimezone::Named {
                timezone_name: "America/New_York".to_string(),
                timezone: "America/New_York"
                    .parse()
                    .expect("valid timezone identifier"),
            },
        };

        let worker_task = super::build_worker_task_with_temporal_context(
            "Run the migration checks",
            &temporal_context,
            &prompt_engine,
        )
        .expect("worker task preamble should render");
        assert!(
            worker_task.contains("Current local date/time:"),
            "worker task should include local time context"
        );
        assert!(
            worker_task.contains("Current UTC date/time:"),
            "worker task should include UTC time context"
        );
        assert!(
            worker_task.contains("Run the migration checks"),
            "worker task should preserve the original task body"
        );
    }

    #[test]
    fn temporal_context_uses_cron_timezone_when_user_timezone_is_invalid() {
        let resolved = super::TemporalContext::resolve_timezone_from_names(
            Some("Not/A-Real-Tz".to_string()),
            Some("America/Los_Angeles".to_string()),
        );
        match resolved {
            super::TemporalTimezone::Named { timezone_name, .. } => {
                assert_eq!(timezone_name, "America/Los_Angeles");
            }
            super::TemporalTimezone::SystemLocal => {
                panic!("expected cron timezone fallback, got system local")
            }
        }
    }

    #[test]
    fn format_batched_message_includes_absolute_and_relative_time() {
        let formatted = super::format_batched_user_message(
            "alice",
            "2026-02-26 15:04:05 PST (America/Los_Angeles, UTC-08:00)",
            "12s ago",
            "ship it",
        );
        assert!(
            formatted.contains("2026-02-26 15:04:05 PST"),
            "batched formatting should include absolute timestamp"
        );
        assert!(
            formatted.contains("12s ago"),
            "batched formatting should include relative timestamp hint"
        );
        assert!(
            formatted.contains("ship it"),
            "batched formatting should include original message text"
        );
    }

    #[test]
    fn format_batched_message_uses_placeholder_for_empty_text() {
        let formatted = super::format_batched_user_message(
            "alice",
            "2026-02-26 15:04:05 PST (America/Los_Angeles, UTC-08:00)",
            "just now",
            "   ",
        );
        assert!(
            formatted.contains("[attachment or empty message]"),
            "batched formatting should use placeholder for empty/whitespace text"
        );
    }
}
