use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Position {
    pub line: u32,
    pub col: u32,
}

// ── Compilation errors ─────────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum PyQLError {
    #[error(transparent)]
    Syntax(#[from] PyQLSyntaxError),
    #[error(transparent)]
    Type(#[from] PyQLTypeError),
    #[error(transparent)]
    Resolution(#[from] PyQLResolutionError),
    #[error(transparent)]
    Cardinality(#[from] PyQLCardinalityError),
    #[error(transparent)]
    Fragment(#[from] PyQLFragmentError),
}

impl PyQLError {
    /// Maps this error to the `pylon.exceptions.*` class name it corresponds to,
    /// plus its message and position — the single source of truth shared by the
    /// Python binding (`pylon-py`'s `pyql_err`) and the language server's
    /// diagnostics, so both surfaces stay in sync with exactly one match arm set.
    pub fn class_name_message_position(&self) -> (&'static str, &str, &Position) {
        use PyQLError as E;
        use PyQLResolutionError as R;
        match self {
            E::Syntax(e) => ("InvalidQueryError", &e.message, &e.position),
            E::Type(e) => ("InvalidQueryError", &e.message, &e.position),
            E::Resolution(R::UnknownType(e)) => ("UnknownTypeError", &e.message, &e.position),
            E::Resolution(R::UnknownField(e)) => ("UnknownLinkError", &e.message, &e.position),
            E::Resolution(R::UnknownParameter(e)) => {
                ("UnknownParameterError", &e.message, &e.position)
            }
            E::Cardinality(e) => ("InvalidQueryError", &e.message, &e.position),
            E::Fragment(e) => ("SchemaError", &e.message, &e.position),
        }
    }
}

#[derive(Debug, Error, Clone)]
#[error("{message}")]
pub struct PyQLSyntaxError {
    pub message: String,
    pub position: Position,
}

#[derive(Debug, Error, Clone)]
#[error("{message}")]
pub struct PyQLTypeError {
    pub message: String,
    pub position: Position,
}

/// Base error for unknown-identifier failures; variants correspond to the Python subclass hierarchy.
#[derive(Debug, Error)]
pub enum PyQLResolutionError {
    #[error(transparent)]
    UnknownType(#[from] PyQLUnknownTypeError),
    #[error(transparent)]
    UnknownField(#[from] PyQLUnknownFieldError),
    #[error(transparent)]
    UnknownParameter(#[from] PyQLUnknownParameterError),
}

#[derive(Debug, Error, Clone)]
#[error("{message}")]
pub struct PyQLUnknownTypeError {
    pub message: String,
    pub position: Position,
}

#[derive(Debug, Error, Clone)]
#[error("{message}")]
pub struct PyQLUnknownFieldError {
    pub message: String,
    pub position: Position,
}

#[derive(Debug, Error, Clone)]
#[error("{message}")]
pub struct PyQLUnknownParameterError {
    pub message: String,
    pub position: Position,
}

#[derive(Debug, Error, Clone)]
#[error("{message}")]
pub struct PyQLCardinalityError {
    pub message: String,
    pub position: Position,
}

#[derive(Debug, Error, Clone)]
#[error("{message}")]
pub struct PyQLFragmentError {
    pub message: String,
    pub position: Position,
    /// Identifies the failing schema element, e.g. 'Product.total_price (computed)'.
    pub context: String,
}

// ── Execution errors ───────────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum PylonExecutionError {
    #[error(transparent)]
    ConstraintViolation(#[from] PylonConstraintViolationError),
    #[error(transparent)]
    CardinalityViolation(#[from] PylonCardinalityViolationError),
    #[error(transparent)]
    MissingRequired(#[from] PylonMissingRequiredError),
    #[error(transparent)]
    InvalidValue(#[from] PylonInvalidValueError),
}

#[derive(Debug, Error, Clone)]
#[error("{message}")]
pub struct PylonConstraintViolationError {
    pub message: String,
}

#[derive(Debug, Error, Clone)]
#[error("{message}")]
pub struct PylonCardinalityViolationError {
    pub message: String,
}

#[derive(Debug, Error, Clone)]
#[error("{message}")]
pub struct PylonMissingRequiredError {
    pub message: String,
}

#[derive(Debug, Error, Clone)]
#[error("{message}")]
pub struct PylonInvalidValueError {
    pub message: String,
}
