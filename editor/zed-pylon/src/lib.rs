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

//! Zed extension that registers `pylon-lsp` as an additional language
//! server for Python buffers (alongside whatever primary Python server is
//! already configured, e.g. basedpyright/ruff) — it republishes
//! `pylon_core`'s parse diagnostics for embedded PyQL query strings.
//!
//! v1: looks the `pylon-lsp` binary up on `PATH` (via `cargo install
//! --path crates/pylon-lsp` or an equivalent local build) rather than
//! downloading a prebuilt release — packaged binary distribution is a
//! separate, later effort (see the design plan).

use zed_extension_api::{self as zed, LanguageServerId, Result};

struct PylonExtension;

impl zed::Extension for PylonExtension {
    fn new() -> Self {
        PylonExtension
    }

    fn language_server_command(
        &mut self,
        _language_server_id: &LanguageServerId,
        worktree: &zed::Worktree,
    ) -> Result<zed::Command> {
        let path = worktree.which("pylon-lsp").ok_or_else(|| {
            "pylon-lsp binary not found on PATH. Build it with \
             `cargo build --release -p pylon-lsp` and add \
             target/release/pylon-lsp to your PATH."
                .to_string()
        })?;

        Ok(zed::Command { command: path, args: vec![], env: vec![] })
    }
}

zed::register_extension!(PylonExtension);
