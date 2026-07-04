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
        Vector => "vector".into(),
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

/// DDL for the `_pylon."IndexOutbox"` table and its supporting types.
///
/// Emitted once, at schema-bootstrap time, before any user-schema DDL.
/// `index_name IS NULL` represents the default (unnamed) index on a type;
/// `NULLS NOT DISTINCT` on the unique constraint collapses multiple writes
/// to the same object/index into a single outstanding job.
pub const INDEX_OUTBOX_DDL: &str = concat!(
    "DO $$ BEGIN\n",
    "    CREATE TYPE _pylon.\"IndexKind\" AS ENUM ('Vector', 'OpenSearch');\n",
    "EXCEPTION WHEN duplicate_object THEN NULL; END $$;\n",
    "DO $$ BEGIN\n",
    "    CREATE TYPE _pylon.\"IndexOutboxStatus\" AS ENUM ('Pending', 'Processing', 'Failed');\n",
    "EXCEPTION WHEN duplicate_object THEN NULL; END $$;\n\n",
    "CREATE TABLE IF NOT EXISTS _pylon.\"IndexOutbox\" (\n",
    "    id            uuid        NOT NULL DEFAULT uuidv7(),\n",
    "    object_id     uuid        NOT NULL,\n",
    "    type_name     text        NOT NULL,\n",
    "    index_kind    _pylon.\"IndexKind\"         NOT NULL,\n",
    "    index_name    text,\n",
    "    operation     text        NOT NULL DEFAULT 'index',\n",
    "    status        _pylon.\"IndexOutboxStatus\" NOT NULL DEFAULT 'Pending',\n",
    "    attempts      int         NOT NULL DEFAULT 0,\n",
    "    enqueued_at   timestamptz NOT NULL DEFAULT now(),\n",
    "    next_attempt  timestamptz,\n",
    "    PRIMARY KEY (id),\n",
    "    UNIQUE NULLS NOT DISTINCT (object_id, index_kind, index_name)\n",
    ");\n",
    "ALTER TABLE _pylon.\"IndexOutbox\" ADD COLUMN IF NOT EXISTS\n",
    "    operation text NOT NULL DEFAULT 'index';\n\n",
    "CREATE INDEX IF NOT EXISTS \"IndexOutbox_status_next_attempt\" ON _pylon.\"IndexOutbox\" (status, next_attempt)\n",
    "    WHERE status IN ('Pending', 'Failed');\n\n",
    "CREATE OR REPLACE FUNCTION _pylon.notify_index_queue()\n",
    "    RETURNS trigger LANGUAGE plpgsql AS $$\n",
    "BEGIN\n",
    "    PERFORM pg_notify('pylon_index_queue', NEW.object_id::text);\n",
    "    RETURN NEW;\n",
    "END\n",
    "$$;\n\n",
    "CREATE OR REPLACE TRIGGER notify_index_queue\n",
    "    AFTER INSERT OR UPDATE ON _pylon.\"IndexOutbox\"\n",
    "    FOR EACH ROW EXECUTE FUNCTION _pylon.notify_index_queue();\n",
);

/// DDL for the `_pylon."Migrations"` and `_pylon."Progress"` tracking tables (§7).
///
/// Emitted alongside `INDEX_OUTBOX_DDL` at schema-bootstrap time.
pub const MIGRATION_TRACKING_DDL: &str = concat!(
    "CREATE TABLE IF NOT EXISTS _pylon.\"Migrations\" (\n",
    "    id          text        PRIMARY KEY,\n",
    "    onto        text        NOT NULL,\n",
    "    filename    text        NOT NULL,\n",
    "    db_state    jsonb       NULL,\n",
    "    applied_at  timestamptz NULL\n",
    ");\n\n",
    "CREATE TABLE IF NOT EXISTS _pylon.\"Progress\" (\n",
    "    id          text        PRIMARY KEY,\n",
    "    step_index  integer     NOT NULL,\n",
    "    updated_at  timestamptz NOT NULL DEFAULT now()\n",
    ");\n",
);

/// Generate the complete `_pylon` schema DDL from the stdlib registry.
///
/// Every `ImplStrategy::PylonFunction` entry contributes one
/// `CREATE OR REPLACE FUNCTION` statement — overloads generate separate
/// statements and PostgreSQL resolves them by argument types.
/// `TranspilerIntrinsic` entries (range, multirange) are skipped.
pub fn export_stdlib() -> String {
    let mut out = String::from("CREATE SCHEMA IF NOT EXISTS _pylon;\n\n");

    out.push_str(INDEX_OUTBOX_DDL);
    out.push('\n');
    out.push_str(MIGRATION_TRACKING_DDL);
    out.push('\n');

    // Internal runtime helpers (not user-callable from PyQL).
    out.push_str(concat!(
        "CREATE OR REPLACE FUNCTION _pylon.array_subscript(arr anyarray, idx bigint)\n",
        "\tRETURNS anyelement\n",
        "\tLANGUAGE plpgsql STABLE PARALLEL SAFE\n",
        "AS $$\n",
        "BEGIN\n",
        "    IF idx < 0 OR idx >= cardinality(arr) THEN\n",
        "        RAISE EXCEPTION 'array index % is out of bounds', idx\n",
        "            USING ERRCODE = 'array_subscript_error';\n",
        "    END IF;\n",
        "    RETURN arr[idx + 1];\n",
        "END\n",
        "$$;\n\n",
        "CREATE OR REPLACE FUNCTION _pylon.str_subscript(s text, idx bigint)\n",
        "\tRETURNS text\n",
        "\tLANGUAGE plpgsql STABLE PARALLEL SAFE\n",
        "AS $$\n",
        "BEGIN\n",
        "    IF idx < 0 OR idx >= char_length(s) THEN\n",
        "        RAISE EXCEPTION 'string index % is out of bounds', idx\n",
        "            USING ERRCODE = 'array_subscript_error';\n",
        "    END IF;\n",
        "    RETURN substr(s, (idx + 1)::int, 1);\n",
        "END\n",
        "$$;\n\n",
        "CREATE OR REPLACE FUNCTION _pylon.str_subscript(s bytea, idx bigint)\n",
        "\tRETURNS bytea\n",
        "\tLANGUAGE plpgsql STABLE PARALLEL SAFE\n",
        "AS $$\n",
        "BEGIN\n",
        "    IF idx < 0 OR idx >= length(s) THEN\n",
        "        RAISE EXCEPTION 'bytes index % is out of bounds', idx\n",
        "            USING ERRCODE = 'array_subscript_error';\n",
        "    END IF;\n",
        "    RETURN substr(s, (idx + 1)::int, 1);\n",
        "END\n",
        "$$;\n\n",
    ));

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
