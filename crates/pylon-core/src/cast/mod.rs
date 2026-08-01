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

use crate::stdlib::PylonType;
use std::sync::OnceLock;

mod matrix;

// ── Cast strategy ─────────────────────────────────────────────────────────────

/// How the transpiler emits a `<type>expr` cast.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CastStrategy {
    /// Inserted silently by the type checker; user never writes `<type>expr`.
    Implicit,
    /// Transpiler emits `expr::pg_type` — no `_pylon` function required.
    Sql(&'static str),
    /// Transpiler resolves to the named stdlib function and uses its `ImplStrategy`.
    /// The string is the unqualified function name, e.g. `"to_int16"`.
    Function(&'static str),
}

// ── Cast entry ────────────────────────────────────────────────────────────────

/// One entry in the closed cast whitelist.
///
/// Any `(source, target)` pair absent from the matrix is a compile-time type
/// error — the type checker rejects it before SQL is emitted.
#[derive(Debug, Clone)]
pub struct CastEntry {
    pub source: PylonType,
    pub target: PylonType,
    pub strategy: CastStrategy,
}

// ── Static matrix ─────────────────────────────────────────────────────────────

static CAST_MATRIX: OnceLock<Vec<CastEntry>> = OnceLock::new();

/// Return the full cast matrix, initializing it on first call.
pub fn cast_matrix() -> &'static [CastEntry] {
    CAST_MATRIX.get_or_init(matrix::build)
}

/// Look up a `(source, target)` pair. Returns `None` for unknown pairs — the
/// transpiler must treat those as compile-time type errors.
pub fn lookup_cast(source: &PylonType, target: &PylonType) -> Option<&'static CastEntry> {
    cast_matrix()
        .iter()
        .find(|e| &e.source == source && &e.target == target)
}

/// True when the cast is inserted silently by the type checker.
pub fn is_implicit(source: &PylonType, target: &PylonType) -> bool {
    matches!(
        lookup_cast(source, target),
        Some(e) if e.strategy == CastStrategy::Implicit
    )
}
