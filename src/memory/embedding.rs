//! Embedding generation via fastembed.

use crate::error::{LlmError, Result};
use std::path::Path;
use std::sync::Arc;

/// Embedding model wrapper with thread-safe sharing.
///
/// fastembed's TextEmbedding is not Send, so we hold it behind an Arc and
/// use spawn_blocking to call into it from async contexts.
pub struct EmbeddingModel {
    model: Arc<fastembed::TextEmbedding>,
}

impl EmbeddingModel {
    /// Create a new embedding model, storing downloaded model files in `cache_dir`.
    pub fn new(cache_dir: &Path) -> Result<Self> {
        let options = fastembed::InitOptions::default()
            .with_cache_dir(cache_dir.to_path_buf())
            .with_show_download_progress(true);

        let model = fastembed::TextEmbedding::try_new(options)
            .map_err(|e| LlmError::EmbeddingFailed(e.to_string()))?;

        Ok(Self {
            model: Arc::new(model),
        })
    }

    /// Generate embeddings for multiple texts (blocking).
    pub fn embed(&self, texts: Vec<String>) -> Result<Vec<Vec<f32>>> {
        self.model
            .embed(texts, None)
            .map_err(|e| LlmError::EmbeddingFailed(e.to_string()).into())
    }

    /// Generate embedding for a single text (blocking).
    pub fn embed_one_blocking(&self, text: &str) -> Result<Vec<f32>> {
        let embeddings = self.embed(vec![text.to_string()])?;
        Ok(embeddings.into_iter().next().unwrap_or_default())
    }

    /// Generate embedding for a single text (async, spawns blocking task).
    pub async fn embed_one(self: &Arc<Self>, text: &str) -> Result<Vec<f32>> {
        let text = text.to_string();
        let model = self.model.clone();
        let result = tokio::task::spawn_blocking(move || {
            model.embed(vec![text], None).map_err(|e| {
                crate::Error::from(crate::error::LlmError::EmbeddingFailed(e.to_string()))
            })
        })
        .await
        .map_err(|e| crate::Error::Other(anyhow::anyhow!("embedding task failed: {}", e)))??;

        Ok(result.into_iter().next().unwrap_or_default())
    }
}

/// Async function to embed text using a shared model.
pub async fn embed_text(model: &Arc<EmbeddingModel>, text: &str) -> Result<Vec<f32>> {
    model.embed_one(text).await
}

/// Compute cosine similarity between two embedding vectors.
///
/// Returns a value in [-1, 1] where:
/// - 1.0 means identical direction
/// - 0.0 means orthogonal
/// - -1.0 means opposite direction
///
/// Returns 0.0 if either vector is empty or has zero magnitude.
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.is_empty() || b.is_empty() || a.len() != b.len() {
        return 0.0;
    }

    let dot_product: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let magnitude_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let magnitude_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();

    if magnitude_a == 0.0 || magnitude_b == 0.0 {
        return 0.0;
    }

    dot_product / (magnitude_a * magnitude_b)
}

/// Check if an embedding is semantically similar to any in a buffer.
///
/// Returns true if the maximum cosine similarity with any buffer embedding
/// exceeds the threshold.
pub fn is_semantically_duplicate<'a, B>(embedding: &[f32], buffer: B, threshold: f32) -> bool
where
    B: IntoIterator<Item = &'a Vec<f32>>,
{
    buffer
        .into_iter()
        .any(|buffer_embedding| cosine_similarity(embedding, buffer_embedding) > threshold)
}
