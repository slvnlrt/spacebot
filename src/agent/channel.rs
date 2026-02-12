//! Channel: User-facing conversation process.

use crate::error::{AgentError, Result};
use crate::{ChannelId, WorkerId, BranchId, ProcessId, ProcessType, AgentDeps, InboundMessage, ProcessEvent};
use crate::hooks::SpacebotHook;
use crate::agent::status::StatusBlock;
use crate::agent::worker::Worker;
use crate::agent::branch::Branch;
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};
use tokio::sync::broadcast;
use std::collections::HashMap;

/// Channel configuration.
#[derive(Debug, Clone)]
pub struct ChannelConfig {
    /// Maximum concurrent branches.
    pub max_concurrent_branches: usize,
    /// Maximum turns for channel LLM calls.
    pub max_turns: usize,
    /// Model name for this channel.
    pub model_name: String,
}

impl Default for ChannelConfig {
    fn default() -> Self {
        Self {
            max_concurrent_branches: 5,
            max_turns: 5,
            model_name: "anthropic/claude-sonnet-4-20250514".into(),
        }
    }
}

/// User-facing conversation process.
pub struct Channel {
    pub id: ChannelId,
    pub title: Option<String>,
    pub config: ChannelConfig,
    pub deps: AgentDeps,
    pub hook: SpacebotHook,
    pub status_block: Arc<RwLock<StatusBlock>>,
    /// System prompt loaded from prompts/CHANNEL.md.
    pub system_prompt: String,
    /// Conversation history (Rig message format for LLM calls).
    pub history: Arc<RwLock<Vec<rig::message::Message>>>,
    /// Active branches.
    pub active_branches: Arc<RwLock<HashMap<BranchId, tokio::task::JoinHandle<()>>>>,
    /// Active workers.
    pub active_workers: Arc<RwLock<HashMap<WorkerId, Worker>>>,
    /// Input channel for receiving messages.
    pub message_rx: mpsc::Receiver<InboundMessage>,
    /// Event receiver for process events.
    pub event_rx: broadcast::Receiver<ProcessEvent>,
}

impl Channel {
    /// Create a new channel.
    pub fn new(
        id: ChannelId,
        deps: AgentDeps,
        config: ChannelConfig,
        system_prompt: impl Into<String>,
        event_rx: broadcast::Receiver<ProcessEvent>,
    ) -> (Self, mpsc::Sender<InboundMessage>) {
        let process_id = ProcessId::Channel(id.clone());
        let hook = SpacebotHook::new(deps.agent_id.clone(), process_id, ProcessType::Channel, deps.event_tx.clone());
        let status_block = Arc::new(RwLock::new(StatusBlock::new()));
        let history = Arc::new(RwLock::new(Vec::new()));
        let (message_tx, message_rx) = mpsc::channel(64);
        
        let channel = Self {
            id: id.clone(),
            title: None,
            config,
            deps,
            hook,
            status_block,
            system_prompt: system_prompt.into(),
            history,
            active_branches: Arc::new(RwLock::new(HashMap::new())),
            active_workers: Arc::new(RwLock::new(HashMap::new())),
            message_rx,
            event_rx,
        };
        
        (channel, message_tx)
    }
    
    /// Run the channel event loop.
    pub async fn run(mut self) -> Result<()> {
        tracing::info!(channel_id = %self.id, "channel started");
        
        loop {
            tokio::select! {
                Some(message) = self.message_rx.recv() => {
                    if let Err(e) = self.handle_message(message).await {
                        tracing::error!("error handling message: {}", e);
                    }
                }
                Ok(event) = self.event_rx.recv() => {
                    if let Err(e) = self.handle_event(event).await {
                        tracing::error!("error handling event: {}", e);
                    }
                }
                else => break,
            }
        }
        
        tracing::info!(channel_id = %self.id, "channel stopped");
        Ok(())
    }
    
    /// Handle an incoming message.
    async fn handle_message(&self, message: InboundMessage) -> Result<()> {
        tracing::info!(
            channel_id = %self.id,
            message_id = %message.id,
            "handling message"
        );
        
        // Add message to history
        {
            let mut history: tokio::sync::RwLockWriteGuard<'_, Vec<rig::message::Message>> = self.history.write().await;
            let content = match &message.content {
                crate::MessageContent::Text(text) => text.clone(),
                crate::MessageContent::Media { text, .. } => text.clone().unwrap_or_default(),
            };
            // Rig Message has From<String> impl for User messages
            history.push(rig::message::Message::from(content));
        }
        
        // Check if there's an active worker for this conversation
        {
            let workers: tokio::sync::RwLockReadGuard<'_, HashMap<WorkerId, Worker>> = self.active_workers.read().await;
            let _interactive_worker: Option<&Worker> = workers.values().find(|w: &&Worker| w.is_interactive());
        }
        
        // Branch to think about the message
        self.spawn_branch("analyze user intent and decide what to do").await?;
        
        Ok(())
    }
    
    /// Handle a process event.
    async fn handle_event(&self, event: ProcessEvent) -> Result<()> {
        // Update status block
        {
            let mut status: tokio::sync::RwLockWriteGuard<'_, StatusBlock> = self.status_block.write().await;
            status.update(&event);
        }
        
        match &event {
            ProcessEvent::BranchResult { branch_id, conclusion, .. } => {
                // Remove from active branches
                let mut branches: tokio::sync::RwLockWriteGuard<'_, HashMap<BranchId, tokio::task::JoinHandle<()>>> = self.active_branches.write().await;
                if let Some(handle) = branches.remove(branch_id) {
                    handle.abort();
                }
                
                // Add to history as an assistant message
                let mut history: tokio::sync::RwLockWriteGuard<'_, Vec<rig::message::Message>> = self.history.write().await;
                history.push(rig::message::Message::assistant(conclusion.clone()));
                
                tracing::info!(branch_id = %branch_id, "branch result incorporated");
            }
            ProcessEvent::WorkerComplete { worker_id, result, .. } => {
                // Remove from active workers
                let mut workers: tokio::sync::RwLockWriteGuard<'_, HashMap<WorkerId, Worker>> = self.active_workers.write().await;
                workers.remove(worker_id);
                
                tracing::info!(worker_id = %worker_id, result = %result, "worker completed");
            }
            _ => {}
        }
        
        Ok(())
    }
    
    /// Spawn a branch for thinking.
    pub async fn spawn_branch(&self, description: impl Into<String>) -> Result<BranchId> {
        let branches: tokio::sync::RwLockReadGuard<'_, HashMap<BranchId, tokio::task::JoinHandle<()>>> = self.active_branches.read().await;
        if branches.len() >= self.config.max_concurrent_branches {
            return Err(AgentError::BranchLimitReached {
                channel_id: self.id.to_string(),
                max: self.config.max_concurrent_branches,
            }.into());
        }
        drop(branches);
        
        // Clone history for the branch
        let history: Vec<rig::message::Message> = {
            let h: tokio::sync::RwLockReadGuard<'_, Vec<rig::message::Message>> = self.history.read().await;
            h.clone()
        };
        
        // TODO: Load branch prompt from identity::files::branch_prompt()
        let branch_system_prompt = "You are a branch process.";
        
        let branch = Branch::new(
            self.id.clone(),
            description,
            self.deps.clone(),
            branch_system_prompt,
            history,
        );
        
        let branch_id = branch.id;
        
        // Spawn the branch
        let handle = tokio::spawn(async move {
            let _ = branch.run("").await;
        });
        
        {
            let mut branches: tokio::sync::RwLockWriteGuard<'_, HashMap<BranchId, tokio::task::JoinHandle<()>>> = self.active_branches.write().await;
            branches.insert(branch_id, handle);
        }
        
        // Add to status block
        {
            let mut status: tokio::sync::RwLockWriteGuard<'_, StatusBlock> = self.status_block.write().await;
            status.add_branch(branch_id, "thinking...");
        }
        
        tracing::info!(branch_id = %branch_id, "branch spawned");
        
        Ok(branch_id)
    }
    
    /// Spawn a worker for a task.
    pub async fn spawn_worker(
        &self,
        task: impl Into<String>,
        interactive: bool,
    ) -> Result<WorkerId> {
        let task = task.into();
        
        // TODO: Load worker prompt from identity::files::worker_prompt()
        let worker_system_prompt = "You are a worker process.";
        
        let worker = if interactive {
            let (worker, _input_tx) = Worker::new_interactive(
                Some(self.id.clone()),
                &task,
                worker_system_prompt,
                self.deps.clone(),
            );
            worker
        } else {
            Worker::new(Some(self.id.clone()), &task, worker_system_prompt, self.deps.clone())
        };
        
        let worker_id = worker.id;
        
        // Add to active workers
        {
            let mut workers: tokio::sync::RwLockWriteGuard<'_, HashMap<WorkerId, Worker>> = self.active_workers.write().await;
            workers.insert(worker_id, worker);
        }
        
        // Add to status block
        {
            let mut status: tokio::sync::RwLockWriteGuard<'_, StatusBlock> = self.status_block.write().await;
            status.add_worker(worker_id, &task, false);
        }
        
        tracing::info!(worker_id = %worker_id, task = %task, "worker spawned");
        
        Ok(worker_id)
    }
    
    /// Get the current status block as a string.
    pub async fn get_status(&self) -> String {
        let status: tokio::sync::RwLockReadGuard<'_, StatusBlock> = self.status_block.read().await;
        status.render()
    }
}
