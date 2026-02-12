//! Branch: Fork context for thinking and delegation.

use crate::error::Result;
use crate::{BranchId, ChannelId, ProcessId, ProcessType, AgentDeps};
use crate::hooks::SpacebotHook;
// StatusBlock not used directly in branch
use std::sync::Arc;
use uuid::Uuid;

/// A branch is a fork of a channel's context for thinking.
pub struct Branch {
    pub id: BranchId,
    pub channel_id: ChannelId,
    pub description: String,
    pub deps: AgentDeps,
    pub hook: SpacebotHook,
    /// System prompt loaded from prompts/BRANCH.md.
    pub system_prompt: String,
    /// Clone of the channel's history at fork time (Rig message format).
    pub history: Vec<rig::message::Message>,
}

impl Branch {
    /// Create a new branch from a channel.
    pub fn new(
        channel_id: ChannelId,
        description: impl Into<String>,
        deps: AgentDeps,
        system_prompt: impl Into<String>,
        history: Vec<rig::message::Message>, // Clone of channel history
    ) -> Self {
        let id = Uuid::new_v4();
        let process_id = ProcessId::Branch(id);
        let hook = SpacebotHook::new(deps.agent_id.clone(), process_id, ProcessType::Branch, deps.event_tx.clone());
        
        Self {
            id,
            channel_id,
            description: description.into(),
            deps,
            hook,
            system_prompt: system_prompt.into(),
            history,
        }
    }
    
    /// Run the branch and return a conclusion.
    pub async fn run(mut self, prompt: impl Into<String>) -> Result<String> {
        let prompt = prompt.into();
        
        tracing::info!(branch_id = %self.id, channel_id = %self.channel_id, "branch starting");
        
        // In real implementation:
        // 1. Create LLM agent with branch context (no reply tool)
        // 2. Give it tools: memory_recall, memory_save, spawn_worker
        // 3. Run the agent with max_turns(10)
        // 4. Return the conclusion (final assistant message)
        // 5. Send BranchResult event
        
        // For now, just simulate
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
        
        let conclusion = format!("Branch completed analysis for: {}", prompt);
        
        // Send completion event
        let _ = self.deps.event_tx.send(crate::ProcessEvent::BranchResult {
            agent_id: self.deps.agent_id.clone(),
            branch_id: self.id,
            channel_id: self.channel_id.clone(),
            conclusion: conclusion.clone(),
        }).await;
        
        tracing::info!(branch_id = %self.id, "branch completed");
        
        Ok(conclusion)
    }
    
    /// Recall memories to inform thinking.
    pub async fn recall(&self, query: &str, max_results: usize) -> Result<Vec<String>> {
        use crate::tools::memory_recall;
        use std::sync::Arc;
        
        let memories = memory_recall::memory_recall(
            Arc::clone(&self.deps.memory_search),
            query,
            max_results,
        ).await?;
        
        Ok(memories.into_iter().map(|m| m.content).collect())
    }
    
    /// Spawn a worker from this branch.
    pub async fn spawn_worker(
        &self,
        task: impl Into<String>,
        interactive: bool,
    ) -> Result<crate::WorkerId> {
        use crate::tools::spawn_worker;
        
        let worker_id = spawn_worker::spawn_worker(
            Some(self.channel_id.clone()),
            task,
            interactive,
        ).await?;
        
        Ok(worker_id)
    }
}
