//! Set status tool for workers.

use crate::{AgentId, ChannelId, ProcessEvent, WorkerId};
use rig::completion::ToolDefinition;
use rig::tool::Tool;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

/// Tool for setting worker status.
#[derive(Debug, Clone)]
pub struct SetStatusTool {
    agent_id: AgentId,
    worker_id: WorkerId,
    channel_id: Option<ChannelId>,
    event_tx: mpsc::Sender<ProcessEvent>,
}

impl SetStatusTool {
    /// Create a new set status tool.
    pub fn new(
        agent_id: AgentId,
        worker_id: WorkerId,
        channel_id: Option<ChannelId>,
        event_tx: mpsc::Sender<ProcessEvent>,
    ) -> Self {
        Self {
            agent_id,
            worker_id,
            channel_id,
            event_tx,
        }
    }
}

/// Error type for set status tool.
#[derive(Debug, thiserror::Error)]
#[error("Failed to set status: {0}")]
pub struct SetStatusError(String);

/// Arguments for set status tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SetStatusArgs {
    /// The status message to report.
    pub status: String,
}

/// Output from set status tool.
#[derive(Debug, Serialize)]
pub struct SetStatusOutput {
    /// Whether the status was set successfully.
    pub success: bool,
    /// The worker ID.
    pub worker_id: WorkerId,
    /// The status that was set.
    pub status: String,
}

impl Tool for SetStatusTool {
    const NAME: &'static str = "set_status";

    type Error = SetStatusError;
    type Args = SetStatusArgs;
    type Output = SetStatusOutput;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: "Report the current status of your work. Use this to update the channel on your progress. The status will appear in the channel's status block. Keep statuses concise (1-2 sentences) and informative.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "status": {
                        "type": "string",
                        "description": "A concise status message describing your current progress (1-2 sentences)"
                    }
                },
                "required": ["status"]
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let event = ProcessEvent::WorkerStatus {
            agent_id: self.agent_id.clone(),
            worker_id: self.worker_id,
            channel_id: self.channel_id.clone(),
            status: args.status.clone(),
        };

        // Send without blocking
        let _ = self.event_tx.try_send(event);

        Ok(SetStatusOutput {
            success: true,
            worker_id: self.worker_id,
            status: args.status,
        })
    }
}

/// Legacy function for setting worker status.
pub fn set_status(
    agent_id: AgentId,
    worker_id: WorkerId,
    status: impl Into<String>,
    event_tx: &mpsc::Sender<ProcessEvent>,
) {
    let event = ProcessEvent::WorkerStatus {
        agent_id,
        worker_id,
        channel_id: None,
        status: status.into(),
    };

    // Send without blocking
    let _ = event_tx.try_send(event);
}
