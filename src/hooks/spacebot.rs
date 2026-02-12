//! SpacebotHook: Prompt hook for channels, branches, and workers.

use crate::{AgentId, ProcessEvent, ProcessId, ProcessType};
use rig::agent::{HookAction, PromptHook, ToolCallHookAction};
use rig::completion::{CompletionModel, CompletionResponse, Message};
use tokio::sync::mpsc;

/// Hook for observing agent behavior and sending events.
#[derive(Clone)]
pub struct SpacebotHook {
    agent_id: AgentId,
    process_id: ProcessId,
    process_type: ProcessType,
    event_tx: mpsc::Sender<ProcessEvent>,
}

impl SpacebotHook {
    /// Create a new hook.
    pub fn new(
        agent_id: AgentId,
        process_id: ProcessId,
        process_type: ProcessType,
        event_tx: mpsc::Sender<ProcessEvent>,
    ) -> Self {
        Self {
            agent_id,
            process_id,
            process_type,
            event_tx,
        }
    }

    /// Send a status update event.
    pub fn send_status(&self, status: impl Into<String>) {
        let event = ProcessEvent::StatusUpdate {
            agent_id: self.agent_id.clone(),
            process_id: self.process_id.clone(),
            status: status.into(),
        };
        let _ = self.event_tx.try_send(event);
    }

    /// Scan content for potential secret leaks.
    fn scan_for_leaks(&self, content: &str) -> Option<String> {
        use regex::Regex;
        use std::sync::LazyLock;

        static LEAK_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
            vec![
                Regex::new(r"sk-[a-zA-Z0-9]{48}").expect("hardcoded regex"),
                Regex::new(r"-----BEGIN.*PRIVATE KEY-----").expect("hardcoded regex"),
                Regex::new(r"ghp_[a-zA-Z0-9]{36}").expect("hardcoded regex"),
                Regex::new(r"AIza[0-9A-Za-z_-]{35}").expect("hardcoded regex"),
            ]
        });

        for pattern in LEAK_PATTERNS.iter() {
            if let Some(matched) = pattern.find(content) {
                return Some(matched.as_str().to_string());
            }
        }

        None
    }
}

impl<M> PromptHook<M> for SpacebotHook
where
    M: CompletionModel,
{
    async fn on_completion_call(
        &self,
        _prompt: &Message,
        _history: &[Message],
    ) -> HookAction {
        // Log the completion call but don't block it
        tracing::debug!(
            process_id = %self.process_id,
            process_type = %self.process_type,
            "completion call started"
        );

        HookAction::Continue
    }

    async fn on_completion_response(
        &self,
        _prompt: &Message,
        response: &CompletionResponse<M::Response>,
    ) -> HookAction {
        // Tool nudging: check if response has tool calls
        // Note: Rig's CompletionResponse structure varies by model implementation
        // We'll do basic observation here

        tracing::debug!(
            process_id = %self.process_id,
            "completion response received"
        );

        HookAction::Continue
    }

    async fn on_tool_call(
        &self,
        tool_name: &str,
        _tool_call_id: Option<String>,
        _internal_call_id: &str,
        _args: &str,
    ) -> ToolCallHookAction {
        // Send event without blocking
        let event = ProcessEvent::ToolStarted {
            agent_id: self.agent_id.clone(),
            process_id: self.process_id.clone(),
            tool_name: tool_name.to_string(),
        };
        let _ = self.event_tx.try_send(event);

        tracing::debug!(
            process_id = %self.process_id,
            tool_name = %tool_name,
            "tool call started"
        );

        ToolCallHookAction::Continue
    }

    async fn on_tool_result(
        &self,
        tool_name: &str,
        _tool_call_id: Option<String>,
        _internal_call_id: &str,
        _args: &str,
        result: &str,
    ) -> HookAction {
        // Scan for potential leaks in tool output
        if let Some(leak) = self.scan_for_leaks(result) {
            tracing::warn!(
                process_id = %self.process_id,
                tool_name = %tool_name,
                leak = %leak,
                "potential secret leak detected in tool output"
            );
            // Return the result but log the warning
        }

        let event = ProcessEvent::ToolCompleted {
            agent_id: self.agent_id.clone(),
            process_id: self.process_id.clone(),
            tool_name: tool_name.to_string(),
            result: result.to_string(),
        };
        let _ = self.event_tx.try_send(event);

        tracing::debug!(
            process_id = %self.process_id,
            tool_name = %tool_name,
            "tool call completed"
        );

        HookAction::Continue
    }
}
