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

//! The crate's error type. Currently only covers config loading (Phase 1);
//! grows a request/serving variant once the hyper server itself lands.

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("could not locate pylon.toml in {0} or any parent directory")]
    TomlNotFound(std::path::PathBuf),
    #[error("failed to read {path}: {source}")]
    Io {
        path: std::path::PathBuf,
        source: std::io::Error,
    },
    #[error("failed to parse {path}: {source}")]
    TomlParse {
        path: std::path::PathBuf,
        source: toml::de::Error,
    },
    #[error("pylon.toml: required section [{0}] is missing or invalid")]
    MissingSection(&'static str),
    #[error("pylon.toml: [{section}] requires '{field}'")]
    MissingField { section: &'static str, field: &'static str },
    #[error("{0}")]
    Invalid(String),
}

pub type Result<T> = std::result::Result<T, Error>;
