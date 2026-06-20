//! Memory storage and retrieval system.

pub mod embedding;
pub mod lance;
pub mod maintenance;
pub mod search;
pub mod store;
#[cfg(feature = "surreal-memory")]
pub mod surreal_maintenance;
#[cfg(feature = "surreal-memory")]
pub mod surreal_migrate;
#[cfg(feature = "surreal-memory")]
pub mod surreal_search;
#[cfg(feature = "surreal-memory")]
pub mod surreal_store;
pub mod types;
pub mod working;

pub use embedding::EmbeddingModel;
pub use lance::EmbeddingTable;
pub use search::{MemorySearch, SearchConfig, SearchMode, SearchSort, curate_results};
pub use store::MemoryStore;
#[cfg(feature = "surreal-memory")]
pub use surreal_search::SurrealMemorySearch;
#[cfg(feature = "surreal-memory")]
pub use surreal_store::SurrealMemoryStore;
pub use types::{Association, Memory, MemoryType, RelationType};
pub use working::{WorkingMemoryEventType, WorkingMemoryStore};
