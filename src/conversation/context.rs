//! Context assembly: prompt + identity + memories + status.

use crate::error::Result;

/// Assembled context ready for injection into LLM.
#[derive(Debug, Clone)]
pub struct AssembledContext {
    /// Full system prompt with identity, memories, and status.
    pub system_prompt: String,
    /// Recent conversation history as formatted text.
    pub conversation_history: String,
}

// NOTE: `build_channel_context` was removed — it had no callers and took a
// concrete `&MemoryStore`, which would bypass the `MemoryBackend` abstraction
// if ever wired in (followups #13). Context building for channels should go
// through `Arc<dyn MemoryBackend>` instead.

/// Build minimal context for a branch.
pub async fn build_branch_context(base_prompt: &str) -> Result<String> {
    // Branches get a simpler context - just their base prompt
    // They can recall memories as needed
    Ok(base_prompt.to_string())
}

/// Build context for a worker.
pub fn build_worker_context(base_prompt: &str, task: &str) -> String {
    format!("{}\n\n## Your Task\n\n{}", base_prompt, task)
}
