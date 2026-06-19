use crate::error::PyQLError;
use crate::schema::SchemaDescriptor;

/// Enclosing context for compiling a schema-level PyQL fragment.
#[derive(Debug, Clone)]
pub struct FragmentContext {
    /// Module-qualified name of the enclosing type, e.g. `default::Product`.
    pub enclosing_type: String,
    /// Name of the field this fragment belongs to, if any.
    pub field_name: Option<String>,
    /// Variables in scope for this fragment, e.g. `__subject__` for constraints/rewrites.
    pub scope_vars: Vec<String>,
}

/// Export the full schema as a PostgreSQL DDL string.
///
/// PyQL fragments (computed columns, constraints, mutation rewrite trigger bodies)
/// are compiled to SQL inline during export. Failures surface as `PyQLFragmentError`.
/// The returned string is valid PostgreSQL DDL ready for Atlas or direct inspection.
pub fn export_schema(_schema: &SchemaDescriptor) -> Result<String, PyQLError> {
    todo!("Schema export not yet implemented")
}

/// Compile a schema-level PyQL expression fragment to a raw SQL expression string.
///
/// Separate entry point from `compile()` — called only by the schema exporter,
/// never by application code.
pub(crate) fn compile_fragment(
    _expression: &str,
    _context: &FragmentContext,
    _schema: &SchemaDescriptor,
) -> Result<String, PyQLError> {
    todo!("Fragment compilation not yet implemented")
}
