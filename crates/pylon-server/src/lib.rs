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

//! Native Rust HTTP server, run as the standalone `pylon-server` binary —
//! see the project memory `project-rust-server-backlog` for the full
//! migration plan.

pub mod ai_chat;
pub mod config_options;
pub mod json;
pub mod routes;
pub mod schema_json;
pub mod server;
pub mod state;
pub mod static_files;
pub mod to_json;
pub mod workers;

/// Re-exported from the standalone `pylon-config` crate — moved out so a
/// lightweight consumer (`pylon-lsp`) can parse `pylon.toml` without
/// pulling in this crate's much heavier dependency set (hyper, tower,
/// reqwest, chrono, ...). `crate::config`/`crate::error` keep working
/// exactly as before for everything in this crate.
pub use pylon_config::config;
pub use pylon_config::error;

pub use config::{load_config, load_config_at, Config};
pub use error::{Error, Result};
pub use server::run;
pub use workers::WorkerToggles;
