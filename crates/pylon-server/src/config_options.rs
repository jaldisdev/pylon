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

//! Registry of known Pylon session config options — Rust port of
//! `pylon/config_options.py`. Backs `GET /api/config-options`.

pub struct ConfigOptionSpec {
    pub name: &'static str,
    pub type_name: &'static str,
    pub default: bool,
}

/// The only session config option `pylon_core::ir::SessionConfig` knows
/// today. Adding a new one: add it here, thread it through
/// `pylon-client`'s param-binding/`SessionConfig`, and add it here.
pub const CONFIG_OPTIONS: &[ConfigOptionSpec] = &[ConfigOptionSpec {
    name: "allow_user_specified_id",
    type_name: "bool",
    default: false,
}];
