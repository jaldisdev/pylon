import asyncio
import dataclasses
import re

import click
from prompt_toolkit import PromptSession
from prompt_toolkit.key_binding import KeyBindings
from prompt_toolkit.keys import Keys

import pylon
from pylon.client import create_async_client

from ..banner import print_banner

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
        text = event.current_buffer.text.strip()
        if text.endswith(";") or text.startswith("\\"):
            event.current_buffer.validate_and_handle()
        else:
            event.current_buffer.insert_text("\n")

    return kb


# --- repl ---------------------------------------------------------------------


def repl(*, as_json: bool = False) -> None:
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

    asyncio.run(_async_repl(as_json=as_json))


async def _async_repl(*, as_json: bool) -> None:
    session: PromptSession[str] = PromptSession(
        multiline=True,
        key_bindings=_make_bindings(),
        prompt_continuation="",
    )

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
            if pyql:
                await _execute(client, pyql, as_json=as_json)


async def _execute(client, pyql: str, *, as_json: bool) -> None:
    """Transpile and execute a single PyQL statement, printing the result."""
    from pylon.query import compile as pyql_compile

    if as_json:
        try:
            click.echo(await client.query_json(pyql))
        except Exception as e:
            click.echo(f"{_BOLD_RED}error:{_RESET} {_translate_pg_types(str(e))}")
        return

    try:
        compiled = pyql_compile(pyql)
        results = await client.query(pyql)
    except Exception as e:
        click.echo(f"{_BOLD_RED}error:{_RESET} {_translate_pg_types(str(e))}")
        return

    shape = compiled.shape
    shape_kind = shape.get("kind", "object")

    # Free scalar: plain values like int, str, bool, and array literals
    if shape_kind in ("scalar", "raw_scalar"):
        click.echo(_format_set([_value(obj) for obj in results]))
        return

    # JSON scalar: value decoded from jsonb, display as Json("...")
    if shape_kind == "json_scalar":
        import json as _json
        def _json_display(v: object) -> str:
            escaped = _json.dumps(v).replace('"', '\\"')
            return f"Json({_brace(chr(34))}{escaped}{_brace(chr(34))})"
        click.echo(_format_set([_json_display(obj) for obj in results]))
        return

    # Anonymous tuple: (1, 'hello')
    if shape_kind == "tuple":
        click.echo(_format_set([_format_tuple(obj) for obj in results]))
        return

    # Schema object or free object
    selected = {f["name"] for f in shape.get("fields", []) if f["name"] != "__type__"}
    type_name = shape.get("type_name") or ""

    display = []
    for obj in results:
        if dataclasses.is_dataclass(obj) and not isinstance(obj, type):
            d = {
                f.name: getattr(obj, f.name)
                for f in dataclasses.fields(obj)
                if f.name in selected
            }
            d["__type__"] = type_name or vars(obj).get("__pylon_type__") or type(obj).__name__
        elif isinstance(obj, dict):
            d = {k: v for k, v in obj.items() if k in selected}
            d["__type__"] = type_name  # empty string for free objects
        else:
            d = {"__type__": type(obj).__name__, "value": str(obj)}
        display.append(d)
    click.echo(_format_results(display))


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
    if isinstance(v, str):
        if _is_uuid(v):
            return f"{_YELLOW}{v}{_RESET}"
        return f"{_GREEN}'{v}'{_RESET}"
    if isinstance(v, bool):
        return f"{_YELLOW}{str(v).lower()}{_RESET}"
    if v is None:
        return f"{_brace('{')}{_brace('}')}"
    if isinstance(v, list):
        inner = ", ".join(_value(item) for item in v)
        return f"{_brace('{')}{inner}{_brace('}')}"
    if dataclasses.is_dataclass(v) and not isinstance(v, type):
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


def _is_uuid(s: str) -> bool:
    return bool(
        re.fullmatch(
            r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}",
            s,
            re.IGNORECASE,
        )
    )


def _format_object(type_name: str, fields: dict) -> str:
    pairs = ", ".join(f"{_key(k)}: {_value(v)}" for k, v in fields.items())
    label = type_name or "Object"
    return f"{_type(label)} {_brace('{')}{pairs}{_brace('}')}"


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


def _format_results(results: list[dict]) -> str:
    """Render a list of result objects in conventional coloured output."""
    if not results:
        return _brace("{}")

    lines = [_brace("{")]
    for obj in results:
        type_name = obj.pop("__type__", "")
        lines.append(f"  {_format_object(type_name, obj)},")
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
