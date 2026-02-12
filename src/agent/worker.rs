//! Worker: Independent task execution process.

use crate::error::Result;
use crate::{WorkerId, ChannelId, ProcessId, ProcessType, AgentDeps};
use crate::hooks::SpacebotHook;
use std::sync::Arc;
use tokio::sync::{mpsc, watch};
use uuid::Uuid;

/// Worker state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerState {
    /// Worker is running and processing.
    Running,
    /// Worker is waiting for follow-up input (interactive only).
    WaitingForInput,
    /// Worker has completed successfully.
    Done,
    /// Worker has failed.
    Failed,
}

/// A worker process that executes tasks independently.
pub struct Worker {
    pub id: WorkerId,
    pub channel_id: Option<ChannelId>,
    pub task: String,
    pub state: WorkerState,
    pub deps: AgentDeps,
    pub hook: SpacebotHook,
    /// System prompt loaded from prompts/WORKER.md.
    pub system_prompt: String,
    /// Input channel for interactive workers.
    pub input_rx: Option<mpsc::Receiver<String>>,
    /// Status updates.
    pub status_tx: watch::Sender<String>,
    pub status_rx: watch::Receiver<String>,
}

impl Worker {
    /// Create a new fire-and-forget worker.
    pub fn new(
        channel_id: Option<ChannelId>,
        task: impl Into<String>,
        system_prompt: impl Into<String>,
        deps: AgentDeps,
    ) -> Self {
        let id = Uuid::new_v4();
        let process_id = ProcessId::Worker(id);
        let hook = SpacebotHook::new(deps.agent_id.clone(), process_id, ProcessType::Worker, deps.event_tx.clone());
        let (status_tx, status_rx) = watch::channel("starting".to_string());
        
        Self {
            id,
            channel_id,
            task: task.into(),
            state: WorkerState::Running,
            deps,
            hook,
            system_prompt: system_prompt.into(),
            input_rx: None,
            status_tx,
            status_rx,
        }
    }
    
    /// Create a new interactive worker.
    pub fn new_interactive(
        channel_id: Option<ChannelId>,
        task: impl Into<String>,
        system_prompt: impl Into<String>,
        deps: AgentDeps,
    ) -> (Self, mpsc::Sender<String>) {
        let id = Uuid::new_v4();
        let process_id = ProcessId::Worker(id);
        let hook = SpacebotHook::new(deps.agent_id.clone(), process_id, ProcessType::Worker, deps.event_tx.clone());
        let (status_tx, status_rx) = watch::channel("starting".to_string());
        let (input_tx, input_rx) = mpsc::channel(32);
        
        let worker = Self {
            id,
            channel_id,
            task: task.into(),
            state: WorkerState::Running,
            deps,
            hook,
            system_prompt: system_prompt.into(),
            input_rx: Some(input_rx),
            status_tx,
            status_rx,
        };
        
        (worker, input_tx)
    }
    
    /// Check if the worker can transition to a new state.
    pub fn can_transition_to(&self, target: WorkerState) -> bool {
        use WorkerState::*;
        
        matches!(
            (self.state, target),
            (Running, WaitingForInput)
                | (Running, Done)
                | (Running, Failed)
                | (WaitingForInput, Running)
                | (WaitingForInput, Failed)
        )
    }
    
    /// Transition to a new state.
    pub fn transition_to(&mut self, new_state: WorkerState) -> Result<()> {
        if !self.can_transition_to(new_state) {
            return Err(crate::error::AgentError::InvalidStateTransition(
                format!("can't transition from {:?} to {:?}", self.state, new_state)
            ).into());
        }
        
        self.state = new_state;
        Ok(())
    }
    
    /// Run the worker until completion.
    pub async fn run(mut self) -> Result<String> {
        // Update status
        self.status_tx.send_modify(|s| *s = "running".to_string());
        self.hook.send_status("running");
        
        // In real implementation:
        // 1. Create LLM agent with task-specific tools (shell, file, exec)
        // 2. Run the agent loop with max_turns(50)
        // 3. Handle interactive mode if input_rx is Some
        // 4. Return the final result
        
        // For now, just simulate completion
        tracing::info!(worker_id = %self.id, "worker running");
        
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
        
        self.state = WorkerState::Done;
        self.hook.send_status("completed");
        
        Ok(format!("Task completed: {}", self.task))
    }
    
    /// Check if worker is in a terminal state.
    pub fn is_done(&self) -> bool {
        matches!(self.state, WorkerState::Done | WorkerState::Failed)
    }
    
    /// Check if worker is interactive.
    pub fn is_interactive(&self) -> bool {
        self.input_rx.is_some()
    }
}
