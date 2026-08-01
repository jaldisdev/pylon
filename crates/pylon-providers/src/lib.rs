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
