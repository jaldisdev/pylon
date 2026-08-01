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

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error(transparent)]
    Pgcon(#[from] pylon_pgcon::Error),
    #[error("cache error: {0}")]
    Cache(String),
    #[error(transparent)]
    Providers(#[from] pylon_providers::Error),
    #[error("{0}")]
    Decode(String),
    #[error("{0}")]
    Schema(String),
    #[error("{0}")]
    Unsupported(String),
    #[error(transparent)]
    Http(#[from] reqwest::Error),
}

impl From<Box<dyn std::error::Error + Send + Sync>> for Error {
    fn from(e: Box<dyn std::error::Error + Send + Sync>) -> Self {
        Error::Cache(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, Error>;
