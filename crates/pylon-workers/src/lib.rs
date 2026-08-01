//
// This source file is part of the Pylon open source project.
//
// Copyright (c) 2026 Jaldis B.V.
//
// Licensed under the MIT OR Apache-2.0 license (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     https://opensource.org/licenses/MIT
//     https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//

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
