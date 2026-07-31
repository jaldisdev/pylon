//! Native Rust HTTP server backing `pylon serve` — see the project memory
//! `project-rust-server-backlog` for the full migration plan. Phase 1
//! (this crate's current state) is just the `pylon.toml` config parser;
//! the hyper server itself lands in a later phase.

pub mod config;
pub mod error;
pub mod json;
pub mod server;

pub use config::{load_config, Config};
pub use error::{Error, Result};
pub use server::run;
