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

//! pyo3 binding for `vector::search`'s text-overload inference plan: embeds
//! the query text via the configured model provider. Closes the "Python
//! does a live HTTP call" step of the inference-plan path onto the same
//! Rust HTTP client `pylon-providers` gives the vector-index worker —
//! `pylon.toml` model-config resolution (which named `[models.*]` entry to
//! use) stays in Python and is passed down as plain, already-resolved
//! `api_style`/`api_url`/`model`/`api_key` args, matching the plan's
//! design decision to keep one-shot config parsing out of Rust.
//!
//! `fts::search`'s remote-backend variant (Meilisearch/OpenSearch) isn't
//! touched here — it still goes through the Python path in `client.py`
//! until the search-worker phase adds a native Rust HTTP client for those
//! two services.

use pyo3::exceptions::{PyNotImplementedError, PyRuntimeError};
use pyo3::prelude::*;

fn providers_err(err: pylon_providers::Error) -> PyErr {
    PyRuntimeError::new_err(format!("model provider request failed: {err}"))
}

/// Returns the embedding vector for `text`. Mirrors `_make_provider(...).embed(text)`:
/// `AnthropicProvider` has no embeddings endpoint, so `api_style="anthropic"` raises
/// the same `NotImplementedError` the Python base class raises for that case.
#[pyfunction]
#[pyo3(signature = (api_style, api_url, model, text, api_key=None))]
fn embed_text<'py>(
    py: Python<'py>,
    api_style: String,
    api_url: String,
    model: String,
    text: String,
    api_key: Option<String>,
) -> PyResult<Bound<'py, PyAny>> {
    pyo3_async_runtimes::tokio::future_into_py(py, async move {
        if api_style == "anthropic" {
            return Err(PyNotImplementedError::new_err(
                "AnthropicProvider does not support embeddings",
            ));
        }
        let provider =
            pylon_providers::OpenAiProvider::new(&api_url, &model, api_key.as_deref()).map_err(providers_err)?;
        let mut batch = provider
            .embed_batch(std::slice::from_ref(&text))
            .await
            .map_err(providers_err)?;
        Ok(batch.pop().expect("embed_batch returns one vector per input text"))
    })
}

pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(embed_text, m)?)?;
    Ok(())
}
