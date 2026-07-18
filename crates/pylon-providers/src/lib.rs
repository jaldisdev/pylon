//! HTTP embedding/chat provider clients — a leaf crate with no dependency
//! on `pylon-core`/pyo3, mirroring `pylon-pgcon`/`pylon-value`/`pylon-cache`.
//! Ports `pylon.vector.models.openai`/`anthropic` 1:1 (same endpoints,
//! headers, batching, timeouts, and — deliberately — the same absence of
//! retry/backoff/streaming logic).

mod anthropic;
mod error;
mod message;
mod openai;

pub use anthropic::AnthropicProvider;
pub use error::{Error, Result};
pub use message::Message;
pub use openai::OpenAiProvider;
