//! Native Rust HTTP server backing `pylon serve` — see the project memory
//! `project-rust-server-backlog` for the full migration plan.

pub mod ai_chat;
pub mod config;
pub mod config_options;
pub mod error;
pub mod json;
pub mod routes;
pub mod schema_json;
pub mod server;
pub mod state;
pub mod static_files;
pub mod to_json;

pub use config::{load_config, Config};
pub use error::{Error, Result};
pub use server::run;
