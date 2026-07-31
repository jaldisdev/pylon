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

pub use config::{load_config, Config};
pub use error::{Error, Result};
pub use server::run;
