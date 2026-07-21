//! Native ports of Pylon's LISTEN/NOTIFY-driven background workers
//! (`pylon.cache.CacheInvalidationWorker`, `pylon.worker.IndexWorker` and
//! its subclasses) — `PgListener` (`pylon-pgcon`) and `Cache`
//! (`pylon-cache`) are both already pure Rust with no Python dependency,
//! so these loops run entirely natively; `pylon-py` only needs one thin
//! pyo3 entrypoint per worker (or one combined entrypoint, see the
//! `worker start` re-architecture phase) to start them from the CLI.

mod cache_worker;
mod error;
pub mod index_worker;
pub mod metrics;
pub mod search_clients;
mod search_worker;
mod vector_worker;

pub use cache_worker::{CacheInvalidationWorker, NOTIFY_CHANNEL as CACHE_NOTIFY_CHANNEL};
pub use error::{Error, Result};
pub use search_clients::{MeilisearchClient, OpenSearchClient};
pub use search_worker::SearchIndexWorker;
pub use vector_worker::{ProviderConfig, VectorIndexWorker};
