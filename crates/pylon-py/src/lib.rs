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

use pyo3::prelude::*;
use pyo3::PyTypeInfo;
use pylon_core as core;
use std::collections::HashMap;

mod cache;
mod introspect;
mod migrate;
mod pgcon;
mod pgvalue;
mod providers;
mod workers;

// ── Exception hierarchy ────────────────────────────────────────────────────────
//
// `pylon.exceptions.*` (Python) is what actually gets raised — `pyql_err`/
// `pgcon_err` construct those by name — so only the two classes below that
// are genuinely raised from Rust by their own type (cache/pgcon usage
// errors, which have no Python-side equivalent) are registered here.

pyo3::create_exception!(
    pylon._core,
    PylonCacheError,
    pyo3::exceptions::PyException,
    "Raised on a cache storage failure (LMDB, serialization, or value encoding)."
);
pyo3::create_exception!(
    pylon._core,
    PylonPgconError,
    pyo3::exceptions::PyException,
    "Raised for pgcon usage errors that aren't a Postgres response at all \
     (e.g. calling a method on an already committed/rolled-back \
     transaction) — real Postgres errors are mapped to pylon.exceptions.* \
     instead (see pgcon_err in pgcon.rs)."
);

// ── Deletion policy ────────────────────────────────────────────────────────────

#[pyclass(module = "pylon._core", frozen)]
pub struct OnDeletePolicy {
    inner: core::schema::OnDeletePolicy,
}

#[pymethods]
impl OnDeletePolicy {
    #[new]
    fn new(side: &str, action: &str) -> PyResult<Self> {
        let side = match side {
            "Target" => core::schema::DeleteSide::Target,
            "Source" => core::schema::DeleteSide::Source,
            _ => return Err(pyo3::exceptions::PyValueError::new_err(
                format!("Unknown deletion side: {side:?}; expected 'Target' or 'Source'")
            )),
        };
        let action = match action {
            "Allow" => core::schema::DeleteAction::Allow,
            "Restrict" => core::schema::DeleteAction::Restrict,
            "DeferredRestrict" => core::schema::DeleteAction::DeferredRestrict,
            "DeleteSource" => core::schema::DeleteAction::DeleteSource,
            "DeleteTarget" => core::schema::DeleteAction::DeleteTarget,
            "DeleteTargetIfOrphan" => core::schema::DeleteAction::DeleteTargetIfOrphan,
            _ => return Err(pyo3::exceptions::PyValueError::new_err(
                format!("Unknown deletion action: {action:?}")
            )),
        };
        Ok(Self { inner: core::schema::OnDeletePolicy { side, action } })
    }

    #[getter]
    fn side(&self) -> &str {
        match self.inner.side {
            core::schema::DeleteSide::Target => "Target",
            core::schema::DeleteSide::Source => "Source",
        }
    }

    #[getter]
    fn action(&self) -> &str {
        match self.inner.action {
            core::schema::DeleteAction::Allow => "Allow",
            core::schema::DeleteAction::Restrict => "Restrict",
            core::schema::DeleteAction::DeferredRestrict => "DeferredRestrict",
            core::schema::DeleteAction::DeleteSource => "DeleteSource",
            core::schema::DeleteAction::DeleteTarget => "DeleteTarget",
            core::schema::DeleteAction::DeleteTargetIfOrphan => "DeleteTargetIfOrphan",
        }
    }
}

// ── Mutation rewrite ───────────────────────────────────────────────────────────

#[pyclass(module = "pylon._core", frozen)]
pub struct RewriteEntry {
    inner: core::schema::RewriteEntry,
}

#[pymethods]
impl RewriteEntry {
    #[new]
    fn new(on: u8, handler: String) -> Self {
        Self {
            inner: core::schema::RewriteEntry { on, handler },
        }
    }

    #[getter]
    fn on(&self) -> u8 {
        self.inner.on
    }

    #[getter]
    fn handler(&self) -> &str {
        &self.inner.handler
    }
}

// ── Post-commit signal registration ─────────────────────────────────────────────

#[pyclass(module = "pylon._core", frozen)]
pub struct SignalEntry {
    inner: core::schema::SignalEntry,
}

#[pymethods]
impl SignalEntry {
    #[new]
    fn new(on: u8) -> Self {
        Self {
            inner: core::schema::SignalEntry { on },
        }
    }

    #[getter]
    fn on(&self) -> u8 {
        self.inner.on
    }
}

// ── Tuple member descriptor (recursive) ────────────────────────────────────────

/// One member's type within a named-tuple/tuple-shaped value. `kind` selects
/// which of `pg_type` (scalar) / `module`+`type_name` (enum, nominal named
/// tuple) / `members` (nested tuple) is meaningful.
#[pyclass(module = "pylon._core", frozen)]
pub struct TupleMember {
    inner: core::schema::TupleMemberDescriptor,
}

#[pymethods]
impl TupleMember {
    #[new]
    #[pyo3(signature = (
        name,
        kind,
        *,
        pg_type = None,
        module = None,
        type_name = None,
        members = None,
    ))]
    fn new(
        name: Option<String>,
        kind: &str,
        pg_type: Option<String>,
        module: Option<String>,
        type_name: Option<String>,
        members: Option<Vec<PyRef<TupleMember>>>,
    ) -> PyResult<Self> {
        let kind = match kind {
            "scalar" => core::schema::TupleMemberKind::Scalar {
                pg_type: pg_type.ok_or_else(|| {
                    pyo3::exceptions::PyValueError::new_err("TupleMember(kind='scalar') requires pg_type")
                })?,
            },
            "enum" => core::schema::TupleMemberKind::Enum {
                module: module.ok_or_else(|| {
                    pyo3::exceptions::PyValueError::new_err("TupleMember(kind='enum') requires module")
                })?,
                name: type_name.ok_or_else(|| {
                    pyo3::exceptions::PyValueError::new_err("TupleMember(kind='enum') requires type_name")
                })?,
            },
            "namedTuple" => core::schema::TupleMemberKind::NamedTuple {
                module: module.ok_or_else(|| {
                    pyo3::exceptions::PyValueError::new_err("TupleMember(kind='namedTuple') requires module")
                })?,
                name: type_name.ok_or_else(|| {
                    pyo3::exceptions::PyValueError::new_err("TupleMember(kind='namedTuple') requires type_name")
                })?,
            },
            "tuple" => core::schema::TupleMemberKind::Tuple {
                members: members.unwrap_or_default().iter().map(|m| m.inner.clone()).collect(),
            },
            other => {
                return Err(pyo3::exceptions::PyValueError::new_err(format!(
                    "unknown TupleMember kind '{other}' (expected scalar/enum/namedTuple/tuple)"
                )))
            }
        };
        Ok(Self { inner: core::schema::TupleMemberDescriptor { name, kind } })
    }

    #[getter]
    fn name(&self) -> Option<&str> {
        self.inner.name.as_deref()
    }
}

// ── Pointer descriptors ────────────────────────────────────────────────────────

#[pyclass(module = "pylon._core", frozen)]
pub struct PropertyDescriptor {
    inner: core::schema::PropertyDescriptor,
}

#[pymethods]
impl PropertyDescriptor {
    #[new]
    #[pyo3(signature = (
        name,
        pg_type,
        nullable,
        *,
        default_sql = None,
        default_pyql = None,
        description = None,
        check_constraints = None,
        is_exclusive = false,
        is_pk = false,
        is_readonly = false,
        rewrites = None,
        tuple_members = None,
        column_type = None
    ))]
    fn new(
        name: String,
        pg_type: String,
        nullable: bool,
        default_sql: Option<String>,
        default_pyql: Option<String>,
        description: Option<String>,
        check_constraints: Option<Vec<String>>,
        is_exclusive: bool,
        is_pk: bool,
        is_readonly: bool,
        rewrites: Option<Vec<PyRef<RewriteEntry>>>,
        tuple_members: Option<Vec<PyRef<TupleMember>>>,
        column_type: Option<String>,
    ) -> Self {
        Self {
            inner: core::schema::PropertyDescriptor {
                name,
                pg_type,
                nullable,
                default_sql,
                default_pyql,
                description,
                check_constraints: check_constraints.unwrap_or_default(),
                is_exclusive,
                is_pk,
                is_readonly,
                rewrites: rewrites
                    .unwrap_or_default()
                    .iter()
                    .map(|r| r.inner.clone())
                    .collect(),
                tuple_members: tuple_members
                    .map(|ms| ms.iter().map(|m| m.inner.clone()).collect()),
                column_type,
            },
        }
    }

    #[getter]
    fn name(&self) -> &str {
        &self.inner.name
    }

    #[getter]
    fn pg_type(&self) -> &str {
        &self.inner.pg_type
    }

    #[getter]
    fn nullable(&self) -> bool {
        self.inner.nullable
    }

    #[getter]
    fn default_sql(&self) -> Option<&str> {
        self.inner.default_sql.as_deref()
    }

    #[getter]
    fn description(&self) -> Option<&str> {
        self.inner.description.as_deref()
    }

    #[getter]
    fn check_constraints(&self) -> Vec<String> {
        self.inner.check_constraints.clone()
    }

    #[getter]
    fn is_exclusive(&self) -> bool {
        self.inner.is_exclusive
    }

    #[getter]
    fn is_pk(&self) -> bool {
        self.inner.is_pk
    }

    #[getter]
    fn is_readonly(&self) -> bool {
        self.inner.is_readonly
    }

    #[getter]
    fn column_type(&self) -> Option<&str> {
        self.inner.column_type.as_deref()
    }
}

#[pyclass(module = "pylon._core", frozen)]
pub struct LinkDescriptor {
    inner: core::schema::LinkDescriptor,
}

#[pymethods]
impl LinkDescriptor {
    #[new]
    #[pyo3(signature = (
        name,
        target,
        nullable,
        *,
        through = None,
        default_pyql = None,
        description = None,
        is_exclusive = false,
        is_readonly = false,
        rewrites = None,
        on_delete = None
    ))]
    fn new(
        name: String,
        target: String,
        nullable: bool,
        through: Option<String>,
        default_pyql: Option<String>,
        description: Option<String>,
        is_exclusive: bool,
        is_readonly: bool,
        rewrites: Option<Vec<PyRef<RewriteEntry>>>,
        on_delete: Option<Vec<PyRef<OnDeletePolicy>>>,
    ) -> Self {
        Self {
            inner: core::schema::LinkDescriptor {
                name,
                target,
                nullable,
                through,
                default_pyql,
                description,
                is_exclusive,
                is_readonly,
                rewrites: rewrites
                    .unwrap_or_default()
                    .iter()
                    .map(|r| r.inner.clone())
                    .collect(),
                on_delete: on_delete
                    .unwrap_or_default()
                    .iter()
                    .map(|p| p.inner.clone())
                    .collect(),
            },
        }
    }

    #[getter]
    fn name(&self) -> &str {
        &self.inner.name
    }

    #[getter]
    fn target(&self) -> &str {
        &self.inner.target
    }

    #[getter]
    fn nullable(&self) -> bool {
        self.inner.nullable
    }

    #[getter]
    fn through(&self) -> Option<&str> {
        self.inner.through.as_deref()
    }

    #[getter]
    fn description(&self) -> Option<&str> {
        self.inner.description.as_deref()
    }

    #[getter]
    fn is_exclusive(&self) -> bool {
        self.inner.is_exclusive
    }

    #[getter]
    fn is_readonly(&self) -> bool {
        self.inner.is_readonly
    }
}

#[pyclass(module = "pylon._core", frozen)]
pub struct MultiLinkDescriptor {
    inner: core::schema::MultiLinkDescriptor,
}

#[pymethods]
impl MultiLinkDescriptor {
    #[new]
    #[pyo3(signature = (
        name,
        target,
        *,
        through = None,
        nullable = false,
        default_pyql = None,
        description = None,
        on_delete = None
    ))]
    fn new(
        name: String,
        target: String,
        through: Option<String>,
        nullable: bool,
        default_pyql: Option<String>,
        description: Option<String>,
        on_delete: Option<Vec<PyRef<OnDeletePolicy>>>,
    ) -> Self {
        Self {
            inner: core::schema::MultiLinkDescriptor {
                name,
                target,
                through,
                nullable,
                default_pyql,
                description,
                on_delete: on_delete
                    .unwrap_or_default()
                    .iter()
                    .map(|p| p.inner.clone())
                    .collect(),
            },
        }
    }

    #[getter]
    fn name(&self) -> &str {
        &self.inner.name
    }

    #[getter]
    fn target(&self) -> &str {
        &self.inner.target
    }

    #[getter]
    fn through(&self) -> Option<&str> {
        self.inner.through.as_deref()
    }

    #[getter]
    fn nullable(&self) -> bool {
        self.inner.nullable
    }

    #[getter]
    fn description(&self) -> Option<&str> {
        self.inner.description.as_deref()
    }
}

#[pyclass(module = "pylon._core", frozen)]
pub struct ComputedDescriptor {
    inner: core::schema::ComputedDescriptor,
}

#[pymethods]
impl ComputedDescriptor {
    #[new]
    #[pyo3(signature = (name, expression, *, return_type = None))]
    fn new(name: String, expression: String, return_type: Option<String>) -> Self {
        Self {
            inner: core::schema::ComputedDescriptor {
                name,
                expression,
                return_type,
            },
        }
    }

    #[getter]
    fn name(&self) -> &str {
        &self.inner.name
    }

    #[getter]
    fn expression(&self) -> &str {
        &self.inner.expression
    }

    #[getter]
    fn return_type(&self) -> Option<&str> {
        self.inner.return_type.as_deref()
    }
}

// ── Type-level constructs ──────────────────────────────────────────────────────

#[pyclass(module = "pylon._core", frozen)]
pub struct IndexDescriptor {
    inner: core::schema::IndexDescriptor,
}

#[pymethods]
impl IndexDescriptor {
    #[new]
    #[pyo3(signature = (
        pointers,
        *,
        expression = None,
        unique = false,
        unless = None
    ))]
    fn new(
        pointers: Vec<String>,
        expression: Option<String>,
        unique: bool,
        unless: Option<String>,
    ) -> Self {
        Self {
            inner: core::schema::IndexDescriptor {
                pointers,
                expression,
                unique,
                unless,
            },
        }
    }

    #[getter]
    fn pointers(&self) -> Vec<String> {
        self.inner.pointers.clone()
    }

    #[getter]
    fn expression(&self) -> Option<&str> {
        self.inner.expression.as_deref()
    }

    #[getter]
    fn unique(&self) -> bool {
        self.inner.unique
    }

    #[getter]
    fn unless(&self) -> Option<&str> {
        self.inner.unless.as_deref()
    }
}

#[pyclass(module = "pylon._core", frozen)]
pub struct VectorIndexDescriptor {
    inner: core::schema::VectorIndexDescriptor,
}

#[pymethods]
impl VectorIndexDescriptor {
    #[new]
    #[pyo3(signature = (pointers, model, metric, dimensions, *, index_name = None))]
    fn new(
        pointers: Vec<String>,
        model: String,
        metric: String,
        dimensions: u32,
        index_name: Option<String>,
    ) -> Self {
        Self {
            inner: core::schema::VectorIndexDescriptor {
                index_name,
                pointers,
                model,
                metric,
                dimensions,
            },
        }
    }

    #[getter]
    fn index_name(&self) -> Option<&str> { self.inner.index_name.as_deref() }
    #[getter]
    fn pointers(&self) -> Vec<String> { self.inner.pointers.clone() }
    #[getter]
    fn model(&self) -> &str { &self.inner.model }
    #[getter]
    fn metric(&self) -> &str { &self.inner.metric }
    #[getter]
    fn dimensions(&self) -> u32 { self.inner.dimensions }
    #[getter]
    fn column_name(&self) -> String { self.inner.column_name() }
}

#[pyclass(module = "pylon._core", frozen)]
pub struct SearchPointerDescriptor {
    inner: core::schema::SearchPointerDescriptor,
}

#[pymethods]
impl SearchPointerDescriptor {
    #[new]
    fn new(name: String, weight: String) -> Self {
        let w = match weight.as_str() {
            "B" => core::schema::SearchWeight::B,
            "C" => core::schema::SearchWeight::C,
            "D" => core::schema::SearchWeight::D,
            _ => core::schema::SearchWeight::A,
        };
        Self { inner: core::schema::SearchPointerDescriptor { name, weight: w } }
    }
    #[getter]
    fn name(&self) -> &str { &self.inner.name }
    #[getter]
    fn weight(&self) -> &str { self.inner.weight.as_str() }
}

#[pyclass(module = "pylon._core", frozen)]
pub struct SearchIndexDescriptor {
    pub(crate) inner: core::schema::SearchIndexDescriptor,
}

#[pymethods]
impl SearchIndexDescriptor {
    #[new]
    #[pyo3(signature = (backend, pointers, *, index_name = None))]
    fn new(
        backend: String,
        pointers: Vec<PyRef<SearchPointerDescriptor>>,
        index_name: Option<String>,
    ) -> Self {
        let b = match backend.as_str() {
            "OpenSearch" => core::schema::SearchBackend::OpenSearch,
            "Meilisearch" => core::schema::SearchBackend::Meilisearch,
            _ => core::schema::SearchBackend::Postgres,
        };
        Self {
            inner: core::schema::SearchIndexDescriptor {
                index_name,
                backend: b,
                pointers: pointers.iter().map(|f| f.inner.clone()).collect(),
            },
        }
    }
    #[getter]
    fn backend(&self) -> &str {
        match &self.inner.backend {
            core::schema::SearchBackend::Postgres => "Postgres",
            core::schema::SearchBackend::OpenSearch => "OpenSearch",
            core::schema::SearchBackend::Meilisearch => "Meilisearch",
        }
    }
    #[getter]
    fn pointers(&self) -> Vec<SearchPointerDescriptor> {
        self.inner.pointers.iter().map(|f| SearchPointerDescriptor { inner: f.clone() }).collect()
    }
    #[getter]
    fn index_name(&self) -> Option<&str> { self.inner.index_name.as_deref() }
    #[getter]
    fn column_name(&self) -> String { self.inner.column_name() }
}

#[pyclass(module = "pylon._core", frozen)]
pub struct TriggerDescriptor {
    inner: core::schema::TriggerDescriptor,
}

#[pymethods]
impl TriggerDescriptor {
    #[new]
    fn new(on: u8, timing: String, handler: String) -> Self {
        Self {
            inner: core::schema::TriggerDescriptor { on, timing, handler },
        }
    }

    #[getter]
    fn on(&self) -> u8 {
        self.inner.on
    }

    #[getter]
    fn timing(&self) -> &str {
        &self.inner.timing
    }

    #[getter]
    fn handler(&self) -> &str {
        &self.inner.handler
    }
}

/// Composite UNIQUE constraint across multiple pointers.
#[pyclass(module = "pylon._core", frozen)]
pub struct ExclusiveConstraint {
    inner: core::schema::TypeConstraint,
}

#[pymethods]
impl ExclusiveConstraint {
    #[new]
    #[pyo3(signature = (pointers, *, unless = None))]
    fn new(pointers: Vec<String>, unless: Option<String>) -> Self {
        Self {
            inner: core::schema::TypeConstraint::Exclusive { pointers, unless },
        }
    }

    #[getter]
    fn pointers(&self) -> Vec<String> {
        match &self.inner {
            core::schema::TypeConstraint::Exclusive { pointers, .. } => pointers.clone(),
            _ => unreachable!(),
        }
    }

    #[getter]
    fn unless(&self) -> Option<&str> {
        match &self.inner {
            core::schema::TypeConstraint::Exclusive { unless, .. } => unless.as_deref(),
            _ => unreachable!(),
        }
    }
}

/// Arbitrary CHECK constraint expressed as a PyQL boolean expression.
#[pyclass(module = "pylon._core", frozen)]
pub struct ExpressionConstraint {
    inner: core::schema::TypeConstraint,
}

#[pymethods]
impl ExpressionConstraint {
    #[new]
    fn new(expr: String) -> Self {
        Self {
            inner: core::schema::TypeConstraint::Expression { expr },
        }
    }

    #[getter]
    fn expr(&self) -> &str {
        match &self.inner {
            core::schema::TypeConstraint::Expression { expr } => expr.as_str(),
            _ => unreachable!(),
        }
    }
}

// ── Type descriptor ────────────────────────────────────────────────────────────

#[pyclass(module = "pylon._core", frozen)]
pub struct TypeDescriptor {
    pub(crate) inner: core::schema::TypeDescriptor,
}

#[pymethods]
impl TypeDescriptor {
    #[new]
    #[pyo3(signature = (
        name,
        module,
        table,
        properties,
        links,
        multilinks,
        computed,
        *,
        abstract_ = false,
        materialized = true,
        junction = false,
        description = None,
        parents = None,
        interfaces = None,
        exclusive_constraints = None,
        expression_constraints = None,
        indexes = None,
        vector_indexes = None,
        search_indexes = None,
        triggers = None,
        signals = None
    ))]
    fn new(
        name: String,
        module: String,
        table: String,
        properties: Vec<PyRef<PropertyDescriptor>>,
        links: Vec<PyRef<LinkDescriptor>>,
        multilinks: Vec<PyRef<MultiLinkDescriptor>>,
        computed: Vec<PyRef<ComputedDescriptor>>,
        abstract_: bool,
        materialized: bool,
        junction: bool,
        description: Option<String>,
        parents: Option<Vec<String>>,
        interfaces: Option<Vec<String>>,
        exclusive_constraints: Option<Vec<PyRef<ExclusiveConstraint>>>,
        expression_constraints: Option<Vec<PyRef<ExpressionConstraint>>>,
        indexes: Option<Vec<PyRef<IndexDescriptor>>>,
        vector_indexes: Option<Vec<PyRef<VectorIndexDescriptor>>>,
        search_indexes: Option<Vec<PyRef<SearchIndexDescriptor>>>,
        triggers: Option<Vec<PyRef<TriggerDescriptor>>>,
        signals: Option<Vec<PyRef<SignalEntry>>>,
    ) -> Self {
        let mut constraints: Vec<core::schema::TypeConstraint> = Vec::new();
        for c in exclusive_constraints.unwrap_or_default().iter() {
            constraints.push(c.inner.clone());
        }
        for c in expression_constraints.unwrap_or_default().iter() {
            constraints.push(c.inner.clone());
        }

        Self {
            inner: core::schema::TypeDescriptor {
                name,
                module,
                table,
                abstract_,
                materialized,
                junction,
                description,
                parents: parents.unwrap_or_default(),
                interfaces: interfaces.unwrap_or_default(),
                properties: properties.iter().map(|p| p.inner.clone()).collect(),
                links: links.iter().map(|l| l.inner.clone()).collect(),
                multilinks: multilinks.iter().map(|m| m.inner.clone()).collect(),
                computed: computed.iter().map(|c| c.inner.clone()).collect(),
                constraints,
                indexes: indexes
                    .unwrap_or_default()
                    .iter()
                    .map(|i| i.inner.clone())
                    .collect(),
                vector_indexes: vector_indexes
                    .unwrap_or_default()
                    .iter()
                    .map(|v| v.inner.clone())
                    .collect(),
                search_indexes: search_indexes
                    .unwrap_or_default()
                    .iter()
                    .map(|s| s.inner.clone())
                    .collect(),
                triggers: triggers
                    .unwrap_or_default()
                    .iter()
                    .map(|t| t.inner.clone())
                    .collect(),
                signals: signals
                    .unwrap_or_default()
                    .iter()
                    .map(|s| s.inner.clone())
                    .collect(),
            },
        }
    }

    #[getter]
    fn name(&self) -> &str {
        &self.inner.name
    }

    #[getter]
    fn module(&self) -> &str {
        &self.inner.module
    }

    #[getter]
    fn table(&self) -> &str {
        &self.inner.table
    }

    #[getter]
    fn abstract_(&self) -> bool {
        self.inner.abstract_
    }

    #[getter]
    fn materialized(&self) -> bool {
        self.inner.materialized
    }

    #[getter]
    fn junction(&self) -> bool {
        self.inner.junction
    }

    #[getter]
    fn description(&self) -> Option<&str> {
        self.inner.description.as_deref()
    }

    #[getter]
    fn parents(&self) -> Vec<String> {
        self.inner.parents.clone()
    }

    #[getter]
    fn interfaces(&self) -> Vec<String> {
        self.inner.interfaces.clone()
    }

    #[getter]
    fn multilinks(&self) -> Vec<MultiLinkDescriptor> {
        self.inner.multilinks.iter().map(|m| MultiLinkDescriptor { inner: m.clone() }).collect()
    }

    #[getter]
    fn vector_indexes(&self) -> Vec<VectorIndexDescriptor> {
        self.inner.vector_indexes.iter().map(|v| VectorIndexDescriptor { inner: v.clone() }).collect()
    }

    #[getter]
    fn search_indexes(&self) -> Vec<SearchIndexDescriptor> {
        self.inner.search_indexes.iter().map(|s| SearchIndexDescriptor { inner: s.clone() }).collect()
    }
}

// ── Scalar / enum / global descriptors ────────────────────────────────────────

#[pyclass(module = "pylon._core", frozen)]
pub struct ScalarDescriptor {
    inner: core::schema::ScalarDescriptor,
}

#[pymethods]
impl ScalarDescriptor {
    #[new]
    #[pyo3(signature = (name, module, base, pg_type, *, check_constraints = None, is_sequence = false))]
    fn new(
        name: String,
        module: String,
        base: String,
        pg_type: String,
        check_constraints: Option<Vec<String>>,
        is_sequence: bool,
    ) -> Self {
        Self {
            inner: core::schema::ScalarDescriptor {
                name,
                module,
                base,
                pg_type,
                check_constraints: check_constraints.unwrap_or_default(),
                is_sequence,
            },
        }
    }

    #[getter]
    fn name(&self) -> &str {
        &self.inner.name
    }

    #[getter]
    fn module(&self) -> &str {
        &self.inner.module
    }

    #[getter]
    fn base(&self) -> &str {
        &self.inner.base
    }

    #[getter]
    fn pg_type(&self) -> &str {
        &self.inner.pg_type
    }

    #[getter]
    fn check_constraints(&self) -> Vec<String> {
        self.inner.check_constraints.clone()
    }
}

#[pyclass(module = "pylon._core", frozen)]
pub struct EnumDescriptor {
    inner: core::schema::EnumDescriptor,
}

#[pymethods]
impl EnumDescriptor {
    #[new]
    fn new(name: String, module: String, members: Vec<String>) -> Self {
        Self {
            inner: core::schema::EnumDescriptor { name, module, members },
        }
    }

    #[getter]
    fn name(&self) -> &str {
        &self.inner.name
    }

    #[getter]
    fn module(&self) -> &str {
        &self.inner.module
    }

    #[getter]
    fn members(&self) -> Vec<String> {
        self.inner.members.clone()
    }
}

#[pyclass(module = "pylon._core", frozen)]
pub struct NamedTupleDescriptor {
    inner: core::schema::NamedTupleDescriptor,
}

#[pymethods]
impl NamedTupleDescriptor {
    #[new]
    #[pyo3(signature = (name, module, *, members = None))]
    fn new(name: String, module: String, members: Option<Vec<PyRef<TupleMember>>>) -> Self {
        Self {
            inner: core::schema::NamedTupleDescriptor {
                name,
                module,
                members: members.unwrap_or_default().iter().map(|m| m.inner.clone()).collect(),
            },
        }
    }

    #[getter]
    fn name(&self) -> &str {
        &self.inner.name
    }

    #[getter]
    fn module(&self) -> &str {
        &self.inner.module
    }
}

#[pyclass(module = "pylon._core", frozen)]
pub struct GlobalDescriptor {
    inner: core::schema::GlobalDescriptor,
}

#[pymethods]
impl GlobalDescriptor {
    #[new]
    #[pyo3(signature = (name, module, scalar_type, required, *, default_expr = None, computed_expr = None))]
    fn new(
        name: String,
        module: String,
        scalar_type: String,
        required: bool,
        default_expr: Option<String>,
        computed_expr: Option<String>,
    ) -> Self {
        Self {
            inner: core::schema::GlobalDescriptor {
                name,
                module,
                scalar_type,
                required,
                default_expr,
                computed_expr,
            },
        }
    }

    #[getter]
    fn name(&self) -> &str {
        &self.inner.name
    }

    #[getter]
    fn module(&self) -> &str {
        &self.inner.module
    }

    #[getter]
    fn scalar_type(&self) -> &str {
        &self.inner.scalar_type
    }

    #[getter]
    fn required(&self) -> bool {
        self.inner.required
    }

    #[getter]
    fn default_expr(&self) -> Option<&str> {
        self.inner.default_expr.as_deref()
    }

    #[getter]
    fn computed_expr(&self) -> Option<&str> {
        self.inner.computed_expr.as_deref()
    }
}

// ── Alias descriptor ───────────────────────────────────────────────────────────

#[pyclass(module = "pylon._core", frozen)]
pub struct AliasDescriptor {
    inner: core::schema::AliasDescriptor,
}

#[pymethods]
impl AliasDescriptor {
    #[new]
    fn new(name: String, module: String, expr: String) -> Self {
        Self { inner: core::schema::AliasDescriptor { name, module, expr } }
    }

    #[getter]
    fn name(&self) -> &str { &self.inner.name }

    #[getter]
    fn module(&self) -> &str { &self.inner.module }

    #[getter]
    fn expr(&self) -> &str { &self.inner.expr }
}

// ── Channel descriptor ──────────────────────────────────────────────────────────

#[pyclass(module = "pylon._core", frozen)]
pub struct ChannelDescriptor {
    inner: core::schema::ChannelDescriptor,
}

#[pymethods]
impl ChannelDescriptor {
    /// `payload_kind` is one of `"type"` / `"scalar"` / `"object"`, with
    /// exactly the matching one of `payload_type_ref` / `payload_scalar_pg_type`
    /// / `payload_object_fields` supplied — mirrors `core::schema::ChannelPayload`'s
    /// three variants without exposing that enum to Python directly.
    #[new]
    #[pyo3(signature = (name, module, wire_name, payload_kind, *, payload_type_ref = None, payload_scalar_pg_type = None, payload_object_fields = None, description = None))]
    fn new(
        name: String,
        module: String,
        wire_name: String,
        payload_kind: &str,
        payload_type_ref: Option<String>,
        payload_scalar_pg_type: Option<String>,
        payload_object_fields: Option<Vec<(String, String)>>,
        description: Option<String>,
    ) -> PyResult<Self> {
        let payload = match payload_kind {
            "type" => core::schema::ChannelPayload::Type(
                payload_type_ref
                    .ok_or_else(|| pyo3::exceptions::PyValueError::new_err("payload_type_ref is required for payload_kind='type'"))?,
            ),
            "scalar" => core::schema::ChannelPayload::Scalar(
                payload_scalar_pg_type
                    .ok_or_else(|| pyo3::exceptions::PyValueError::new_err("payload_scalar_pg_type is required for payload_kind='scalar'"))?,
            ),
            "object" => core::schema::ChannelPayload::Object(
                payload_object_fields
                    .ok_or_else(|| pyo3::exceptions::PyValueError::new_err("payload_object_fields is required for payload_kind='object'"))?,
            ),
            other => {
                return Err(pyo3::exceptions::PyValueError::new_err(format!(
                    "unknown channel payload_kind {other:?}, expected 'type', 'scalar', or 'object'"
                )))
            }
        };
        Ok(Self { inner: core::schema::ChannelDescriptor { name, module, wire_name, payload, description } })
    }

    #[getter]
    fn name(&self) -> &str { &self.inner.name }

    #[getter]
    fn module(&self) -> &str { &self.inner.module }

    #[getter]
    fn wire_name(&self) -> &str { &self.inner.wire_name }

    #[getter]
    fn description(&self) -> Option<&str> { self.inner.description.as_deref() }

    /// One of `"type"` / `"scalar"` / `"object"` — which of the three
    /// getters below is populated. Lets Python-side code (e.g.
    /// `Client.listen()`'s payload decoder) branch on the payload kind
    /// without needing `ChannelPayload` exposed as its own pyclass.
    #[getter]
    fn payload_kind(&self) -> &str {
        match &self.inner.payload {
            core::schema::ChannelPayload::Type(_) => "type",
            core::schema::ChannelPayload::Scalar(_) => "scalar",
            core::schema::ChannelPayload::Object(_) => "object",
        }
    }

    #[getter]
    fn payload_type_ref(&self) -> Option<&str> {
        match &self.inner.payload {
            core::schema::ChannelPayload::Type(qname) => Some(qname),
            _ => None,
        }
    }

    #[getter]
    fn payload_scalar_pg_type(&self) -> Option<&str> {
        match &self.inner.payload {
            core::schema::ChannelPayload::Scalar(pg_type) => Some(pg_type),
            _ => None,
        }
    }

    #[getter]
    fn payload_object_fields(&self) -> Option<Vec<(String, String)>> {
        match &self.inner.payload {
            core::schema::ChannelPayload::Object(fields) => Some(fields.clone()),
            _ => None,
        }
    }
}

// ── Function descriptors ────────────────────────────────────────────────────────

#[pyclass(module = "pylon._core", frozen)]
pub struct FunctionParamDescriptor {
    inner: core::schema::FunctionParamDescriptor,
}

#[pymethods]
impl FunctionParamDescriptor {
    #[new]
    fn new(name: String, pg_type: String) -> Self {
        Self { inner: core::schema::FunctionParamDescriptor { name, pg_type } }
    }

    #[getter]
    fn name(&self) -> &str { &self.inner.name }

    #[getter]
    fn pg_type(&self) -> &str { &self.inner.pg_type }
}

#[pyclass(module = "pylon._core", frozen)]
pub struct FunctionDescriptor {
    inner: core::schema::FunctionDescriptor,
}

#[pymethods]
impl FunctionDescriptor {
    #[new]
    #[pyo3(signature = (
        name,
        module,
        params,
        return_pg_type,
        body,
        *,
        return_is_object = false,
        return_is_set = false,
        return_is_polymorphic = false,
        volatility = "volatile"
    ))]
    fn new(
        name: String,
        module: String,
        params: Vec<PyRef<FunctionParamDescriptor>>,
        return_pg_type: String,
        body: String,
        return_is_object: bool,
        return_is_set: bool,
        return_is_polymorphic: bool,
        volatility: &str,
    ) -> Self {
        Self {
            inner: core::schema::FunctionDescriptor {
                name,
                module,
                params: params.iter().map(|p| p.inner.clone()).collect(),
                return_pg_type,
                return_is_object,
                return_is_set,
                return_is_polymorphic,
                volatility: volatility.to_string(),
                body,
            },
        }
    }

    #[getter]
    fn name(&self) -> &str { &self.inner.name }

    #[getter]
    fn module(&self) -> &str { &self.inner.module }

    #[getter]
    fn return_pg_type(&self) -> &str { &self.inner.return_pg_type }

    #[getter]
    fn return_is_object(&self) -> bool { self.inner.return_is_object }

    #[getter]
    fn return_is_set(&self) -> bool { self.inner.return_is_set }

    #[getter]
    fn volatility(&self) -> &str { &self.inner.volatility }

    #[getter]
    fn body(&self) -> &str { &self.inner.body }
}

/// Opaque Rust value built by the Python schema registry. Treat as immutable;
/// recreate after schema changes.
#[pyclass(module = "pylon._core", frozen)]
pub struct SchemaDescriptor {
    pub(crate) inner: core::schema::SchemaDescriptor,
}

#[pymethods]
impl SchemaDescriptor {
    #[new]
    #[pyo3(signature = (*, types = None, scalars = None, enums = None, named_tuples = None, globals = None, functions = None, aliases = None, channels = None))]
    fn new(
        types: Option<Vec<PyRef<TypeDescriptor>>>,
        scalars: Option<Vec<PyRef<ScalarDescriptor>>>,
        enums: Option<Vec<PyRef<EnumDescriptor>>>,
        named_tuples: Option<Vec<PyRef<NamedTupleDescriptor>>>,
        globals: Option<Vec<PyRef<GlobalDescriptor>>>,
        functions: Option<Vec<PyRef<FunctionDescriptor>>>,
        aliases: Option<Vec<PyRef<AliasDescriptor>>>,
        channels: Option<Vec<PyRef<ChannelDescriptor>>>,
    ) -> Self {
        Self {
            inner: core::schema::SchemaDescriptor {
                types: types
                    .unwrap_or_default()
                    .iter()
                    .map(|t| t.inner.clone())
                    .collect(),
                scalars: scalars
                    .unwrap_or_default()
                    .iter()
                    .map(|s| s.inner.clone())
                    .collect(),
                enums: enums
                    .unwrap_or_default()
                    .iter()
                    .map(|e| e.inner.clone())
                    .collect(),
                named_tuples: named_tuples
                    .unwrap_or_default()
                    .iter()
                    .map(|n| n.inner.clone())
                    .collect(),
                globals: globals
                    .unwrap_or_default()
                    .iter()
                    .map(|g| g.inner.clone())
                    .collect(),
                functions: functions
                    .unwrap_or_default()
                    .iter()
                    .map(|f| f.inner.clone())
                    .collect(),
                aliases: aliases
                    .unwrap_or_default()
                    .iter()
                    .map(|a| a.inner.clone())
                    .collect(),
                channels: channels
                    .unwrap_or_default()
                    .iter()
                    .map(|c| c.inner.clone())
                    .collect(),
            },
        }
    }

    #[getter]
    fn type_count(&self) -> usize {
        self.inner.types.len()
    }

    #[getter]
    fn types(&self) -> Vec<TypeDescriptor> {
        self.inner.types.iter().map(|t| TypeDescriptor { inner: t.clone() }).collect()
    }

    /// Every declared `Channel` — the runtime source of truth for
    /// `Client.listen()` to resolve a channel name to its wire name and
    /// payload shape (this is read from the *migrated* schema singleton,
    /// not the transient Python-side `Channel`/`ChannelDescriptor` objects
    /// `pylon.schema._channels.collect_module_channels` builds at
    /// `finalize()` time and discards — see that module's own docs).
    #[getter]
    fn channels(&self) -> Vec<ChannelDescriptor> {
        self.inner.channels.iter().map(|c| ChannelDescriptor { inner: c.clone() }).collect()
    }

    #[getter]
    fn scalar_count(&self) -> usize {
        self.inner.scalars.len()
    }

    #[getter]
    fn enum_count(&self) -> usize {
        self.inner.enums.len()
    }

    #[getter]
    fn named_tuple_count(&self) -> usize {
        self.inner.named_tuples.len()
    }

    #[getter]
    fn global_count(&self) -> usize {
        self.inner.globals.len()
    }

    /// Return global descriptors as a list of dicts for the REPL and client.
    /// Each dict has keys: name, module, qualified_name, scalar_type, required, computed.
    fn globals<'py>(&self, py: Python<'py>) -> PyResult<pyo3::Bound<'py, pyo3::types::PyList>> {
        use pyo3::types::{PyDict, PyList};
        let items: Vec<_> = self.inner.globals.iter().map(|g| -> PyResult<_> {
            let d = PyDict::new(py);
            d.set_item("name", &g.name)?;
            d.set_item("module", &g.module)?;
            d.set_item("qualified_name", format!("{}::{}", g.module, g.name))?;
            d.set_item("scalar_type", &g.scalar_type)?;
            d.set_item("required", g.required)?;
            d.set_item("computed", g.computed_expr.is_some())?;
            Ok(d)
        }).collect::<PyResult<_>>()?;
        Ok(PyList::new(py, items)?)
    }

    /// Serialize the full schema to JSON — the same format written to
    /// `_pylon."Schema"` by `migration apply`/`watch` (see
    /// `migration_write_schema_snapshot`) and read back by `from_json`.
    fn to_json(&self) -> PyResult<String> {
        serde_json::to_string(&self.inner)
            .map_err(|e| pyo3::exceptions::PyValueError::new_err(e.to_string()))
    }

    /// Deserializes a `SchemaDescriptor` from `to_json`'s own output —
    /// specifically the JSON text `migration_read_schema_snapshot` fetches
    /// from `_pylon."Schema"`. This is how a real client installs the
    /// *migrated* schema as the query-compilation singleton instead of the
    /// one `pylon.finalize()` built fresh from the current `.py` files —
    /// see `pylon.finalize()`'s own doc comment for why those two can
    /// legitimately differ until a migration is applied.
    #[staticmethod]
    fn from_json(json: &str) -> PyResult<Self> {
        serde_json::from_str(json)
            .map(|inner| Self { inner })
            .map_err(|e| pyo3::exceptions::PyValueError::new_err(e.to_string()))
    }
}

// ── Query types ────────────────────────────────────────────────────────────────

#[pyclass(module = "pylon._core", frozen)]
pub struct CompiledQuery {
    pub(crate) inner: core::query::CompiledQuery,
}

#[pymethods]
impl CompiledQuery {
    /// Debug-only escape hatch (the REPL, tests). The normal query path
    /// never reads this: `PgconPool`/`PgconTransaction`'s `*_compiled`
    /// methods (`pgcon.rs`) take a `&CompiledQuery` directly and read
    /// `.sql` out of the Rust-owned struct themselves, so the SQL text
    /// never needs to cross into Python as a string at all.
    #[getter]
    fn sql(&self) -> &str {
        &self.inner.sql
    }

    /// Ordered parameter names matching $1, $2, … in the SQL.
    /// Use this to map kwargs to positional arguments for asyncpg.
    #[getter]
    fn param_names<'py>(&self, py: Python<'py>) -> Bound<'py, pyo3::types::PyList> {
        pyo3::types::PyList::new(py, self.inner.param_names.iter().map(|s| s.as_str()))
            .expect("infallible: strings are always valid Python objects")
    }

    /// Shape descriptor as a nested Python dict.
    /// Walk this alongside each ``result`` column from asyncpg to decode records.
    #[getter]
    fn shape<'py>(&self, py: Python<'py>) -> PyResult<pyo3::Bound<'py, pyo3::types::PyAny>> {
        shape_node_to_py(py, &self.inner.shape.root)
    }

    fn warnings<'py>(&self, py: Python<'py>) -> Bound<'py, pyo3::types::PyList> {
        pyo3::types::PyList::new(py, &self.inner.warnings).unwrap()
    }

    /// Schema-qualified tables (`"schema.table"`) this statement reads from or
    /// writes to. For a SELECT, the tags to cache the result under; for an
    /// INSERT/UPDATE/DELETE, the tags a cache layer must invalidate on commit.
    #[getter]
    fn tags<'py>(&self, py: Python<'py>) -> Bound<'py, pyo3::types::PyList> {
        pyo3::types::PyList::new(py, &self.inner.tags).unwrap()
    }

    /// Returns an `InferencePlan` dict if this query requires a pre-execution model call,
    /// or `None` for pure-SQL queries. The dict always has a `"kind"` key: `"search"` or `"embedding"`.
    #[getter]
    fn inference_plan<'py>(&self, py: Python<'py>) -> PyResult<pyo3::Bound<'py, pyo3::types::PyAny>> {
        use pyo3::types::PyDict;
        use core::query::InferencePlan;
        match &self.inner.inference_plan {
            None => Ok(py.None().into_bound(py)),
            Some(InferencePlan::Search { backend, index_name, query_param_name, query_literal, size }) => {
                let d = PyDict::new(py);
                d.set_item("kind", "search")?;
                d.set_item("backend", backend)?;
                d.set_item("index_name", index_name)?;
                d.set_item("query_param_name", query_param_name)?;
                d.set_item("query_literal", query_literal.as_deref())?;
                d.set_item("size", size)?;
                Ok(d.into_any())
            }
            Some(InferencePlan::Embedding { model_name, type_name, index_name, query_param_name, query_literal }) => {
                let d = PyDict::new(py);
                d.set_item("kind", "embedding")?;
                d.set_item("model_name", model_name)?;
                d.set_item("type_name", type_name)?;
                d.set_item("index_name", index_name.as_deref())?;
                d.set_item("query_param_name", query_param_name)?;
                d.set_item("query_literal", query_literal.as_deref())?;
                Ok(d.into_any())
            }
        }
    }
}

// ── Public functions ───────────────────────────────────────────────────────────

#[pyfunction]
#[pyo3(signature = (query, schema, *, allow_user_specified_id = false))]
fn compile(query: &str, schema: &SchemaDescriptor, allow_user_specified_id: bool) -> PyResult<CompiledQuery> {
    let config = core::ir::SessionConfig { allow_user_specified_id };
    core::query::compile_with_config(query, &schema.inner, &config)
        .map(|q| CompiledQuery { inner: q })
        .map_err(|e| pyql_err(e, Some(query)))
}

/// Records a compile-stage outcome in `pylon_queries_total{stage="compile"}`
/// — called from `pylon.client._compile_and_resolve`/`_compile_and_bind`
/// right after their own `pylon.query.compile()` call, since those (not
/// this bare function, which the LSP and other tooling also call for
/// non-serving purposes) are the actual query-serving compile step.
#[pyfunction]
fn record_query_compile_result(success: bool) {
    pylon_workers::metrics::record_compile_result(success);
}

#[pyfunction]
fn export_schema(schema: &SchemaDescriptor) -> PyResult<String> {
    core::export::export_schema(&schema.inner).map_err(|e| pyql_err(e, None))
}

/// Compiles every user function body, computed-pointer expression, and
/// property/link default and checks each one's declared type against what
/// its body actually produces — see `pylon_core::validate` for the details
/// and its best-effort scope. Every mismatch found is collected and reported
/// together, joined by newline, as a single `SchemaError`.
#[pyfunction]
fn validate_schema_types(schema: &SchemaDescriptor) -> PyResult<()> {
    core::validate::validate_schema_types(&schema.inner).map_err(|errs| {
        let message = errs
            .iter()
            .map(|e| e.class_name_message_position().1.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        pyql_err(
            core::error::PyQLError::Fragment(core::error::PyQLFragmentError {
                message,
                position: core::error::Position { line: 0, col: 0 },
                context: "schema".to_string(),
            }),
            None,
        )
    })
}

#[pyfunction]
fn export_stdlib() -> String {
    core::stdlib::export_stdlib()
}

// ── Migration ─────────────────────────────────────────────────────────────────

/// Parsed migration file exposed to Python.
#[pyclass]
struct MigrationFile {
    pub(crate) inner: core::migration::MigrationFile,
}

#[pymethods]
impl MigrationFile {
    #[getter] fn id(&self) -> &str { &self.inner.id }
    #[getter] fn onto(&self) -> &str { &self.inner.onto }
    #[getter] fn filename(&self) -> &str { &self.inner.filename }
    #[getter] fn body(&self) -> &str { &self.inner.body }
    #[getter] fn short_id(&self) -> &str { self.inner.short_id() }
    #[getter] fn is_first(&self) -> bool { self.inner.is_first() }
    #[getter] fn squashed(&self) -> Vec<String> { self.inner.squashed.clone() }
}

/// Parse a migration file's content. `filename` is for error messages only.
#[pyfunction]
fn parse_migration(content: &str, filename: &str) -> PyResult<MigrationFile> {
    core::migration::parse(content, filename)
        .map(|inner| MigrationFile { inner })
        .map_err(|e| pyo3::exceptions::PyValueError::new_err(e.to_string()))
}


/// Validate a list of migration files form a single unbroken chain.
/// Returns them in chain order (oldest first).
#[pyfunction]
fn validate_migration_chain(migrations: Vec<PyRef<MigrationFile>>) -> PyResult<Vec<String>> {
    let owned: Vec<core::migration::MigrationFile> = migrations.iter().map(|m| m.inner.clone()).collect();
    core::migration::validate_chain(&owned)
        .map(|chain| chain.iter().map(|m| m.id.clone()).collect())
        .map_err(|e| pyo3::exceptions::PyValueError::new_err(e.to_string()))
}

/// Compute a migration's full ID from its body.
#[pyfunction]
fn compute_migration_id(body: &str) -> String {
    core::migration::compute_id(body)
}

/// Compute a migration's short ID (filename component) from its body.
#[pyfunction]
fn compute_migration_short_id(body: &str) -> String {
    core::migration::compute_short_id(body)
}

/// Render a complete migration file string (header + body).
#[pyfunction]
#[pyo3(signature = (onto, body, squashed = None))]
fn render_migration_file(onto: &str, body: &str, squashed: Option<Vec<String>>) -> String {
    core::migration::render_file(onto, body, squashed.as_deref().unwrap_or(&[]))
}

/// Return the stub body for a blank migration.
#[pyfunction]
fn blank_migration_body() -> &'static str {
    core::migration::blank_body()
}

// ── Diff / DbState ────────────────────────────────────────────────────────────

/// Mutable snapshot of live PostgreSQL structure, built from pg_catalog queries.
/// Pass to `diff_schema()` to compute the DDL needed to reach the target schema.
#[pyclass(module = "pylon._core")]
pub struct DbState {
    pub(crate) inner: core::diff::DbState,
}


/// Compute ordered DDL SQL statements to bring `current` in sync with `target`.
/// Returns a list of SQL strings; empty when nothing needs to change.
/// All index creation uses plain (non-CONCURRENTLY) form — suitable for watch mode.
#[pyfunction]
fn diff_schema(target: &SchemaDescriptor, current: &DbState) -> PyResult<Vec<String>> {
    core::diff::diff_schema(&target.inner, &current.inner)
        .map_err(|e| pyo3::exceptions::PyValueError::new_err(e))
}

/// Compute ordered DDL ops with non-transactional markers for migration file creation.
/// Returns a list of `(sql, non_transactional)` tuples. Indexes on pre-existing tables
/// use `CREATE INDEX CONCURRENTLY` and are marked `non_transactional=True`.
#[pyfunction]
fn diff_schema_ops(target: &SchemaDescriptor, current: &DbState) -> PyResult<Vec<(String, bool)>> {
    core::diff::diff_schema_ops(&target.inner, &current.inner)
        .map(|ops| ops.into_iter().map(|op| (op.sql, op.non_transactional)).collect())
        .map_err(|e| pyo3::exceptions::PyValueError::new_err(e))
}

/// Compute the net DDL to go from `before` to `after` (two live-DB snapshots).
/// Used by the squash command: apply migrations to a shadow DB, introspect before
/// and after, then call this to produce a single equivalent migration body.
/// Returns a list of `(sql, non_transactional)` tuples.
#[pyfunction]
fn diff_states(before: &DbState, after: &DbState) -> Vec<(String, bool)> {
    core::diff::diff_states(&before.inner, &after.inner)
        .into_iter()
        .map(|op| (op.sql, op.non_transactional))
        .collect()
}

/// Detect potential type (table) renames.
/// Returns list of (old_module, old_table, new_module, new_table, new_type_name, confidence).
#[pyfunction]
fn detect_type_renames(
    target: &SchemaDescriptor,
    current: &DbState,
    guidance: &Guidance,
) -> Vec<(String, String, String, String, String, f64)> {
    core::diff::detect_type_renames(&target.inner, &current.inner, &guidance.inner)
        .into_iter()
        .map(|c| (c.old_module, c.old_table, c.new_module, c.new_table, c.new_type_name, c.confidence))
        .collect()
}

/// Detect potential column renames within existing tables.
/// Returns list of (module, table, old_col, new_col, pg_type).
#[pyfunction]
fn detect_col_renames(
    target: &SchemaDescriptor,
    current: &DbState,
    guidance: &Guidance,
) -> Vec<(String, String, String, String, String)> {
    core::diff::detect_col_renames(&target.inner, &current.inner, &guidance.inner)
        .into_iter()
        .map(|c| (c.module, c.table, c.old_col, c.new_col, c.pg_type))
        .collect()
}

/// Accumulates rejected rename candidates so a re-diff (after the user says
/// "no" to a proposed rename) doesn't propose the same pair again — the CLI
/// creates one of these per `migration create` invocation and calls
/// `ban_type_rename`/`ban_col_rename` on rejection.
#[pyclass(module = "pylon._core")]
pub struct Guidance {
    pub(crate) inner: core::diff::Guidance,
}

#[pymethods]
impl Guidance {
    #[new]
    fn new() -> Self {
        Self { inner: core::diff::Guidance::default() }
    }

    fn ban_type_rename(&mut self, old_module: String, old_table: String, new_module: String, new_table: String) {
        self.inner.banned_type_renames.insert((old_module, old_table, new_module, new_table));
    }

    fn ban_col_rename(&mut self, module: String, table: String, old_col: String, new_col: String) {
        self.inner.banned_col_renames.insert((module, table, old_col, new_col));
    }
}

/// One logical schema-level question ("did you create scalar type 'X'?"),
/// bundling every DDL statement that answers it — see
/// `pylon_core::diff::MigrationStep`.
#[pyclass(module = "pylon._core", frozen)]
pub struct MigrationStep {
    pub(crate) inner: core::diff::MigrationStep,
}

#[pymethods]
impl MigrationStep {
    #[getter]
    fn prompt(&self) -> &str {
        &self.inner.prompt
    }

    #[getter]
    fn verb(&self) -> &str {
        self.inner.verb.as_str()
    }

    #[getter]
    fn object_desc(&self) -> &str {
        &self.inner.object_desc
    }

    /// List of (sql, non_transactional) tuples, in emission order. May
    /// contain `\(placeholder)` tokens — see `required_input` — that must be
    /// resolved via `resolved_ddl` before executing.
    #[getter]
    fn ddl(&self) -> Vec<(String, bool)> {
        self.inner.ddl.iter().map(|op| (op.sql.clone(), op.non_transactional)).collect()
    }

    /// List of (placeholder, prompt, default_expr, type_name) tuples — one
    /// per free-form expression the caller must resolve (e.g. a property's
    /// type-change conversion expression) before this step's `ddl` is
    /// directly usable. Empty for the common case.
    #[getter]
    fn required_input(&self) -> Vec<(String, String, String, String)> {
        self.inner.required_input.iter()
            .map(|r| (r.placeholder.clone(), r.prompt.clone(), r.default_expr.clone(), r.type_name.clone()))
            .collect()
    }

    /// One of "module", "scalar", "table", "function", "view" — the kind of
    /// object this step's identity refers to. Lets a caller correlate steps
    /// (e.g. an interface's "view" step with its implementors' "table"
    /// steps) without parsing `prompt`/`object_desc` display text.
    #[getter]
    fn kind(&self) -> &'static str {
        match &self.inner.op_key {
            core::diff::OpKey::Module(_) => "module",
            core::diff::OpKey::Scalar(_, _) => "scalar",
            core::diff::OpKey::Table(_, _) => "table",
            core::diff::OpKey::Function(_, _) => "function",
            core::diff::OpKey::View(_, _) => "view",
        }
    }

    /// This step's object as a qualified `module::name` string. For a
    /// "table" step this is the *type's* name, not its physical table
    /// (which may differ via `table=`) — resolved by looking the type up
    /// in `schema`. `None` only if no matching descriptor is found
    /// (shouldn't happen in practice).
    fn qualified_name(&self, schema: &SchemaDescriptor) -> Option<String> {
        match &self.inner.op_key {
            core::diff::OpKey::Module(m) => Some(m.clone()),
            core::diff::OpKey::Scalar(m, n) | core::diff::OpKey::Function(m, n) | core::diff::OpKey::View(m, n) => {
                Some(format!("{m}::{n}"))
            }
            core::diff::OpKey::Table(m, table) => schema.inner.types.iter()
                .find(|t| &t.module == m && &t.table == table)
                .map(|t| format!("{}::{}", t.module, t.name)),
        }
    }

    /// For a "table" step, the qualified names of every interface its type
    /// implements (empty for any other step kind, or if no matching type is
    /// found) — lets the CLI ask about an interface before its first
    /// implementor even though the interface's own DDL (a view selecting
    /// from its implementors) must still be assembled afterward.
    fn implements(&self, schema: &SchemaDescriptor) -> Vec<String> {
        let core::diff::OpKey::Table(m, table) = &self.inner.op_key else { return vec![] };
        schema.inner.types.iter()
            .find(|t| &t.module == m && &t.table == table)
            .map(|t| t.interfaces.clone())
            .unwrap_or_default()
    }

    /// This step's DDL with every `\(placeholder)` token substituted —
    /// `overrides[placeholder]` if given, else that input's own
    /// `default_expr`. Returns (sql, non_transactional) tuples, same shape
    /// as `ddl`.
    fn resolved_ddl(&self, overrides: HashMap<String, String>) -> Vec<(String, bool)> {
        self.inner.resolved_ddl(&overrides).into_iter().map(|op| (op.sql, op.non_transactional)).collect()
    }

    /// Best-effort "equivalent Python schema declaration" for this step's
    /// object, shown alongside its DDL in the interactive migration CLI.
    /// `None` for step kinds not covered yet (modules, functions, interface
    /// views) — see `pylon_core::export::python_snippet`'s doc comment.
    fn python_snippet(&self, schema: &SchemaDescriptor) -> Option<String> {
        core::export::python_snippet::python_snippet_for_step(&self.inner, &schema.inner)
    }
}

/// Compute the diff as one `MigrationStep` per logical schema-level question,
/// with confirmed renames and fill expressions already applied — the
/// interactive-loop counterpart of `diff_schema_ops_with_renames_and_fills`.
/// `type_renames`: list of (old_module, old_table, new_module, new_table).
/// `col_renames`:  list of (module, table, old_col, new_col).
/// `fills`:        list of (module, table, column, sql_expr).
#[pyfunction]
fn diff_schema_steps_with_renames_and_fills(
    target: &SchemaDescriptor,
    current: &DbState,
    type_renames: Vec<(String, String, String, String)>,
    col_renames: Vec<(String, String, String, String)>,
    fills: Vec<(String, String, String, String)>,
) -> PyResult<Vec<MigrationStep>> {
    core::diff::diff_schema_steps_with_renames_and_fills(
        &target.inner, &current.inner, &type_renames, &col_renames, &fills,
    )
    .map(|steps| steps.into_iter().map(|inner| MigrationStep { inner }).collect())
    .map_err(|e| pyo3::exceptions::PyValueError::new_err(e))
}

/// Diff with confirmed renames applied.
/// `type_renames`: list of (old_module, old_table, new_module, new_table).
/// `col_renames`:  list of (module, table, old_col, new_col).
/// Returns list of (sql, non_transactional).
#[pyfunction]
fn diff_schema_ops_with_renames(
    target: &SchemaDescriptor,
    current: &DbState,
    type_renames: Vec<(String, String, String, String)>,
    col_renames: Vec<(String, String, String, String)>,
) -> PyResult<Vec<(String, bool)>> {
    core::diff::diff_schema_ops_with_renames(&target.inner, &current.inner, &type_renames, &col_renames)
        .map(|ops| ops.into_iter().map(|op| (op.sql, op.non_transactional)).collect())
        .map_err(|e| pyo3::exceptions::PyValueError::new_err(e))
}

/// Compile a PyQL fill expression to a bare SQL expression for use in an UPDATE SET clause.
/// `type_name` is the qualified type name (e.g. `"blog::Post"`).
/// `expr_str`  is the PyQL expression (e.g. `"'No content'"`, `".title"`, `"0"`).
/// Raises `PyQLError` on syntax, type, or resolution failures.
/// Raises `PyQLSyntaxError` if the expression contains query parameters ($name).
#[pyfunction]
fn compile_fill_expr(
    type_name: &str,
    expr_str: &str,
    schema: &SchemaDescriptor,
) -> PyResult<String> {
    core::query::compile_fill_expr(type_name, expr_str, &schema.inner).map_err(|e| pyql_err(e, Some(expr_str)))
}

/// Detect columns that are being made NOT NULL and will need a fill expression.
/// Returns list of (module, table, column, pg_type, type_name, is_new_column, default_sql).
/// `default_sql` is `None` when no schema-level default is declared.
#[pyfunction]
fn detect_fill_required(
    target: &SchemaDescriptor,
    current: &DbState,
) -> Vec<(String, String, String, String, String, bool, Option<String>)> {
    core::diff::detect_fill_required(&target.inner, &current.inner)
        .into_iter()
        .map(|f| (f.module, f.table, f.column, f.pg_type, f.type_name, f.is_new_column, f.default_sql))
        .collect()
}

/// Diff with confirmed renames and fill expressions applied.
/// `type_renames`: list of (old_module, old_table, new_module, new_table).
/// `col_renames`:  list of (module, table, old_col, new_col).
/// `fills`:        list of (module, table, column, sql_expr).
/// Returns list of (sql, non_transactional).
#[pyfunction]
fn diff_schema_ops_with_renames_and_fills(
    target: &SchemaDescriptor,
    current: &DbState,
    type_renames: Vec<(String, String, String, String)>,
    col_renames: Vec<(String, String, String, String)>,
    fills: Vec<(String, String, String, String)>,
) -> PyResult<Vec<(String, bool)>> {
    core::diff::diff_schema_ops_with_renames_and_fills(
        &target.inner, &current.inner, &type_renames, &col_renames, &fills,
    )
    .map(|ops| ops.into_iter().map(|op| (op.sql, op.non_transactional)).collect())
    .map_err(|e| pyo3::exceptions::PyValueError::new_err(e))
}

/// Serialize a compiled `SchemaDescriptor` to the `DbState` JSON snapshot format.
/// The result is suitable for storage in `_pylon."Migrations".db_state` and can be
/// read back as a diff baseline via `db_state_from_json`.
#[pyfunction]
fn schema_to_db_state_json(schema: &SchemaDescriptor) -> String {
    let state = core::diff::schema_to_db_state(&schema.inner);
    core::diff::db_state_to_json(&state)
}

/// `CREATE EXTENSION IF NOT EXISTS` statements for every Postgres extension
/// `target` requires (currently just `vector`, needed the moment any type
/// declares a vector index) that isn't already present in `current` — meant
/// to be prepended to an assembled migration/`watch` sync unconditionally,
/// not offered as a per-step confirmation (enabling a required extension
/// isn't a design decision, it's a hard prerequisite).
#[pyfunction]
fn missing_extension_ddl(target: &SchemaDescriptor, current: &DbState) -> Vec<String> {
    core::diff::missing_extension_ddl(&target.inner, &current.inner)
}

/// True when `target` differs from `previous` in any way — including
/// schema semantics with zero physical DDL footprint (`readonly`,
/// rewrites, computed globals, pub/sub `Channel`s, ...) that
/// `diff_schema_steps`/`diff_schema_ops`/`missing_extension_ddl` can never
/// see, since there's no column/constraint/catalog object for those to
/// introspect. `migration create`/`watch` must treat this as "there is a
/// change" even when the DDL-step list above comes back empty — otherwise
/// that class of change can never be migrated at all, not just "doesn't
/// require" one. `previous=None` (no migration ever applied) compares
/// against an empty schema. See `core::diff::schema_content_changed`'s own
/// doc comment for the full rationale.
#[pyfunction]
fn schema_content_changed(target: &SchemaDescriptor, previous: Option<&SchemaDescriptor>) -> bool {
    core::diff::schema_content_changed(&target.inner, previous.map(|p| &p.inner))
}

/// Deserialize a `DbState` from the JSON snapshot stored in `_pylon."Migrations".db_state`.
/// Returns a `DbState` object usable as a diff baseline for `diff_schema_ops` etc.
#[pyfunction]
fn db_state_from_json(json: &str) -> PyResult<DbState> {
    core::diff::db_state_from_json(json)
        .map(|inner| DbState { inner })
        .map_err(|e| pyo3::exceptions::PyValueError::new_err(e))
}

/// Discard all cached compiled queries. Must be called after a schema reload
/// so stale compiled SQL is not reused against the new schema.
#[pyfunction]
fn clear_query_cache() {
    core::query::clear_query_cache();
}

// ── Shape conversion ───────────────────────────────────────────────────────────

/// Recursive per-member decode plan for a jsonb-backed tuple value
/// (`ShapeNode::NamedTuple.members`) — see `core::query::JsonMember`.
fn json_member_to_py<'py>(
    py: Python<'py>,
    member: &core::query::JsonMember,
) -> PyResult<pyo3::Bound<'py, pyo3::types::PyAny>> {
    use pyo3::types::{PyDict, PyList};
    use core::query::JsonMemberKind;

    let d = PyDict::new(py);
    d.set_item("key", member.key.as_deref())?;
    match &member.kind {
        JsonMemberKind::Scalar => {
            d.set_item("kind", "scalar")?;
        }
        JsonMemberKind::Enum { enum_type } => {
            d.set_item("kind", "enum")?;
            d.set_item("enum_type", enum_type.as_str())?;
        }
        JsonMemberKind::Tuple { type_name, members } => {
            d.set_item("kind", "tuple")?;
            d.set_item("type_name", type_name.as_deref())?;
            let py_members = PyList::new(
                py,
                members.iter().map(|m| json_member_to_py(py, m)).collect::<PyResult<Vec<_>>>()?,
            )?;
            d.set_item("members", py_members)?;
        }
    }
    Ok(d.into_any())
}

fn shape_node_to_py<'py>(
    py: Python<'py>,
    node: &core::query::ShapeNode,
) -> PyResult<pyo3::Bound<'py, pyo3::types::PyAny>> {
    use pyo3::types::{PyDict, PyList};
    use core::query::{Cardinality, ShapeNode};

    let d = PyDict::new(py);
    match node {
        ShapeNode::Scalar { name, position } => {
            d.set_item("kind", "scalar")?;
            d.set_item("name", name.as_str())?;
            d.set_item("position", position)?;
        }
        ShapeNode::Object { name, type_name, position, cardinality, pointers } => {
            d.set_item("kind", "object")?;
            d.set_item("name", name.as_str())?;
            d.set_item("type_name", type_name.as_deref())?;
            d.set_item("position", position)?;
            d.set_item("cardinality", match cardinality {
                Cardinality::Required => "required",
                Cardinality::Optional => "optional",
                Cardinality::Many    => "many",
            })?;
            let py_pointers = PyList::new(
                py,
                pointers.iter().map(|f| shape_node_to_py(py, f)).collect::<PyResult<Vec<_>>>()?,
            )?;
            d.set_item("pointers", py_pointers)?;
        }
        ShapeNode::Array { name, position, element } => {
            d.set_item("kind", "array")?;
            d.set_item("name", name.as_str())?;
            d.set_item("position", position)?;
            d.set_item("element", shape_node_to_py(py, element)?)?;
        }
        ShapeNode::Tuple { position, elements } => {
            d.set_item("kind", "tuple")?;
            d.set_item("position", position)?;
            let py_elems = PyList::new(
                py,
                elements.iter().map(|e| shape_node_to_py(py, e)).collect::<PyResult<Vec<_>>>()?,
            )?;
            d.set_item("elements", py_elems)?;
        }
        ShapeNode::RawScalar => {
            d.set_item("kind", "raw_scalar")?;
        }
        ShapeNode::JsonScalar => {
            d.set_item("kind", "json_scalar")?;
        }
        ShapeNode::NamedTuple { name, position, type_name, members, is_free_object } => {
            d.set_item("kind", "named_tuple")?;
            d.set_item("name", name.as_str())?;
            d.set_item("position", position)?;
            d.set_item("type_name", type_name.as_deref())?;
            d.set_item("is_free_object", *is_free_object)?;
            match members {
                Some(ms) => {
                    let py_members = PyList::new(
                        py,
                        ms.iter().map(|m| json_member_to_py(py, m)).collect::<PyResult<Vec<_>>>()?,
                    )?;
                    d.set_item("members", py_members)?;
                }
                None => d.set_item("members", py.None())?,
            }
        }
        ShapeNode::Enum { name, position, enum_type } => {
            d.set_item("kind", "enum")?;
            d.set_item("name", name.as_str())?;
            d.set_item("position", position)?;
            d.set_item("enum_type", enum_type.as_str())?;
        }
        ShapeNode::Group { key_nodes, grouping_position, elements_position, element } => {
            d.set_item("kind", "group")?;
            let py_key_nodes = PyList::new(
                py,
                key_nodes.iter().map(|n| shape_node_to_py(py, n)).collect::<PyResult<Vec<_>>>()?,
            )?;
            d.set_item("key_nodes", py_key_nodes)?;
            d.set_item("grouping_position", grouping_position)?;
            d.set_item("elements_position", elements_position)?;
            d.set_item("element", shape_node_to_py(py, element)?)?;
        }
        ShapeNode::VectorSearch { object_position, distance_position, object_node } => {
            d.set_item("kind", "vector_search")?;
            d.set_item("object_position", object_position)?;
            d.set_item("distance_position", distance_position)?;
            d.set_item("object_node", shape_node_to_py(py, object_node)?)?;
        }
        ShapeNode::FtsSearch { object_position, rank_position, object_node } => {
            d.set_item("kind", "fts_search")?;
            d.set_item("object_position", object_position)?;
            d.set_item("rank_position", rank_position)?;
            d.set_item("object_node", shape_node_to_py(py, object_node)?)?;
        }
    }
    Ok(d.into_any())
}

// ── Error conversion ───────────────────────────────────────────────────────────

/// Converts a 1-based `(line, col)` position (as `error::Position` reports
/// it — `col` counts *bytes* within the line, since the lexer operates on
/// raw bytes) into a 0-based *character* offset into `text`, matching the
/// wire-protocol convention `pylon.exceptions.PylonError`'s caret-snippet
/// renderer expects (`_FIELD_CHARACTER_START`). Returns
/// `None` for a dead/unset position (`line == 0` — used by compile-time
/// type/resolution errors, which don't track a real position yet) rather
/// than rendering a nonsensical snippet.
///
/// Clamped to the last valid character index: an EOF error's position is
/// one column *past* the last character (there's nothing there to point
/// at), and `pylon.exceptions._format_error`'s line-walker skips a line
/// entirely once its running offset reaches-or-exceeds that line's length
/// — for a single-line, no-trailing-newline query, an unclamped offset
/// exactly equal to the query's length falls into that "past the end,
/// keep looking" branch with no further line to find, silently dropping
/// the source excerpt from the rendered error.
fn char_offset(text: &str, line: u32, col: u32) -> Option<usize> {
    if line == 0 {
        return None;
    }
    let total_chars = text.chars().count();
    if total_chars == 0 {
        return None;
    }
    let mut offset = 0usize;
    for (i, l) in text.split_inclusive('\n').enumerate() {
        if i as u32 + 1 == line {
            let byte_col = col.saturating_sub(1) as usize;
            let char_col = l.char_indices().take_while(|(b, _)| *b < byte_col).count();
            return Some((offset + char_col).min(total_chars - 1));
        }
        offset += l.chars().count();
    }
    None
}

/// Maps a Rust-side `PyQLError` straight to the real `pylon.exceptions.*`
/// class (not the separate, unrelated `pylon._core.PyQL*Error` hierarchy
/// registered below, which is kept importable for anyone referencing it by
/// name but is no longer what actually gets raised) — mirrors the existing
/// `pgcon_err` pattern in `pgcon.rs` for Postgres errors. Callers get the
/// right exception class *and*, when a real position is available (syntax
/// and type errors always carry one; resolution/cardinality/fragment
/// errors don't track one yet), the annotated-source-snippet rendering
/// `PylonError.__str__` already implements via
/// `_from_transpiler` — previously unreachable because nothing ever called
/// it, and because `pylon.client`'s blanket `except BaseException` used to
/// collapse every compile error into `InternalServerError` regardless.
fn pyql_err(err: core::error::PyQLError, query: Option<&str>) -> PyErr {
    let (class_name, message, position) = err.class_name_message_position();
    construct_pylon_error(class_name, message, query, position)
}

fn construct_pylon_error(class_name: &str, message: &str, query: Option<&str>, position: &core::error::Position) -> PyErr {
    Python::attach(|py| {
        let result: PyResult<PyErr> = (|| {
            let module = py.import("pylon.exceptions")?;
            let cls = module.getattr(class_name)?;
            let kwargs = pyo3::types::PyDict::new(py);
            if let Some(q) = query {
                if let Some(offset) = char_offset(q, position.line, position.col) {
                    kwargs.set_item("query", q)?;
                    kwargs.set_item("position_start", offset)?;
                    kwargs.set_item("position_end", offset + 1)?;
                    kwargs.set_item("line", position.line)?;
                    kwargs.set_item("col", position.col)?;
                }
            }
            let instance = cls.call_method("_from_transpiler", (message,), Some(&kwargs))?;
            Ok(PyErr::from_value(instance))
        })();
        match result {
            Ok(err) => err,
            Err(construct_err) => construct_err,
        }
    })
}

// ── Module ─────────────────────────────────────────────────────────────────────

#[pymodule]
fn _core(m: &Bound<'_, PyModule>) -> PyResult<()> {
    let py = m.py();

    // Exceptions
    m.add("PylonCacheError", PylonCacheError::type_object(py))?;
    m.add("PylonPgconError", PylonPgconError::type_object(py))?;

    // Deletion policy
    m.add_class::<OnDeletePolicy>()?;

    // Pointer descriptors
    m.add_class::<RewriteEntry>()?;
    m.add_class::<SignalEntry>()?;
    m.add_class::<TupleMember>()?;
    m.add_class::<PropertyDescriptor>()?;
    m.add_class::<LinkDescriptor>()?;
    m.add_class::<MultiLinkDescriptor>()?;
    m.add_class::<ComputedDescriptor>()?;

    // Type-level constructs
    m.add_class::<IndexDescriptor>()?;
    m.add_class::<VectorIndexDescriptor>()?;
    m.add_class::<SearchPointerDescriptor>()?;
    m.add_class::<SearchIndexDescriptor>()?;
    m.add_class::<TriggerDescriptor>()?;
    m.add_class::<ExclusiveConstraint>()?;
    m.add_class::<ExpressionConstraint>()?;

    // Top-level descriptors
    m.add_class::<TypeDescriptor>()?;
    m.add_class::<ScalarDescriptor>()?;
    m.add_class::<EnumDescriptor>()?;
    m.add_class::<NamedTupleDescriptor>()?;
    m.add_class::<GlobalDescriptor>()?;
    m.add_class::<AliasDescriptor>()?;
    m.add_class::<ChannelDescriptor>()?;
    m.add_class::<FunctionParamDescriptor>()?;
    m.add_class::<FunctionDescriptor>()?;
    m.add_class::<SchemaDescriptor>()?;

    // Query types
    m.add_class::<CompiledQuery>()?;

    // Functions
    m.add_function(wrap_pyfunction!(compile, m)?)?;
    m.add_function(wrap_pyfunction!(record_query_compile_result, m)?)?;
    m.add_function(wrap_pyfunction!(export_schema, m)?)?;
    m.add_function(wrap_pyfunction!(validate_schema_types, m)?)?;
    m.add_function(wrap_pyfunction!(export_stdlib, m)?)?;

    // Migration
    m.add_class::<MigrationFile>()?;
    m.add_function(wrap_pyfunction!(parse_migration, m)?)?;
    m.add_function(wrap_pyfunction!(validate_migration_chain, m)?)?;
    m.add_function(wrap_pyfunction!(compute_migration_id, m)?)?;
    m.add_function(wrap_pyfunction!(compute_migration_short_id, m)?)?;
    m.add_function(wrap_pyfunction!(render_migration_file, m)?)?;
    m.add_function(wrap_pyfunction!(blank_migration_body, m)?)?;

    // Diff / watch
    m.add_class::<DbState>()?;
    m.add_class::<Guidance>()?;
    m.add_class::<MigrationStep>()?;
    m.add_function(wrap_pyfunction!(diff_schema, m)?)?;
    m.add_function(wrap_pyfunction!(diff_schema_ops, m)?)?;
    m.add_function(wrap_pyfunction!(diff_states, m)?)?;
    m.add_function(wrap_pyfunction!(detect_type_renames, m)?)?;
    m.add_function(wrap_pyfunction!(detect_col_renames, m)?)?;
    m.add_function(wrap_pyfunction!(diff_schema_ops_with_renames, m)?)?;
    m.add_function(wrap_pyfunction!(compile_fill_expr, m)?)?;
    m.add_function(wrap_pyfunction!(detect_fill_required, m)?)?;
    m.add_function(wrap_pyfunction!(diff_schema_ops_with_renames_and_fills, m)?)?;
    m.add_function(wrap_pyfunction!(diff_schema_steps_with_renames_and_fills, m)?)?;
    m.add_function(wrap_pyfunction!(schema_to_db_state_json, m)?)?;
    m.add_function(wrap_pyfunction!(missing_extension_ddl, m)?)?;
    m.add_function(wrap_pyfunction!(db_state_from_json, m)?)?;
    m.add_function(wrap_pyfunction!(schema_content_changed, m)?)?;
    m.add_function(wrap_pyfunction!(clear_query_cache, m)?)?;

    // Cache
    cache::register(m)?;

    // Postgres driver (async) — one persistent multi-threaded tokio
    // runtime for the whole process, built once here rather than per call.
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.enable_all();
    pyo3_async_runtimes::tokio::init(builder);
    pgcon::register(m)?;
    migrate::register(m)?;
    introspect::register(m)?;
    providers::register(m)?;
    workers::register(m)?;

    Ok(())
}
