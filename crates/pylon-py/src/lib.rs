use pyo3::prelude::*;
use pyo3::PyTypeInfo;
use pylon_core as core;

// ── Exception hierarchy ────────────────────────────────────────────────────────
// Compilation errors

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

// Execution errors

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

// ── Enums ──────────────────────────────────────────────────────────────────────

#[pyclass(eq, frozen, from_py_object, module = "pylon._core")]
#[derive(Clone, PartialEq)]
pub enum CardinalityMode {
    Required,
    Optional,
    Many,
}

impl From<CardinalityMode> for core::schema::CardinalityMode {
    fn from(v: CardinalityMode) -> Self {
        match v {
            CardinalityMode::Required => core::schema::CardinalityMode::Required,
            CardinalityMode::Optional => core::schema::CardinalityMode::Optional,
            CardinalityMode::Many => core::schema::CardinalityMode::Many,
        }
    }
}

#[pyclass(eq, frozen, from_py_object, module = "pylon._core")]
#[derive(Clone, PartialEq)]
pub enum FieldKind {
    Scalar,
    Link,
    MultiLink,
    Computed,
}

impl From<FieldKind> for core::schema::FieldKind {
    fn from(v: FieldKind) -> Self {
        match v {
            FieldKind::Scalar => core::schema::FieldKind::Scalar,
            FieldKind::Link => core::schema::FieldKind::Link,
            FieldKind::MultiLink => core::schema::FieldKind::MultiLink,
            FieldKind::Computed => core::schema::FieldKind::Computed,
        }
    }
}

// ── Schema descriptor types ────────────────────────────────────────────────────

#[pyclass(module = "pylon._core", frozen)]
pub struct FieldDescriptor {
    inner: core::schema::FieldDescriptor,
}

#[pymethods]
impl FieldDescriptor {
    #[new]
    fn new(
        name: String,
        kind: FieldKind,
        cardinality: CardinalityMode,
        target: Option<String>,
        scalar_type: Option<String>,
    ) -> Self {
        Self {
            inner: core::schema::FieldDescriptor {
                name,
                kind: kind.into(),
                cardinality: cardinality.into(),
                target,
                scalar_type,
            },
        }
    }

    #[getter]
    fn name(&self) -> &str {
        &self.inner.name
    }
}

#[pyclass(module = "pylon._core", frozen)]
pub struct TypeDescriptor {
    inner: core::schema::TypeDescriptor,
}

#[pymethods]
impl TypeDescriptor {
    /// `abstract_` corresponds to Python keyword argument `abstract`.
    #[new]
    #[pyo3(signature = (name, fields, *, abstract_ = false, materialized = true))]
    fn new(
        name: String,
        fields: Vec<PyRef<FieldDescriptor>>,
        abstract_: bool,
        materialized: bool,
    ) -> Self {
        Self {
            inner: core::schema::TypeDescriptor {
                name,
                fields: fields.iter().map(|f| f.inner.clone()).collect(),
                abstract_,
                materialized,
            },
        }
    }

    #[getter]
    fn name(&self) -> &str {
        &self.inner.name
    }
}

#[pyclass(module = "pylon._core", frozen)]
pub struct ScalarDescriptor {
    inner: core::schema::ScalarDescriptor,
}

#[pymethods]
impl ScalarDescriptor {
    #[new]
    fn new(
        name: String,
        base: String,
        pg_type: String,
        constraints: Vec<String>,
        module: String,
    ) -> Self {
        Self {
            inner: core::schema::ScalarDescriptor {
                name,
                base,
                pg_type,
                constraints,
                module,
            },
        }
    }

    #[getter]
    fn name(&self) -> &str {
        &self.inner.name
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
    fn new(
        types: Vec<PyRef<TypeDescriptor>>,
        scalars: Vec<PyRef<ScalarDescriptor>>,
    ) -> Self {
        Self {
            inner: core::schema::SchemaDescriptor {
                types: types.iter().map(|t| t.inner.clone()).collect(),
                scalars: scalars.iter().map(|s| s.inner.clone()).collect(),
            },
        }
    }
}

// ── Query types ────────────────────────────────────────────────────────────────

/// The output of a successful PyQL compilation. Immutable and safe to cache.
#[pyclass(module = "pylon._core", frozen)]
pub struct CompiledQuery {
    inner: core::query::CompiledQuery,
}

#[pymethods]
impl CompiledQuery {
    /// PostgreSQL SQL string ready for execution.
    #[getter]
    fn sql(&self) -> &str {
        &self.inner.sql
    }

    /// Positional bound parameters ($1, $2, …).
    #[getter]
    fn params<'py>(&self, py: Python<'py>) -> Bound<'py, pyo3::types::PyList> {
        // TODO: convert QueryParam variants to Python scalar types
        pyo3::types::PyList::empty(py)
    }
}

// ── Public functions ───────────────────────────────────────────────────────────

/// Compile a PyQL string to SQL against schema. Raises PyQLError on failure.
#[pyfunction]
fn compile(query: &str, schema: &SchemaDescriptor) -> PyResult<CompiledQuery> {
    core::query::compile(query, &schema.inner)
        .map(|q| CompiledQuery { inner: q })
        .map_err(pyql_err)
}

/// Export the full schema as a PostgreSQL DDL string. Raises PyQLError on failure.
#[pyfunction]
fn export_schema(schema: &SchemaDescriptor) -> PyResult<String> {
    core::export::export_schema(&schema.inner).map_err(pyql_err)
}

/// Deserialize asyncpg Records into Python objects using the shape embedded in query.
///
/// The top-level result is always a list since PyQL select is set-valued.
#[pyfunction]
fn deserialize(
    _records: &Bound<'_, PyAny>,
    _query: &CompiledQuery,
    _registry: &Bound<'_, PyAny>,
) -> PyResult<Py<PyAny>> {
    todo!("Deserializer not yet implemented")
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

    // Exceptions — compilation (base before derived)
    m.add("PyQLError", PyQLError::type_object(py))?;
    m.add("PyQLSyntaxError", PyQLSyntaxError::type_object(py))?;
    m.add("PyQLTypeError", PyQLTypeError::type_object(py))?;
    m.add("PyQLResolutionError", PyQLResolutionError::type_object(py))?;
    m.add("PyQLUnknownTypeError", PyQLUnknownTypeError::type_object(py))?;
    m.add("PyQLUnknownFieldError", PyQLUnknownFieldError::type_object(py))?;
    m.add("PyQLUnknownParameterError", PyQLUnknownParameterError::type_object(py))?;
    m.add("PyQLCardinalityError", PyQLCardinalityError::type_object(py))?;
    m.add("PyQLFragmentError", PyQLFragmentError::type_object(py))?;

    // Exceptions — execution (base before derived)
    m.add("PylonExecutionError", PylonExecutionError::type_object(py))?;
    m.add("PylonConstraintViolationError", PylonConstraintViolationError::type_object(py))?;
    m.add("PylonCardinalityViolationError", PylonCardinalityViolationError::type_object(py))?;
    m.add("PylonMissingRequiredError", PylonMissingRequiredError::type_object(py))?;
    m.add("PylonInvalidValueError", PylonInvalidValueError::type_object(py))?;

    // Enums
    m.add_class::<CardinalityMode>()?;
    m.add_class::<FieldKind>()?;

    // Schema descriptor types
    m.add_class::<FieldDescriptor>()?;
    m.add_class::<TypeDescriptor>()?;
    m.add_class::<ScalarDescriptor>()?;
    m.add_class::<SchemaDescriptor>()?;

    // Query types
    m.add_class::<CompiledQuery>()?;

    // Functions
    m.add_function(wrap_pyfunction!(compile, m)?)?;
    m.add_function(wrap_pyfunction!(export_schema, m)?)?;
    m.add_function(wrap_pyfunction!(deserialize, m)?)?;

    Ok(())
}
