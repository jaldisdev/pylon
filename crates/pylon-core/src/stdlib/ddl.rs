use super::{FnDescriptor, FnVolatility, ImplStrategy, Param, PylonFnDef, PylonType, SqlLanguage};

// ── PostgreSQL type mapping ───────────────────────────────────────────────────

fn pg_type(ty: &PylonType) -> String {
    use PylonType::*;
    match ty {
        Str => "text".into(),
        Bool => "bool".into(),
        Int16 => "int2".into(),
        Int32 => "int4".into(),
        Int64 => "int8".into(),
        Float32 => "float4".into(),
        Float64 => "float8".into(),
        Decimal | BigInt => "numeric".into(),
        Uuid => "uuid".into(),
        Json => "jsonb".into(),
        Bytes => "bytea".into(),
        Datetime => "timestamptz".into(),
        Duration | RelativeDuration => "interval".into(),
        LocalDatetime => "timestamp".into(),
        LocalDate => "date".into(),
        LocalTime => "time".into(),
        Any | AnyOrderable | AnyPoint => "anyelement".into(),
        Array(inner) => match inner.as_ref() {
            Any | AnyOrderable | AnyPoint => "anyarray".into(),
            other => format!("{}[]", pg_type(other)),
        },
        // Set in parameter position: transpiler converts the set to an array before the call.
        Set(inner) => match inner.as_ref() {
            Any | AnyOrderable | AnyPoint => "anyarray".into(),
            other => format!("{}[]", pg_type(other)),
        },
        Optional(inner) => pg_type(inner),
        Range(inner) => match inner.as_ref() {
            Any | AnyOrderable | AnyPoint => "anyrange".into(),
            other => format!("{}range", pg_type(other)),
        },
        Multirange(inner) => match inner.as_ref() {
            Any | AnyOrderable | AnyPoint => "anymultirange".into(),
            other => format!("{}multirange", pg_type(other)),
        },
        Tuple(_) => panic!("Tuple cannot appear as a PG function parameter type"),
    }
}

/// Derive the PostgreSQL RETURNS clause from a `PylonType`.
/// Call sites may override this via `PylonFnDef::returns_override`.
fn pg_returns(ty: &PylonType) -> String {
    use PylonType::*;
    match ty {
        Set(inner) => match inner.as_ref() {
            Tuple(_) => panic!("TABLE returns must use PylonFnDef::returns_override"),
            other => format!("SETOF {}", pg_type(other)),
        },
        Optional(inner) => pg_type(inner),
        other => pg_type(other),
    }
}

/// Render the parameter list for a `CREATE FUNCTION` statement.
///
/// A variadic param that is last becomes `VARIADIC type[]`; one in a non-last
/// position is emitted as a plain `type[]` (the transpiler collects args into
/// the array before the call).
fn pg_params(params: &[Param]) -> String {
    if params.is_empty() {
        return String::new();
    }
    let last = params.len() - 1;
    params
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let arr_ty = match &p.ty {
                PylonType::Any | PylonType::AnyOrderable | PylonType::AnyPoint => {
                    "anyarray".into()
                }
                other => format!("{}[]", pg_type(other)),
            };
            if p.variadic && i == last {
                // PG syntax: VARIADIC name type[]
                format!("VARIADIC {} {}", p.name, arr_ty)
            } else if p.variadic {
                // Non-last variadic: transpiler collects into array; no VARIADIC keyword
                format!("{} {}", p.name, arr_ty)
            } else {
                format!("{} {}", p.name, pg_type(&p.ty))
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

// ── DDL generator ─────────────────────────────────────────────────────────────

fn render_function(desc: &FnDescriptor, def: &PylonFnDef) -> String {
    let params = pg_params(&desc.params);
    let returns = def
        .returns_override
        .map(|s| s.to_owned())
        .unwrap_or_else(|| pg_returns(&desc.return_type));
    let lang = match def.language {
        SqlLanguage::Sql => "sql",
        SqlLanguage::PlPgSql => "plpgsql",
    };
    let volatility = match def.volatility {
        FnVolatility::Immutable => "IMMUTABLE",
        FnVolatility::Stable => "STABLE",
    };
    let strict = if def.strict { " STRICT" } else { "" };

    format!(
        "CREATE OR REPLACE FUNCTION _pylon.{name}({params})\n\
         \tRETURNS {returns}\n\
         \tLANGUAGE {lang} {volatility} PARALLEL SAFE{strict}\n\
         AS $$\n\
         {body}\n\
         $$;\n",
        name = def.name,
        body = def.body,
    )
}

/// Generate the complete `_pylon` schema DDL from the stdlib registry.
///
/// Every `ImplStrategy::PylonFunction` entry contributes one
/// `CREATE OR REPLACE FUNCTION` statement — overloads generate separate
/// statements and PostgreSQL resolves them by argument types.
/// `TranspilerIntrinsic` entries (range, multirange) are skipped.
pub fn export_stdlib() -> String {
    let mut out = String::from("CREATE SCHEMA IF NOT EXISTS _pylon;\n\n");
    for desc in super::registry() {
        if let ImplStrategy::PylonFunction(def) = &desc.impl_strategy {
            out.push_str(&render_function(desc, def));
            out.push('\n');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::export_stdlib;

    #[test]
    fn ddl_smoke() {
        let ddl = export_stdlib();
        let fn_count = ddl.matches("CREATE OR REPLACE FUNCTION").count();
        assert!(ddl.starts_with("CREATE SCHEMA IF NOT EXISTS _pylon;"));
        assert!(fn_count > 0, "no functions generated");
        assert!(ddl.contains("_pylon.to_bool"), "to_bool missing");
        assert!(ddl.contains("_pylon.enumerate"), "enumerate missing");
        assert!(ddl.contains("_pylon.datetime_get"), "datetime_get missing");
        assert!(!ddl.contains("_pylon.range("), "range must not be installed (TranspilerIntrinsic)");
        assert!(!ddl.contains("_pylon.multirange("), "multirange must not be installed");
        eprintln!("export_stdlib: {} PylonFunction overloads installed", fn_count);
    }

    #[test]
    fn ddl_to_bool_has_three_overloads() {
        let ddl = export_stdlib();
        let count = ddl.matches("_pylon.to_bool(").count();
        assert_eq!(count, 3, "expected int2/int4/int8 overloads; got {count}");
    }

    #[test]
    fn ddl_enumerate_returns_table() {
        let ddl = export_stdlib();
        assert!(ddl.contains("RETURNS TABLE(index bigint, value anyelement)"));
    }

    #[test]
    fn ddl_json_get_uses_variadic() {
        let ddl = export_stdlib();
        assert!(ddl.contains("VARIADIC path text[]"), "json_get must use VARIADIC");
    }
}
