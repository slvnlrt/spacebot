//! Spawn worker tool for creating new workers.

use crate::agent::channel::{ChannelState, spawn_worker_from_state};
use crate::WorkerId;
use rig::completion::ToolDefinition;
use rig::tool::Tool;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Tool for spawning workers.
#[derive(Debug, Clone)]
pub struct SpawnWorkerTool {
    state: ChannelState,
}

impl SpawnWorkerTool {
    /// Create a new spawn worker tool with access to channel state.
    pub fn new(state: ChannelState) -> Self {
        Self { state }
    }
}

/// Error type for spawn worker tool.
#[derive(Debug, thiserror::Error)]
#[error("Worker spawn failed: {0}")]
pub struct SpawnWorkerError(String);

/// Arguments for spawn worker tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SpawnWorkerArgs {
    /// The task description for the worker.
    pub task: String,
    /// Whether this is an interactive worker (accepts follow-up messages).
    #[serde(default)]
    pub interactive: bool,
    /// Optional skill name to load into the worker's context. The worker will
    /// receive the full skill instructions in its system prompt.
    #[serde(default)]
    pub skill: Option<String>,
}

/// Output from spawn worker tool.
#[derive(Debug, Serialize)]
pub struct SpawnWorkerOutput {
    /// The ID of the spawned worker.
    pub worker_id: WorkerId,
    /// Whether the worker was spawned successfully.
    pub spawned: bool,
    /// Whether this is an interactive worker.
    pub interactive: bool,
    /// Status message.
    pub message: String,
}

impl Tool for SpawnWorkerTool {
    const NAME: &'static str = "spawn_worker";

    type Error = SpawnWorkerError;
    type Args = SpawnWorkerArgs;
    type Output = SpawnWorkerOutput;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: "Spawn an independent worker process with shell, file, and exec tools. The worker only sees the task description you provide — no conversation history.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "task": {
                        "type": "string",
                        "description": "Clear, specific description of what the worker should do. Include all context needed since the worker can't see your conversation."
                    },
                    "interactive": {
                        "type": "boolean",
                        "default": false,
                        "description": "If true, the worker stays alive and accepts follow-up messages via route_to_worker. If false (default), the worker runs once and returns."
                    },
                    "skill": {
                        "type": "string",
                        "description": "Name of a skill to load into the worker. The worker receives the full skill instructions in its system prompt. Only use skill names from <available_skills>."
                    }
                },
                "required": ["task"]
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let worker_id = spawn_worker_from_state(&self.state, &args.task, args.interactive, args.skill.as_deref())
            .await
            .map_err(|e| SpawnWorkerError(format!("{e}")))?;

        let message = if args.interactive {
            format!("Interactive worker {worker_id} spawned for: {}. Route follow-ups with route_to_worker.", args.task)
        } else {
            format!("Worker {worker_id} spawned for: {}. It will report back when done.", args.task)
        };

        Ok(SpawnWorkerOutput {
            worker_id,
            spawned: true,
            interactive: args.interactive,
            message,
        })
    }
}
