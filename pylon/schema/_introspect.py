"""PostgreSQL pg_catalog introspection for schema diffing.

Queries the live database and returns a populated DbState (pylon._core.DbState)
describing the current user-managed schema structure. System schemas (_pylon,
public, pg_*, information_schema) are excluded automatically.
"""

from __future__ import annotations

from typing import TYPE_CHECKING

if TYPE_CHECKING:
    import asyncpg
    from pylon._core import DbState

# Schemas we never manage or diff against.
_SYSTEM_SCHEMAS = (
    "_pylon",
    "public",
    "pg_catalog",
    "information_schema",
    "pg_toast",
)

_SCHEMAS_SQL = """
SELECT nspname
FROM pg_namespace
WHERE nspname NOT LIKE 'pg_%'
  AND nspname <> ALL($1::text[])
ORDER BY nspname
"""

_ENUMS_SQL = """
SELECT n.nspname AS schema, t.typname AS name, e.enumlabel AS member
FROM pg_type t
JOIN pg_namespace n ON n.oid = t.typnamespace
JOIN pg_enum e ON e.enumtypid = t.oid
WHERE t.typtype = 'e'
  AND n.nspname NOT LIKE 'pg_%'
  AND n.nspname <> ALL($1::text[])
ORDER BY n.nspname, t.typname, e.enumsortorder
"""

_DOMAINS_SQL = """
SELECT n.nspname AS schema, t.typname AS name
FROM pg_type t
JOIN pg_namespace n ON n.oid = t.typnamespace
WHERE t.typtype = 'd'
  AND n.nspname NOT LIKE 'pg_%'
  AND n.nspname <> ALL($1::text[])
ORDER BY n.nspname, t.typname
"""

_TABLES_SQL = """
SELECT n.nspname AS schema, c.relname AS name
FROM pg_class c
JOIN pg_namespace n ON n.oid = c.relnamespace
WHERE c.relkind = 'r'
  AND n.nspname NOT LIKE 'pg_%'
  AND n.nspname <> ALL($1::text[])
ORDER BY n.nspname, c.relname
"""

_COLUMNS_SQL = """
SELECT
    a.attname AS name,
    pg_catalog.format_type(a.atttypid, a.atttypmod) AS pg_type,
    NOT a.attnotnull AS nullable,
    a.attgenerated = 's' AS is_generated,
    CASE WHEN a.atthasdef AND a.attgenerated = '' THEN
        pg_catalog.pg_get_expr(d.adbin, d.adrelid)
    END AS column_default
FROM pg_attribute a
JOIN pg_class c ON c.oid = a.attrelid
JOIN pg_namespace n ON n.oid = c.relnamespace
LEFT JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum
WHERE n.nspname = $1
  AND c.relname = $2
  AND a.attnum > 0
  AND NOT a.attisdropped
ORDER BY a.attnum
"""

_FKS_SQL = """
SELECT
    con.conname AS constraint_name,
    a.attname AS local_column,
    n2.nspname AS ref_schema,
    c2.relname AS ref_table
FROM pg_constraint con
JOIN pg_class c ON c.oid = con.conrelid
JOIN pg_namespace n ON n.oid = c.relnamespace
JOIN pg_attribute a ON a.attrelid = con.conrelid AND a.attnum = con.conkey[1]
JOIN pg_class c2 ON c2.oid = con.confrelid
JOIN pg_namespace n2 ON n2.oid = c2.relnamespace
WHERE con.contype = 'f'
  AND n.nspname = $1
  AND c.relname = $2
"""

_INDEXES_SQL = """
SELECT
    i.relname AS name,
    ix.indisunique AS is_unique,
    am.amname AS method
FROM pg_index ix
JOIN pg_class i ON i.oid = ix.indexrelid
JOIN pg_am am ON am.oid = i.relam
JOIN pg_class t ON t.oid = ix.indrelid
JOIN pg_namespace n ON n.oid = t.relnamespace
WHERE ix.indisprimary = false
  AND n.nspname = $1
  AND t.relname = $2
"""

_SEQUENCES_SQL = """
SELECT schemaname AS schema, sequencename AS name
FROM pg_sequences
WHERE schemaname NOT LIKE 'pg_%'
  AND schemaname <> ALL($1::text[])
ORDER BY schemaname, sequencename
"""

_VIEWS_SQL = """
SELECT table_schema AS schema, table_name AS name, view_definition
FROM information_schema.views
WHERE table_schema NOT LIKE 'pg_%'
  AND table_schema <> ALL($1::text[])
ORDER BY table_schema, table_name
"""

_FUNCTIONS_SQL = """
SELECT n.nspname AS schema, p.proname AS name,
       pg_get_functiondef(p.oid) AS definition
FROM pg_proc p
JOIN pg_namespace n ON n.oid = p.pronamespace
WHERE p.prokind = 'f'
  AND n.nspname NOT LIKE 'pg_%'
  AND n.nspname <> ALL($1::text[])
ORDER BY n.nspname, p.proname
"""


def _ddl_hash(text: str) -> str:
    import hashlib
    return hashlib.sha256(text.encode()).hexdigest()[:16]


async def introspect_db_state(conn: "asyncpg.Connection") -> "DbState":
    """Query pg_catalog and return a DbState describing the live database."""
    from pylon._core import DbState

    state = DbState()
    system = list(_SYSTEM_SCHEMAS)

    # Schemas
    schemas = await conn.fetch(_SCHEMAS_SQL, system)
    for row in schemas:
        state.add_schema(row["nspname"])

    # Enums (collect members per enum)
    enum_rows = await conn.fetch(_ENUMS_SQL, system)
    current_enum: tuple[str, str] | None = None
    members: list[str] = []
    for row in enum_rows:
        key = (row["schema"], row["name"])
        if key != current_enum:
            if current_enum is not None:
                state.add_enum(current_enum[0], current_enum[1], members)
            current_enum = key
            members = []
        members.append(row["member"])
    if current_enum is not None:
        state.add_enum(current_enum[0], current_enum[1], members)

    # Domains
    domain_rows = await conn.fetch(_DOMAINS_SQL, system)
    for row in domain_rows:
        state.add_domain(row["schema"], row["name"])

    # Tables + columns + FKs + indexes
    table_rows = await conn.fetch(_TABLES_SQL, system)
    for trow in table_rows:
        schema = trow["schema"]
        name = trow["name"]
        state.add_table(schema, name)

        col_rows = await conn.fetch(_COLUMNS_SQL, schema, name)
        for col in col_rows:
            state.add_column(
                schema,
                name,
                col["name"],
                col["pg_type"],
                col["nullable"],
                col["is_generated"],
                col["column_default"],
            )

        fk_rows = await conn.fetch(_FKS_SQL, schema, name)
        for fk in fk_rows:
            state.add_foreign_key(
                schema,
                name,
                fk["constraint_name"],
                fk["local_column"],
                fk["ref_schema"],
                fk["ref_table"],
            )

        idx_rows = await conn.fetch(_INDEXES_SQL, schema, name)
        for idx in idx_rows:
            state.add_index(
                schema,
                name,
                idx["name"],
                idx["is_unique"],
                idx["method"],
            )

    # Sequences
    seq_rows = await conn.fetch(_SEQUENCES_SQL, system)
    for row in seq_rows:
        state.add_sequence(row["schema"], row["name"])

    # Views
    view_rows = await conn.fetch(_VIEWS_SQL, system)
    for row in view_rows:
        state.add_view(row["schema"], row["name"], _ddl_hash(row["view_definition"]))

    # Functions
    fn_rows = await conn.fetch(_FUNCTIONS_SQL, system)
    for row in fn_rows:
        state.add_function(row["schema"], row["name"], _ddl_hash(row["definition"]))

    return state
