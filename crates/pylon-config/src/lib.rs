//! `pylon.toml` parsing, with no Python involved — see `config`'s own doc
//! comment for the port's exact scope. Split out from `pylon-server` so
//! that a lightweight consumer (`pylon-lsp`, which only ever needs the
//! `[database]` DSN) doesn't have to pull in the rest of that crate's
//! dependencies (hyper, tower, reqwest, chrono, ...) just to parse a TOML
//! file.

pub mod config;
pub mod error;
