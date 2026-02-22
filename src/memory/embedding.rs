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

#[cfg(test)]
mod tests {
    use super::*;

    /// Test that identical vectors have cosine similarity of 1.0.
    #[test]
    fn test_cosine_similarity_identical() {
        let vector = vec![1.0, 2.0, 3.0, 4.0];
        let similarity = cosine_similarity(&vector, &vector);
        assert!(
            (similarity - 1.0).abs() < 1e-6,
            "Expected 1.0 for identical vectors, got {}",
            similarity
        );
    }

    /// Test that orthogonal vectors have cosine similarity of 0.0.
    #[test]
    fn test_cosine_similarity_orthogonal() {
        // (1, 0) and (0, 1) are orthogonal
        let a = vec![1.0, 0.0];
        let b = vec![0.0, 1.0];
        let similarity = cosine_similarity(&a, &b);
        assert!(
            similarity.abs() < 1e-6,
            "Expected 0.0 for orthogonal vectors, got {}",
            similarity
        );
    }

    /// Test that opposite vectors have cosine similarity of -1.0.
    #[test]
    fn test_cosine_similarity_opposite() {
        let a = vec![1.0, 2.0, 3.0];
        let b = vec![-1.0, -2.0, -3.0];
        let similarity = cosine_similarity(&a, &b);
        assert!(
            (similarity - (-1.0)).abs() < 1e-6,
            "Expected -1.0 for opposite vectors, got {}",
            similarity
        );
    }

    /// Test that semantic deduplication filters embeddings above the threshold.
    #[test]
    fn test_deduplication_semantic() {
        // Create a base embedding
        let base = vec![1.0, 0.0, 0.0];

        // Create a buffer with an embedding that's similar (cosine = 0.8)
        // (1, 0, 0) dot (1, 0.6, 0) = 1, magnitude_a = 1, magnitude_b = sqrt(1.36) ≈ 1.166
        // cosine = 1 / 1.166 ≈ 0.857
        let similar = vec![1.0, 0.6, 0.0];
        let buffer = vec![similar.clone()];

        // With threshold 0.8, the similar embedding should be filtered (0.857 > 0.8)
        assert!(
            is_semantically_duplicate(&base, &buffer, 0.8),
            "Expected base to be detected as duplicate of similar embedding with threshold 0.8"
        );

        // With threshold 0.9, it should NOT be filtered (0.857 <= 0.9)
        assert!(
            !is_semantically_duplicate(&base, &buffer, 0.9),
            "Expected base to NOT be detected as duplicate with threshold 0.9"
        );

        // Test with orthogonal embedding (should never be filtered)
        let orthogonal = vec![0.0, 1.0, 0.0];
        let buffer_orthogonal = vec![orthogonal];
        assert!(
            !is_semantically_duplicate(&base, &buffer_orthogonal, 0.1),
            "Expected orthogonal embedding to not be duplicate"
        );
    }

    /// Test edge cases for cosine similarity.
    #[test]
    fn test_cosine_similarity_edge_cases() {
        // Empty vectors
        assert_eq!(cosine_similarity(&[], &[]), 0.0);

        // Different lengths
        assert_eq!(cosine_similarity(&[1.0], &[1.0, 2.0]), 0.0);

        // Zero vectors
        assert_eq!(cosine_similarity(&[0.0, 0.0], &[1.0, 2.0]), 0.0);
        assert_eq!(cosine_similarity(&[1.0, 2.0], &[0.0, 0.0]), 0.0);
    }
}
