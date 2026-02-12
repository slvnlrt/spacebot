//! Identity file loading: SOUL.md, IDENTITY.md, USER.md, and system prompts.

use crate::error::Result;
use anyhow::Context as _;
use std::path::{Path, PathBuf};

/// Loaded identity files for an agent.
#[derive(Clone, Debug, Default)]
pub struct Identity {
    pub soul: Option<String>,
    pub identity: Option<String>,
    pub user: Option<String>,
}

impl Identity {
    /// Load identity files from an agent's workspace directory.
    pub async fn load(workspace: &Path) -> Self {
        Self {
            soul: load_optional_file(&workspace.join("SOUL.md")).await,
            identity: load_optional_file(&workspace.join("IDENTITY.md")).await,
            user: load_optional_file(&workspace.join("USER.md")).await,
        }
    }

    /// Render identity context for injection into system prompts.
    pub fn render(&self) -> String {
        let mut output = String::new();

        if let Some(soul) = &self.soul {
            output.push_str("## Soul\n\n");
            output.push_str(soul);
            output.push_str("\n\n");
        }
        if let Some(identity) = &self.identity {
            output.push_str("## Identity\n\n");
            output.push_str(identity);
            output.push_str("\n\n");
        }
        if let Some(user) = &self.user {
            output.push_str("## User\n\n");
            output.push_str(user);
            output.push_str("\n\n");
        }

        output
    }
}

/// Container for all loaded process-type prompts.
#[derive(Clone, Debug)]
pub struct Prompts {
    pub channel: String,
    pub branch: String,
    pub worker: String,
    pub cortex: String,
    pub compactor: String,
}

impl Prompts {
    /// Load prompts with agent workspace override, falling back to shared prompts dir.
    pub async fn load(workspace: &Path, shared_prompts_dir: &Path) -> anyhow::Result<Self> {
        Ok(Self {
            channel: load_prompt("CHANNEL", workspace, shared_prompts_dir).await?,
            branch: load_prompt("BRANCH", workspace, shared_prompts_dir).await?,
            worker: load_prompt("WORKER", workspace, shared_prompts_dir).await?,
            cortex: load_prompt("CORTEX", workspace, shared_prompts_dir).await?,
            compactor: load_prompt("COMPACTOR", workspace, shared_prompts_dir).await?,
        })
    }
}

/// Load a prompt file with fallback chain:
/// 1. Agent workspace/prompts/{name}.md (override)
/// 2. Shared prompts/{name}.md (default)
/// 3. Relative prompts/{name}.md (dev/backward compat)
async fn load_prompt(name: &str, workspace: &Path, shared_prompts_dir: &Path) -> Result<String> {
    let filename = format!("{name}.md");

    let agent_path = workspace.join("prompts").join(&filename);
    if agent_path.exists() {
        return tokio::fs::read_to_string(&agent_path)
            .await
            .with_context(|| format!("failed to read agent prompt override: {}", agent_path.display()))
            .map_err(Into::into);
    }

    let shared_path = shared_prompts_dir.join(&filename);
    if shared_path.exists() {
        return tokio::fs::read_to_string(&shared_path)
            .await
            .with_context(|| format!("failed to read shared prompt: {}", shared_path.display()))
            .map_err(Into::into);
    }

    let relative_path = PathBuf::from("prompts").join(&filename);
    tokio::fs::read_to_string(&relative_path)
        .await
        .with_context(|| format!(
            "prompt not found: tried {}, {}, {}",
            agent_path.display(), shared_path.display(), relative_path.display()
        ))
        .map_err(Into::into)
}

/// Load a file if it exists, returning None if missing.
async fn load_optional_file(path: &Path) -> Option<String> {
    tokio::fs::read_to_string(path).await.ok()
}
