//! Cortex: System-level observer across all channels.

use crate::error::Result;
use crate::{ProcessEvent, AgentDeps};
use crate::hooks::CortexHook;
use std::sync::Arc;
use tokio::sync::RwLock;

/// The cortex observes system-wide activity and maintains coherence.
pub struct Cortex {
    pub deps: AgentDeps,
    pub hook: CortexHook,
    /// Recent activity signals (rolling window).
    pub signal_buffer: Arc<RwLock<Vec<Signal>>>,
    /// System prompt loaded from prompts/CORTEX.md.
    pub system_prompt: String,
}

/// A high-level activity signal (not raw conversation).
#[derive(Debug, Clone)]
pub enum Signal {
    /// Channel started.
    ChannelStarted { channel_id: String },
    /// Channel ended.
    ChannelEnded { channel_id: String },
    /// Memory was saved.
    MemorySaved {
        memory_type: String,
        content_summary: String,
        importance: f32,
    },
    /// Worker completed.
    WorkerCompleted {
        task_summary: String,
        result_summary: String,
    },
    /// Compaction occurred.
    Compaction {
        channel_id: String,
        turns_compacted: i64,
    },
    /// Error occurred.
    Error {
        component: String,
        error_summary: String,
    },
}

impl Cortex {
    /// Create a new cortex.
    pub fn new(deps: AgentDeps, system_prompt: impl Into<String>) -> Self {
        let hook = CortexHook::new();
        
        Self {
            deps,
            hook,
            signal_buffer: Arc::new(RwLock::new(Vec::with_capacity(100))),
            system_prompt: system_prompt.into(),
        }
    }
    
    /// Process a process event and extract signals.
    pub async fn observe(&self, event: ProcessEvent) {
        let signal = match &event {
            ProcessEvent::MemorySaved { memory_id, .. } => {
                // Load memory to get details
                // For now, simplified
                Some(Signal::MemorySaved {
                    memory_type: "unknown".into(),
                    content_summary: format!("memory {}", memory_id),
                    importance: 0.5,
                })
            }
            ProcessEvent::WorkerComplete { result, .. } => {
                Some(Signal::WorkerCompleted {
                    task_summary: "completed task".into(),
                    result_summary: result.lines().next().unwrap_or("done").into(),
                })
            }
            ProcessEvent::CompactionTriggered { channel_id, threshold_reached, .. } => {
                Some(Signal::Compaction {
                    channel_id: channel_id.to_string(),
                    turns_compacted: (*threshold_reached * 100.0) as i64,
                })
            }
            _ => None,
        };
        
        if let Some(signal) = signal {
            let mut buffer = self.signal_buffer.write().await;
            
            // Add signal
            buffer.push(signal);
            
            // Keep buffer at max 100 signals
            if buffer.len() > 100 {
                buffer.remove(0);
            }
            
            tracing::debug!("cortex received signal, buffer size: {}", buffer.len());
        }
    }
    
    /// Run periodic consolidation.
    pub async fn run_consolidation(&self) -> Result<()> {
        // In real implementation:
        // 1. Analyze signal buffer for patterns
        // 2. Check for duplicate/similar memories
        // 3. Run maintenance tasks
        // 4. Generate observations
        
        tracing::info!("cortex running consolidation");
        
        Ok(())
    }
}
