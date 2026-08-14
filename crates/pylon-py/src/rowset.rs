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

//! A query result that hasn't been turned into Python objects yet.
//!
//! Every row the driver decodes, and every row the cache stores, is a
//! `DecodedValue`. Handing those to Python as tuples and dicts and then
//! immediately hydrating them into the user's own classes builds a whole
//! intermediate object tree that nothing ever reads — `hydrate` only indexes
//! positions out of it. Keeping the rows Rust-side until hydration skips
//! that tree entirely, on both the fresh-query and cache-hit paths.
//!
//! It stays indexable and iterable from Python (`__len__`/`__getitem__`)
//! for the callers that genuinely want plain values — the JSON-returning
//! query methods, and tests — which convert lazily, one row at a time.

use pyo3::prelude::*;
use pyo3::types::PyList;

use pylon_value::DecodedValue;

use crate::pgvalue::cached_to_py;

#[pyclass(module = "pylon._core", frozen, sequence)]
pub struct RowSet {
    pub(crate) rows: Vec<DecodedValue>,
}

impl RowSet {
    pub(crate) fn new(rows: Vec<DecodedValue>) -> Self {
        Self { rows }
    }
}

#[pymethods]
impl RowSet {
    fn __len__(&self) -> usize {
        self.rows.len()
    }

    /// Converts one row to plain Python values. Supports negative indices,
    /// matching a list.
    fn __getitem__<'py>(&self, py: Python<'py>, index: isize) -> PyResult<Bound<'py, PyAny>> {
        let len = self.rows.len() as isize;
        let resolved = if index < 0 { index + len } else { index };
        if resolved < 0 || resolved >= len {
            return Err(pyo3::exceptions::PyIndexError::new_err("row index out of range"));
        }
        cached_to_py(py, &self.rows[resolved as usize])
    }

    /// Every row as plain Python values — the representation callers had
    /// before rows stayed Rust-side. Converts the whole set eagerly, so
    /// prefer iteration or `hydrate` where possible.
    fn to_list<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyList>> {
        let items = self
            .rows
            .iter()
            .map(|row| cached_to_py(py, row))
            .collect::<PyResult<Vec<_>>>()?;
        PyList::new(py, items)
    }

    fn __repr__(&self) -> String {
        format!("<RowSet {} rows>", self.rows.len())
    }
}

pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<RowSet>()?;
    Ok(())
}
