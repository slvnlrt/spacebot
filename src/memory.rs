//! Memory storage and retrieval system.

pub mod embedding;
pub mod lance;
pub mod maintenance;
pub mod search;
pub mod store;
pub mod types;

pub use embedding::{cosine_similarity, is_semantically_duplicate, EmbeddingModel};
pub use lance::EmbeddingTable;
pub use search::{curate_results, MemorySearch, SearchConfig, SearchMode, SearchSort};
pub use store::MemoryStore;
pub use types::{Association, Memory, MemoryType, RelationType};
