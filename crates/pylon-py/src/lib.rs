use pyo3::prelude::*;
use pyo3::PyTypeInfo;
use pylon_core as core;

// ── Exception hierarchy ────────────────────────────────────────────────────────

pyo3::create_exception!(
    pylon._core,
    PyQLError,
    pyo3::exceptions::PyException,
    "Base class for all PyQL compilation errors."
);
pyo3::create_exception!(
    pylon._core,
    PyQLSyntaxError,
    PyQLError,
    "Raised on lexer or parser failure in the PyQL string."
);
pyo3::create_exception!(
    pylon._core,
    PyQLTypeError,
    PyQLError,
    "Raised on type mismatch or invalid cast detected during compilation."
);
pyo3::create_exception!(
    pylon._core,
    PyQLResolutionError,
    PyQLError,
    "Base class for unknown-identifier errors."
);
pyo3::create_exception!(
    pylon._core,
    PyQLUnknownTypeError,
    PyQLResolutionError,
    "Referenced type name does not exist in the schema."
);
pyo3::create_exception!(
    pylon._core,
    PyQLUnknownFieldError,
    PyQLResolutionError,
    "Referenced property or link does not exist on the type."
);
pyo3::create_exception!(
    pylon._core,
    PyQLUnknownParameterError,
    PyQLResolutionError,
    "Query parameter ($name) not declared."
);
pyo3::create_exception!(
    pylon._core,
    PyQLCardinalityError,
    PyQLError,
    "Cardinality mismatch inferred at compile time."
);
pyo3::create_exception!(
    pylon._core,
    PyQLFragmentError,
    PyQLError,
    "Failure compiling a schema-level PyQL fragment during schema export."
);

pyo3::create_exception!(
    pylon._core,
    PylonExecutionError,
    pyo3::exceptions::PyException,
    "Base class for all Pylon execution errors."
);
pyo3::create_exception!(
    pylon._core,
    PylonConstraintViolationError,
    PylonExecutionError,
    "A database constraint (unique, check, exclusive) was violated."
);
pyo3::create_exception!(
    pylon._core,
    PylonCardinalityViolationError,
    PylonExecutionError,
    "A single-cardinality field received multiple values."
);
pyo3::create_exception!(
    pylon._core,
    PylonMissingRequiredError,
    PylonExecutionError,
    "A required property or link was not provided."
);
pyo3::create_exception!(
    pylon._core,
    PylonInvalidValueError,
    PylonExecutionError,
    "Invalid value for a type (e.g. out-of-range, bad format)."
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

// ── Field descriptors ──────────────────────────────────────────────────────────

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
        description = None,
        check_constraints = None,
        is_exclusive = false,
        is_pk = false,
        is_readonly = false,
        rewrites = None
    ))]
    fn new(
        name: String,
        pg_type: String,
        nullable: bool,
        default_sql: Option<String>,
        description: Option<String>,
        check_constraints: Option<Vec<String>>,
        is_exclusive: bool,
        is_pk: bool,
        is_readonly: bool,
        rewrites: Option<Vec<PyRef<RewriteEntry>>>,
    ) -> Self {
        Self {
            inner: core::schema::PropertyDescriptor {
                name,
                pg_type,
                nullable,
                default_sql,
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
        description = None,
        on_delete = None
    ))]
    fn new(
        name: String,
        target: String,
        through: Option<String>,
        nullable: bool,
        description: Option<String>,
        on_delete: Option<Vec<PyRef<OnDeletePolicy>>>,
    ) -> Self {
        Self {
            inner: core::schema::MultiLinkDescriptor {
                name,
                target,
                through,
                nullable,
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
        fields,
        *,
        expression = None,
        unique = false,
        unless = None
    ))]
    fn new(
        fields: Vec<String>,
        expression: Option<String>,
        unique: bool,
        unless: Option<String>,
    ) -> Self {
        Self {
            inner: core::schema::IndexDescriptor {
                fields,
                expression,
                unique,
                unless,
            },
        }
    }

    #[getter]
    fn fields(&self) -> Vec<String> {
        self.inner.fields.clone()
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

/// Composite UNIQUE constraint across multiple fields.
#[pyclass(module = "pylon._core", frozen)]
pub struct ExclusiveConstraint {
    inner: core::schema::TypeConstraint,
}

#[pymethods]
impl ExclusiveConstraint {
    #[new]
    #[pyo3(signature = (fields, *, unless = None))]
    fn new(fields: Vec<String>, unless: Option<String>) -> Self {
        Self {
            inner: core::schema::TypeConstraint::Exclusive { fields, unless },
        }
    }

    #[getter]
    fn fields(&self) -> Vec<String> {
        match &self.inner {
            core::schema::TypeConstraint::Exclusive { fields, .. } => fields.clone(),
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
        description = None,
        parents = None,
        interfaces = None,
        exclusive_constraints = None,
        expression_constraints = None,
        indexes = None,
        triggers = None
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
        description: Option<String>,
        parents: Option<Vec<String>>,
        interfaces: Option<Vec<String>>,
        exclusive_constraints: Option<Vec<PyRef<ExclusiveConstraint>>>,
        expression_constraints: Option<Vec<PyRef<ExpressionConstraint>>>,
        indexes: Option<Vec<PyRef<IndexDescriptor>>>,
        triggers: Option<Vec<PyRef<TriggerDescriptor>>>,
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
                triggers: triggers
                    .unwrap_or_default()
                    .iter()
                    .map(|t| t.inner.clone())
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
}

// ── Scalar / enum / global descriptors ────────────────────────────────────────

#[pyclass(module = "pylon._core", frozen)]
pub struct ScalarDescriptor {
    inner: core::schema::ScalarDescriptor,
}

#[pymethods]
impl ScalarDescriptor {
    #[new]
    #[pyo3(signature = (name, module, base, pg_type, *, check_constraints = None))]
    fn new(
        name: String,
        module: String,
        base: String,
        pg_type: String,
        check_constraints: Option<Vec<String>>,
    ) -> Self {
        Self {
            inner: core::schema::ScalarDescriptor {
                name,
                module,
                base,
                pg_type,
                check_constraints: check_constraints.unwrap_or_default(),
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
pub struct GlobalDescriptor {
    inner: core::schema::GlobalDescriptor,
}

#[pymethods]
impl GlobalDescriptor {
    #[new]
    #[pyo3(signature = (name, module, scalar_type, required, *, default_expr = None))]
    fn new(
        name: String,
        module: String,
        scalar_type: String,
        required: bool,
        default_expr: Option<String>,
    ) -> Self {
        Self {
            inner: core::schema::GlobalDescriptor {
                name,
                module,
                scalar_type,
                required,
                default_expr,
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
    #[pyo3(signature = (*, types = None, scalars = None, enums = None, globals = None))]
    fn new(
        types: Option<Vec<PyRef<TypeDescriptor>>>,
        scalars: Option<Vec<PyRef<ScalarDescriptor>>>,
        enums: Option<Vec<PyRef<EnumDescriptor>>>,
        globals: Option<Vec<PyRef<GlobalDescriptor>>>,
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
                globals: globals
                    .unwrap_or_default()
                    .iter()
                    .map(|g| g.inner.clone())
                    .collect(),
            },
        }
    }

    #[getter]
    fn type_count(&self) -> usize {
        self.inner.types.len()
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
    fn global_count(&self) -> usize {
        self.inner.globals.len()
    }
}

// ── Query types ────────────────────────────────────────────────────────────────

#[pyclass(module = "pylon._core", frozen)]
pub struct CompiledQuery {
    inner: core::query::CompiledQuery,
}

#[pymethods]
impl CompiledQuery {
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
}

// ── Public functions ───────────────────────────────────────────────────────────

#[pyfunction]
fn compile(query: &str, schema: &SchemaDescriptor) -> PyResult<CompiledQuery> {
    core::query::compile(query, &schema.inner)
        .map(|q| CompiledQuery { inner: q })
        .map_err(pyql_err)
}

#[pyfunction]
fn export_schema(schema: &SchemaDescriptor) -> PyResult<String> {
    core::export::export_schema(&schema.inner).map_err(pyql_err)
}

#[pyfunction]
fn export_stdlib() -> String {
    core::stdlib::export_stdlib()
}

// ── Shape conversion ───────────────────────────────────────────────────────────

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
        ShapeNode::Object { name, type_name, position, cardinality, fields } => {
            d.set_item("kind", "object")?;
            d.set_item("name", name.as_str())?;
            d.set_item("type_name", type_name.as_deref())?;
            d.set_item("position", position)?;
            d.set_item("cardinality", match cardinality {
                Cardinality::Required => "required",
                Cardinality::Optional => "optional",
                Cardinality::Many    => "many",
            })?;
            let py_fields = PyList::new(
                py,
                fields.iter().map(|f| shape_node_to_py(py, f)).collect::<PyResult<Vec<_>>>()?,
            )?;
            d.set_item("fields", py_fields)?;
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
    }
    Ok(d.into_any())
}

// ── Error conversion ───────────────────────────────────────────────────────────

fn pyql_err(err: core::error::PyQLError) -> PyErr {
    match err {
        core::error::PyQLError::Syntax(e) => PyQLSyntaxError::new_err(e.message),
        core::error::PyQLError::Type(e) => PyQLTypeError::new_err(e.message),
        core::error::PyQLError::Resolution(e) => match e {
            core::error::PyQLResolutionError::UnknownType(e) => {
                PyQLUnknownTypeError::new_err(e.message)
            }
            core::error::PyQLResolutionError::UnknownField(e) => {
                PyQLUnknownFieldError::new_err(e.message)
            }
            core::error::PyQLResolutionError::UnknownParameter(e) => {
                PyQLUnknownParameterError::new_err(e.message)
            }
        },
        core::error::PyQLError::Cardinality(e) => PyQLCardinalityError::new_err(e.message),
        core::error::PyQLError::Fragment(e) => PyQLFragmentError::new_err(e.message),
    }
}

// ── Module ─────────────────────────────────────────────────────────────────────

#[pymodule]
fn _core(m: &Bound<'_, PyModule>) -> PyResult<()> {
    let py = m.py();

    // Exceptions — compilation
    m.add("PyQLError", PyQLError::type_object(py))?;
    m.add("PyQLSyntaxError", PyQLSyntaxError::type_object(py))?;
    m.add("PyQLTypeError", PyQLTypeError::type_object(py))?;
    m.add("PyQLResolutionError", PyQLResolutionError::type_object(py))?;
    m.add("PyQLUnknownTypeError", PyQLUnknownTypeError::type_object(py))?;
    m.add("PyQLUnknownFieldError", PyQLUnknownFieldError::type_object(py))?;
    m.add("PyQLUnknownParameterError", PyQLUnknownParameterError::type_object(py))?;
    m.add("PyQLCardinalityError", PyQLCardinalityError::type_object(py))?;
    m.add("PyQLFragmentError", PyQLFragmentError::type_object(py))?;

    // Exceptions — execution
    m.add("PylonExecutionError", PylonExecutionError::type_object(py))?;
    m.add("PylonConstraintViolationError", PylonConstraintViolationError::type_object(py))?;
    m.add("PylonCardinalityViolationError", PylonCardinalityViolationError::type_object(py))?;
    m.add("PylonMissingRequiredError", PylonMissingRequiredError::type_object(py))?;
    m.add("PylonInvalidValueError", PylonInvalidValueError::type_object(py))?;

    // Deletion policy
    m.add_class::<OnDeletePolicy>()?;

    // Field descriptors
    m.add_class::<RewriteEntry>()?;
    m.add_class::<PropertyDescriptor>()?;
    m.add_class::<LinkDescriptor>()?;
    m.add_class::<MultiLinkDescriptor>()?;
    m.add_class::<ComputedDescriptor>()?;

    // Type-level constructs
    m.add_class::<IndexDescriptor>()?;
    m.add_class::<TriggerDescriptor>()?;
    m.add_class::<ExclusiveConstraint>()?;
    m.add_class::<ExpressionConstraint>()?;

    // Top-level descriptors
    m.add_class::<TypeDescriptor>()?;
    m.add_class::<ScalarDescriptor>()?;
    m.add_class::<EnumDescriptor>()?;
    m.add_class::<GlobalDescriptor>()?;
    m.add_class::<SchemaDescriptor>()?;

    // Query types
    m.add_class::<CompiledQuery>()?;

    // Functions
    m.add_function(wrap_pyfunction!(compile, m)?)?;
    m.add_function(wrap_pyfunction!(export_schema, m)?)?;
    m.add_function(wrap_pyfunction!(export_stdlib, m)?)?;
    Ok(())
}
