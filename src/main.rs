//! Spacebot CLI entry point.

use anyhow::Context as _;
use clap::Parser;
use std::collections::HashMap;
use std::sync::Arc;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "spacebot")]
#[command(about = "A Rust agentic system with dedicated processes for every task")]
struct Cli {
    /// Path to config file (optional)
    #[arg(short, long)]
    config: Option<std::path::PathBuf>,

    /// Enable debug logging
    #[arg(short, long)]
    debug: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    let filter = if cli.debug {
        EnvFilter::new("debug")
    } else {
        EnvFilter::new("info")
    };

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .init();

    tracing::info!("starting spacebot");

    // Load configuration
    let config = if let Some(config_path) = cli.config {
        spacebot::config::Config::load_from_path(&config_path)
            .with_context(|| format!("failed to load config from {}", config_path.display()))?
    } else {
        spacebot::config::Config::load()
            .with_context(|| "failed to load configuration")?
    };

    tracing::info!(instance_dir = %config.instance_dir.display(), "configuration loaded");

    // Shared LLM manager (same API keys for all agents)
    let llm_manager = Arc::new(
        spacebot::llm::LlmManager::new(config.llm.clone())
            .await
            .with_context(|| "failed to initialize LLM manager")?
    );

    // Shared embedding model (stateless, agent-agnostic)
    let embedding_model = Arc::new(
        spacebot::memory::EmbeddingModel::new()
            .context("failed to initialize embedding model")?
    );

    tracing::info!("shared resources initialized");

    // Resolve agent configs and initialize each agent
    let resolved_agents = config.resolve_agents();
    let mut agents: HashMap<spacebot::AgentId, spacebot::Agent> = HashMap::new();

    let shared_prompts_dir = config.prompts_dir();

    for agent_config in &resolved_agents {
        tracing::info!(agent_id = %agent_config.id, "initializing agent");

        // Ensure agent directories exist
        std::fs::create_dir_all(&agent_config.workspace)
            .with_context(|| format!("failed to create workspace: {}", agent_config.workspace.display()))?;
        std::fs::create_dir_all(&agent_config.data_dir)
            .with_context(|| format!("failed to create data dir: {}", agent_config.data_dir.display()))?;
        std::fs::create_dir_all(&agent_config.archives_dir)
            .with_context(|| format!("failed to create archives dir: {}", agent_config.archives_dir.display()))?;

        // Per-agent database connections
        let db = spacebot::db::Db::connect(&agent_config.data_dir)
            .await
            .with_context(|| format!("failed to connect databases for agent '{}'", agent_config.id))?;

        // Per-agent memory system
        let memory_store = spacebot::memory::MemoryStore::new(db.sqlite.clone());
        let embedding_table = spacebot::memory::EmbeddingTable::open_or_create(&db.lance)
            .await
            .with_context(|| format!("failed to init embeddings for agent '{}'", agent_config.id))?;

        let memory_search = Arc::new(spacebot::memory::MemorySearch::new(
            memory_store,
            embedding_table,
            embedding_model.clone(),
        ));

        // Per-agent event bus
        let (event_tx, _event_rx) = tokio::sync::mpsc::channel(64);

        // Per-agent tool server (tools registered when agents are fully wired)
        let tool_server = rig::tool::server::ToolServer::new().run();

        let agent_id: spacebot::AgentId = Arc::from(agent_config.id.as_str());

        let deps = spacebot::AgentDeps {
            agent_id: agent_id.clone(),
            memory_search,
            llm_manager: llm_manager.clone(),
            tool_server,
            event_tx,
        };

        // Load identity files from agent workspace
        let identity = spacebot::identity::Identity::load(&agent_config.workspace).await;

        // Load prompts (agent overrides, then shared)
        let prompts = spacebot::identity::Prompts::load(
            &agent_config.workspace,
            &shared_prompts_dir,
        ).await.with_context(|| format!("failed to load prompts for agent '{}'", agent_config.id))?;

        let agent = spacebot::Agent {
            id: agent_id.clone(),
            config: agent_config.clone(),
            db,
            deps,
            prompts,
            identity,
        };

        tracing::info!(agent_id = %agent_config.id, "agent initialized");
        agents.insert(agent_id, agent);
    }

    tracing::info!(agent_count = agents.len(), "all agents initialized");

    // Wait for shutdown signal
    tokio::signal::ctrl_c().await?;

    tracing::info!("shutdown signal received");

    // Graceful shutdown
    for (agent_id, agent) in agents {
        tracing::info!(%agent_id, "shutting down agent");
        agent.db.close().await;
    }

    tracing::info!("spacebot stopped");
    Ok(())
}
