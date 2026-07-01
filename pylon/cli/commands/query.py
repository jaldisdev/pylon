import asyncio
import dataclasses
import re
import shutil
from pathlib import Path
from typing import Any

import click
from prompt_toolkit import PromptSession
from prompt_toolkit.history import FileHistory
from prompt_toolkit.key_binding import KeyBindings
from prompt_toolkit.keys import Keys

import pylon
from pylon.client import create_async_client

from ..banner import print_banner

# Regex that matches `set global name := expression` (case-insensitive SET/GLOBAL)
_SET_GLOBAL_RE = re.compile(
    r"^set\s+global\s+([\w:]+)\s*:=\s*(.+)$",
    re.IGNORECASE | re.DOTALL,
)

# --- colours ------------------------------------------------------------------

_INFO_COLOR = "\x1b[38;2;136;120;168m"  # #8878A8
_BOLD_RED = "\x1b[1;31m"
_RED = "\x1b[38;5;208m"  # type names
_YELLOW = "\x1b[38;5;178m"  # keys and UUIDs
_GREEN = "\x1b[38;5;107m"  # string values
_BLUE = "\x1b[38;2;96;135;176m"  # outer braces (#6087B0)
_BOLD_WHITE = "\x1b[1;37m"
_DIM = "\x1b[2m"
_RESET = "\x1b[0m"


# --- help text ----------------------------------------------------------------

_HELP_TEXT = f"""\
{_INFO_COLOR}Type PyQL statements ending with ; to execute them.

  \\help    show this help
  \\quit    exit the session (also Ctrl-D){_RESET}"""


# --- key bindings -------------------------------------------------------------


def _make_bindings() -> KeyBindings:
    kb = KeyBindings()

    @kb.add(Keys.Enter)
    def _(event) -> None:
        buf = event.current_buffer
        text = buf.text.strip()
        at_end = buf.cursor_position == len(buf.text)
        if at_end and (text.endswith(";") or text.startswith("\\")):
            buf.validate_and_handle()
        else:
            buf.insert_text("\n")

    return kb


# --- repl ---------------------------------------------------------------------


def _history_path(project_name: str | None) -> Path:
    history_dir = Path.home() / ".pylon" / "history"
    history_dir.mkdir(parents=True, exist_ok=True)
    name = project_name or "default"
    return history_dir / name


def repl(*, as_json: bool = False, project_name: str | None = None) -> None:
    """Start an interactive PyQL session.

    Statements are terminated by a semicolon. Type \\help for help,
    \\quit or Ctrl-D to exit.
    """
    try:
        pylon.finalize()
    except Exception as e:
        click.echo(f"{_BOLD_RED}error:{_RESET} could not load schema: {e}", err=True)
        return

    print_banner(info_line="Type \\help for help, \\quit to quit.")

    asyncio.run(_async_repl(as_json=as_json, project_name=project_name))


async def _async_repl(*, as_json: bool, project_name: str | None) -> None:
    session: PromptSession[str] = PromptSession(
        multiline=True,
        key_bindings=_make_bindings(),
        prompt_continuation="",
        history=FileHistory(str(_history_path(project_name))),
    )

    # Session globals: qualified_name → value (e.g. "default::current_user_id" → uuid)
    _session_globals: dict[str, Any] = {}

    async with create_async_client() as client:
        while True:
            try:
                text = await session.prompt_async("pylon> ")
            except KeyboardInterrupt:
                click.echo()
                click.echo(f"{_INFO_COLOR}Use \\quit or Ctrl-D to exit.{_RESET}")
                continue
            except EOFError:
                click.echo()
                break

            stripped = text.strip()

            if stripped == r"\quit":
                break
            elif stripped == r"\help":
                click.echo(_HELP_TEXT)
                continue

            pyql = stripped.rstrip(";").strip()
            if not pyql:
                continue

            # Intercept `set global name := expression`
            m = _SET_GLOBAL_RE.match(pyql)
            if m:
                await _handle_set_global(client, m.group(1), m.group(2).strip(), _session_globals)
                continue

            await _execute(client, pyql, as_json=as_json, globals_=_session_globals)


async def _handle_set_global(
    client: Any,
    name: str,
    expression: str,
    session_globals: dict[str, Any],
) -> None:
    """Evaluate `expression` via `select <expr>` and store in session_globals."""
    from pylon.query import _get_schema

    # Resolve unqualified name to module::name using schema globals
    try:
        schema = _get_schema()
        gs = {g["name"]: g["qualified_name"] for g in schema.globals()}
        qualified = gs.get(name) or (name if "::" in name else None)
        if qualified is None:
            click.echo(f"{_BOLD_RED}error:{_RESET} unknown global {name!r}")
            return
    except Exception:
        qualified = name if "::" in name else f"default::{name}"

    try:
        results = await client.query(f"select {expression}")
    except Exception as e:
        click.echo(f"{_BOLD_RED}error:{_RESET} {_translate_pg_types(str(e))}")
        return

    value = results[0] if results else None
    session_globals[qualified] = value
    click.echo(f"{_INFO_COLOR}OK — {qualified} = {_value(value)}{_RESET}")


async def _execute(client, pyql: str, *, as_json: bool, repl: bool = True, globals_: dict[str, Any] | None = None) -> None:
    """Transpile and execute a single PyQL statement, printing the result."""
    from pylon.client import _transpile, _hydrate

    if as_json:
        try:
            click.echo(await client.query_json(pyql))
        except Exception as e:
            click.echo(f"{_BOLD_RED}error:{_RESET} {_translate_pg_types(str(e))}")
        return

    try:
        import asyncpg as _asyncpg
        from pylon.client import _fmt_pg_error
        sql, params, compiled = _transpile(pyql, {}, globals_)
        for w in compiled.warnings():
            click.echo(f"{_YELLOW}warning:{_RESET} {w}", err=True)
        try:
            async with client._require_pool().acquire() as conn:
                records = list(await conn.fetch(sql, *params))
        except _asyncpg.PostgresError as exc:
            raise _fmt_pg_error(exc) from exc
        results = _hydrate(records, compiled)
    except Exception as e:
        click.echo(f"{_BOLD_RED}error:{_RESET} {_translate_pg_types(str(e))}")
        return

    shape = compiled.shape
    shape_kind = shape.get("kind", "object")

    # Free scalar: plain values like int, str, bool, and array literals
    if shape_kind in ("scalar", "raw_scalar"):
        items = [_value(obj) for obj in results]
        click.echo(_format_set(items) if repl else "\n".join(items))
        return

    # JSON scalar: value decoded from jsonb, display as Json("...")
    if shape_kind == "json_scalar":
        import json as _json
        def _json_display(v: object) -> str:
            escaped = _json.dumps(v).replace('"', '\\"')
            return f"Json({_brace(chr(34))}{escaped}{_brace(chr(34))})"
        items = [_json_display(obj) for obj in results]
        click.echo(_format_set(items) if repl else "\n".join(items))
        return

    # Anonymous tuple: (1, 'hello')
    if shape_kind == "tuple":
        items = [_format_tuple(obj) for obj in results]
        click.echo(_format_set(items) if repl else "\n".join(items))
        return

    # Named tuple: (x := val, y := val)
    if shape_kind == "named_tuple":
        items = [_format_named_tuple(obj) for obj in results]
        click.echo(_format_set(items) if repl else "\n".join(items))
        return

    # Group result: free objects with key / grouping / elements
    if shape_kind == "group":
        max_width = shutil.get_terminal_size((100, 24)).columns
        depth = 1 if repl else 0
        items = [_format_group_row(obj, depth, max_width) for obj in results]
        click.echo(_format_set(items) if repl else "\n".join(items))
        return

    # Schema object or free object
    # Auto-injected __type__ at position 0 is excluded; explicit __type__ (pos > 0) is included.
    selected = {
        f["name"] for f in shape.get("fields", [])
        if not (f["name"] == "__type__" and f["position"] == 0)
    }
    type_name = shape.get("type_name") or ""

    display = []
    for obj in results:
        if dataclasses.is_dataclass(obj) and not isinstance(obj, type):
            d = {
                k: v
                for k, v in vars(obj).items()
                if k in selected
            }
            # __display_type__ is the internal sentinel used as the type label in _format_results.
            # Keeping it separate from __type__ lets an explicit `__type__` field show in the output.
            d["__display_type__"] = vars(obj).get("__pylon_type__") or type_name or type(obj).__name__
        elif isinstance(obj, dict):
            d = {k: v for k, v in obj.items() if k in selected}
            d["__display_type__"] = type_name
        else:
            d = {"__display_type__": type(obj).__name__, "value": str(obj)}
        display.append(d)
    click.echo(_format_results(display, wrap=repl))


# --- one-shot query command ---------------------------------------------------


@click.command("query")
@click.argument("pyql_query")
@click.option("--json", "as_json", is_flag=True, default=False,
              help="Return results as JSON.")
def query_cmd(pyql_query: str, as_json: bool) -> None:
    """Execute a single PyQL query and print the result.

    Example:

        pylon query "select Person { name, age };"
    """
    try:
        pylon.finalize()
    except Exception as e:
        click.echo(f"{_BOLD_RED}error:{_RESET} could not load schema: {e}", err=True)
        raise SystemExit(1)

    pyql = pyql_query.rstrip(";").strip()

    async def run() -> None:
        async with create_async_client() as client:
            await _execute(client, pyql, as_json=as_json, repl=False)

    asyncio.run(run())


# --- result formatting --------------------------------------------------------


_PG_TO_PYQL = {
    "timestamp with time zone": "datetime",
    "time without time zone": "time",
    "double precision": "float64",
    "character varying": "str",
    "smallint": "int16",
    "integer": "int32",
    "bigint": "int64",
    "boolean": "bool",
    "numeric": "decimal",
    "bytea": "bytes",
    "jsonb": "json",
    "text": "str",
    "real": "float32",
}

_PG_TYPE_RE = re.compile(
    r"\b(" + "|".join(re.escape(k) for k in sorted(_PG_TO_PYQL, key=len, reverse=True)) + r")\b"
)


def _translate_pg_types(msg: str) -> str:
    return _PG_TYPE_RE.sub(lambda m: _PG_TO_PYQL[m.group()], msg)


def _type(s: str) -> str:
    return f"{_RED}{s}{_RESET}"


def _key(s: str) -> str:
    return f"{_YELLOW}{s}{_RESET}"


def _brace(s: str) -> str:
    return f"{_BLUE}{s}{_RESET}"


def _value(v: object) -> str:
    import enum as _enum_mod
    if isinstance(v, _enum_mod.Enum):
        return f"{_RED}{v.name}{_RESET}"
    if isinstance(v, str):
        if _is_uuid(v):
            return f"{_YELLOW}{v}{_RESET}"
        return f"{_GREEN}'{v}'{_RESET}"
    if isinstance(v, bool):
        return f"{_YELLOW}{str(v).lower()}{_RESET}"
    if v is None:
        return f"{_brace('{')}{_brace('}')}"
    if isinstance(v, list):
        from pylon.datatypes import PylonSet
        inner = ", ".join(_value(item) for item in v)
        if isinstance(v, PylonSet):
            return f"{_brace('{')}{inner}{_brace('}')}"
        return f"{_brace('[')}{inner}{_brace(']')}"
    if dataclasses.is_dataclass(v) and not isinstance(v, type):
        from pylon.schema._named_tuples import NamedTuple as PylonNamedTuple
        if isinstance(v, PylonNamedTuple):
            return _format_named_tuple(v)
        qname = vars(v).get("__pylon_type__") or type(v).__name__
        pairs = ", ".join(
            f"{_key(k)}: {_value(val)}"
            for k, val in vars(v).items()
            if k != "__pylon_type__"
        )
        return f"{_type(qname)} {_brace('{')}{pairs}{_brace('}')}"
    if isinstance(v, dict):
        pairs = ", ".join(f"{_key(k)}: {_value(val)}" for k, val in v.items())
        return f"{_brace('{')}{pairs}{_brace('}')}"
    return str(v)


def _format_named_tuple(v: object) -> str:
    """Display a named tuple as default::Point (x := val, y := val)."""
    if v is None:
        return f"{_brace('{')}{_brace('}')}"
    if dataclasses.is_dataclass(v) and not isinstance(v, type):
        mod = getattr(type(v), "__pylon_module__", None)
        qname = f"{mod}::{type(v).__name__}" if mod else type(v).__name__
        fields = {k: val for k, val in vars(v).items() if not k.startswith("__pylon_")}
    elif isinstance(v, dict):
        qname = ""
        fields = v
    else:
        return str(v)
    prefix = f"{_type(qname)} " if qname else ""
    pairs = ", ".join(f"{_key(k)} := {_value(val)}" for k, val in fields.items())
    return f"{prefix}({pairs})"


def _is_uuid(s: str) -> bool:
    return bool(
        re.fullmatch(
            r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}",
            s,
            re.IGNORECASE,
        )
    )


_ANSI_RE = re.compile(r'\x1b\[[0-9;]*m')


def _visual_len(s: str) -> int:
    return len(_ANSI_RE.sub('', s))


def _pformat_value(v: object, depth: int, max_width: int) -> str:
    """Pretty-print a value, recursively expanding objects that are too wide."""
    if dataclasses.is_dataclass(v) and not isinstance(v, type):
        qname = vars(v).get("__pylon_type__") or type(v).__name__
        fields = {k: val for k, val in vars(v).items() if k != "__pylon_type__"}
        return _pformat_object(qname, fields, depth, max_width)
    if isinstance(v, dict):
        return _pformat_object("", v, depth, max_width)
    from pylon.datatypes import PylonSet
    if isinstance(v, PylonSet):
        elem_strs = [_pformat_value(e, depth + 1, max_width) for e in v]
        return _format_set(elem_strs)
    return _value(v)


def _pformat_object(type_name: str, fields: dict, depth: int, max_width: int) -> str:
    """Format an object as single-line if it fits, otherwise expand to multi-line."""
    prefix = f"{_type(type_name)} " if type_name else ""
    pairs_compact = ", ".join(f"{_key(k)}: {_value(v)}" for k, v in fields.items())
    compact = f"{prefix}{_brace('{')}{pairs_compact}{_brace('}')}"
    if _visual_len(compact) <= max_width - depth * 2:
        return compact
    indent = "  " * (depth + 1)
    closing = "  " * depth
    field_strs = [f"{_key(k)}: {_pformat_value(v, depth + 1, max_width)}" for k, v in fields.items()]
    inner = f",\n{indent}".join(field_strs)
    return f"{prefix}{_brace('{')}\n{indent}{inner}\n{closing}{_brace('}')}"


def _format_group_row(obj: dict, depth: int, max_width: int) -> str:
    """Format a GROUP result row: key as free object, grouping as set, elements as set."""
    key_pairs = ", ".join(f"{_key(k)}: {_value(v)}" for k, v in obj.get("key", {}).items())
    key_str = f"{_brace('{')}{key_pairs}{_brace('}')}"

    grouping_items = [f"{_GREEN}'{name}'{_RESET}" for name in sorted(obj.get("grouping", []))]
    grouping_str = _format_set(grouping_items)

    elements = obj.get("elements", [])
    if dataclasses and elements and dataclasses.is_dataclass(elements[0]):
        elem_strs = [
            _pformat_object(
                vars(e).get("__pylon_type__") or type(e).__name__,
                {k: v for k, v in vars(e).items() if k != "__pylon_type__"},
                depth + 2,
                max_width,
            )
            for e in elements
        ]
    else:
        elem_strs = [_pformat_value(e, depth + 2, max_width) for e in elements]
    elements_str = _format_set(elem_strs)

    fields = {
        "key": key_str,
        "grouping": grouping_str,
        "elements": elements_str,
    }
    indent = "  " * (depth + 1)
    closing = "  " * depth
    inner = f",\n{indent}".join(f"{_key(k)}: {v}" for k, v in fields.items())
    return f"{_brace('{')}\n{indent}{inner}\n{closing}{_brace('}')}"


def _format_tuple(t: tuple) -> str:
    return "(" + ", ".join(_value(v) for v in t) + ")"


def _format_set(items: list[str]) -> str:
    """Render a set of pre-formatted value strings in the upstream engine style."""
    if not items:
        return _brace("{}")
    if len(items) == 1:
        return f"{_brace('{')}{items[0]}{_brace('}')}"
    inner = ",\n  ".join(items)
    return f"{_brace('{')}\n  {inner}\n{_brace('}')}"


def _format_results(results: list[dict], *, wrap: bool = True) -> str:
    """Render a list of result objects in conventional coloured output."""
    max_width = shutil.get_terminal_size((100, 24)).columns
    if not results:
        return _brace("{}") if wrap else ""

    depth = 1 if wrap else 0
    formatted = [
        _pformat_object(obj.pop("__display_type__", ""), obj, depth, max_width)
        for obj in results
    ]

    if not wrap:
        return "\n".join(formatted)

    lines = [_brace("{")]
    for item in formatted:
        lines.append(f"  {item},")
    lines.append(_brace("}"))
    return "\n".join(lines)


def format_error(
    error_type: str, message: str, source: str, line: int, col: int
) -> str:
    """Render a query error in conventional coloured output.

    Args:
        error_type: Exception class name, e.g. ``InvalidReferenceError``.
        message:    Human-readable error message.
        source:     The original PyQL source string (single line).
        line:       1-based line number where the error occurred.
        col:        1-based column number where the error starts.
    """
    token_match = re.match(r"[\w:]+", source[col - 1 :])
    span = len(token_match.group()) if token_match else 1
    carets = "^" * span

    line_num = str(line)
    gutter = len(line_num)

    lines = [
        f"{_BOLD_RED}error: {error_type}:{_RESET} {_BOLD_WHITE}{message}{_RESET}",
        f"  {_DIM}\u250c\u2500 <query>:{line}:{col}{_RESET}",
        f"",
        f"  {_DIM}{line_num}{_RESET}  \u2502  {source}",
        f"  {' ' * gutter}     {_BOLD_RED}{carets} error{_RESET}",
        f"",
    ]
    return "\n".join(lines)
