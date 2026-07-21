from __future__ import annotations

import io
import os
import traceback
import unicodedata
import warnings

# ---------------------------------------------------------------------------
# Colour support
# ---------------------------------------------------------------------------


class _Color:
    BLUE = ""
    FAIL = ""
    ENDC = ""
    BOLD = ""


def _get_color() -> _Color:
    global _COLOR
    if _COLOR is None:
        _COLOR = _Color()
        try:
            use = {
                "default": lambda: os.isatty(2),
                "auto": lambda: os.isatty(2),
                "enabled": True,
                "disabled": False,
            }[os.getenv("PYLON_COLOR_OUTPUT", "default")]
            if callable(use):
                use = use()
        except (KeyError, Exception):
            use = False
        if use:
            _COLOR.BLUE = "\033[94m"
            _COLOR.FAIL = "\033[91m"
            _COLOR.ENDC = "\033[0m"
            _COLOR.BOLD = "\033[1m"
    return _COLOR


_COLOR: _Color | None = None

try:
    _SHOW_HINT = {
        "default": True,
        "enabled": True,
        "disabled": False,
    }[os.getenv("PYLON_ERROR_HINT", "default")]
except KeyError:
    warnings.warn(
        "PYLON_ERROR_HINT must be one of: default, enabled, disabled",
        stacklevel=1,
    )
    _SHOW_HINT = False


# ---------------------------------------------------------------------------
# Field constants (match the reference wire-protocol field ids)
# ---------------------------------------------------------------------------

_FIELD_HINT = 0x00_01
_FIELD_DETAILS = 0x00_02
_FIELD_CHARACTER_START = 0xFF_F9
_FIELD_CHARACTER_END = 0xFF_FA
_FIELD_LINE_START = 0xFF_F3
_FIELD_COLUMN_START = 0xFF_F4


# ---------------------------------------------------------------------------
# Base exception
# ---------------------------------------------------------------------------


class PylonError(Exception):
    """Root of all Pylon exceptions.

    Structured fields (hint, details, source position) are stored in
    ``_attrs`` and exposed as properties.  When a query string is attached
    and position information is available, ``__str__`` renders an annotated
    source snippet with a caret pointing at the offending token.
    """

    _query: str | None = None

    def __init__(self, *args: object, **kwargs: object) -> None:
        self._attrs: dict[int, str | bytes] = {}
        super().__init__(*args, **kwargs)

    # ------------------------------------------------------------------
    # Structured field helpers
    # ------------------------------------------------------------------

    def _read_str(self, key: int, default: str | None = None) -> str | None:
        val = self._attrs.get(key)
        if isinstance(val, bytes):
            return val.decode("utf-8")
        if val is not None:
            return str(val)
        return default

    @property
    def _position_start(self) -> int:
        return int(self._read_str(_FIELD_CHARACTER_START, "-1"))  # type: ignore[arg-type]

    @property
    def _position_end(self) -> int:
        return int(self._read_str(_FIELD_CHARACTER_END, "-1"))  # type: ignore[arg-type]

    @property
    def _line(self) -> int:
        return int(self._read_str(_FIELD_LINE_START, "-1"))  # type: ignore[arg-type]

    @property
    def _col(self) -> int:
        return int(self._read_str(_FIELD_COLUMN_START, "-1"))  # type: ignore[arg-type]

    @property
    def _hint(self) -> str | None:
        return self._read_str(_FIELD_HINT)

    @property
    def _details(self) -> str | None:
        return self._read_str(_FIELD_DETAILS)

    # ------------------------------------------------------------------
    # Factory — called by the Rust transpiler to attach position info
    # ------------------------------------------------------------------

    @classmethod
    def _from_transpiler(
        cls,
        message: str,
        *,
        query: str | None = None,
        position_start: int = -1,
        position_end: int = -1,
        line: int = -1,
        col: int = -1,
        hint: str | None = None,
        details: str | None = None,
    ) -> "PylonError":
        exc = cls(message)
        exc._query = query
        if position_start >= 0:
            exc._attrs[_FIELD_CHARACTER_START] = str(position_start)
        if position_end >= 0:
            exc._attrs[_FIELD_CHARACTER_END] = str(position_end)
        if line >= 0:
            exc._attrs[_FIELD_LINE_START] = str(line)
        if col >= 0:
            exc._attrs[_FIELD_COLUMN_START] = str(col)
        if hint:
            exc._attrs[_FIELD_HINT] = hint
        if details:
            exc._attrs[_FIELD_DETAILS] = details
        return exc

    # ------------------------------------------------------------------
    # Rendering
    # ------------------------------------------------------------------

    def __str__(self) -> str:
        msg = super().__str__()
        if _SHOW_HINT and self._query and self._position_start >= 0:
            try:
                return _format_error(
                    msg,
                    self._query,
                    self._position_start,
                    max(1, self._position_end - self._position_start),
                    self._line if self._line > 0 else "?",
                    self._col if self._col > 0 else "?",
                    self._hint or "error",
                    self._details,
                )
            except Exception:
                return "".join(
                    (
                        msg,
                        os.linesep,
                        os.linesep,
                        "During formatting of the above exception, "
                        "another exception occurred:",
                        os.linesep,
                        os.linesep,
                        traceback.format_exc(),
                    )
                )
        return msg


# ---------------------------------------------------------------------------
# Source snippet formatter
# ---------------------------------------------------------------------------


def _format_error(
    msg: str,
    query: str,
    start: int,
    offset: int,
    line: int | str,
    col: int | str,
    hint: str,
    details: str | None,
) -> str:
    c = _get_color()
    rv = io.StringIO()
    rv.write(f"{c.BOLD}{msg}{c.ENDC}{os.linesep}")

    lines = query.splitlines(keepends=True)
    num_len = len(str(len(lines)))
    rv.write(f"{c.BLUE}{'':>{num_len}} ┌─{c.ENDC} query:{line}:{col}{os.linesep}")
    rv.write(f"{c.BLUE}{'':>{num_len}} │ {c.ENDC}{os.linesep}")

    for num, ln in enumerate(lines):
        length = len(ln)
        ln = ln.rstrip()

        if start >= length:
            start -= length
            continue

        if start >= 0:
            first_half = repr(ln[:start])[1:-1]
            ln = ln[start:]
            length -= start
            rv.write(f"{c.BLUE}{num + 1:>{num_len}} │   {c.ENDC}{first_half}")
            start = _unicode_width(first_half)
        else:
            rv.write(f"{c.BLUE}{num + 1:>{num_len}} │ {c.FAIL}│ {c.ENDC}")

        if offset > length:
            ln = repr(ln)[1:-1]
            rv.write(f"{c.FAIL}{ln}{c.ENDC}{os.linesep}")
            if start >= 0:
                rv.write(
                    f"{c.BLUE}{'':>{num_len}} │ "
                    f"{c.FAIL}╭─{'─' * start}^{c.ENDC}{os.linesep}"
                )
            offset -= length
            start = -1
        else:
            first_half = repr(ln[:offset])[1:-1]
            rest = repr(ln[offset:])[1:-1]
            rv.write(f"{c.FAIL}{first_half}{c.ENDC}{rest}{os.linesep}")
            size = _unicode_width(first_half)
            if start >= 0:
                rv.write(
                    f"{c.BLUE}{'':>{num_len}} │   {' ' * start}"
                    f"{c.FAIL}{'^' * size} {hint}{c.ENDC}"
                )
            else:
                rv.write(
                    f"{c.BLUE}{'':>{num_len}} │ "
                    f"{c.FAIL}╰─{'─' * (size - 1)}^ {hint}{c.ENDC}"
                )
            break

    if details:
        rv.write(f"{os.linesep}Details: {details}")

    return rv.getvalue()


def _unicode_width(text: str) -> int:
    return sum(
        0
        if unicodedata.category(c) in ("Mn", "Cf")
        else 2
        if unicodedata.east_asian_width(c) == "W"
        else 1
        for c in text
    )


# ---------------------------------------------------------------------------
# Client / configuration
# ---------------------------------------------------------------------------


class ClientError(PylonError):
    """Client-side configuration or usage mistake."""


class NoActiveDatabaseError(ClientError):
    """No database connection has been configured or the pool is closed."""


class InterfaceError(ClientError):
    """Misuse of the client API (wrong argument types, calling order, etc.)."""


# ---------------------------------------------------------------------------
# Connection
# ---------------------------------------------------------------------------


class ConnectionError(PylonError):  # noqa: A001
    """Base for all connection-level failures."""


class ConnectionFailedError(ConnectionError):
    """Initial connection to PostgreSQL could not be established."""


class ConnectionTimeoutError(ConnectionError):
    """Acquiring a connection from the pool timed out."""


class ClientConnectionClosedError(ConnectionError):
    """Operation attempted on a connection that has already been closed."""


# ---------------------------------------------------------------------------
# Transaction
# ---------------------------------------------------------------------------


class TransactionError(PylonError):
    """Base for transaction-related failures."""


class TransactionSerializationError(TransactionError):
    """Transaction could not be serialised."""


class TransactionDeadlockError(TransactionError):
    """Deadlock detected; transaction was rolled back."""


# ---------------------------------------------------------------------------
# Query / execution
# ---------------------------------------------------------------------------


class QueryError(PylonError):
    """Base for query compilation and execution failures."""


class InvalidQueryError(QueryError):
    """PyQL query is syntactically or semantically invalid."""


class UnknownParameterError(QueryError):
    """A query parameter was supplied that is not declared in the query."""


class MissingParameterError(QueryError):
    """A required query parameter was not supplied."""


class InvalidParameterTypeError(QueryError):
    """A parameter value does not match the declared type."""


# ---------------------------------------------------------------------------
# Result shape
# ---------------------------------------------------------------------------


class ResultCardinalityError(QueryError):
    """Result cardinality violated the expected constraint."""


class NoDataError(ResultCardinalityError):
    """query_required_single returned an empty result set."""


# ---------------------------------------------------------------------------
# Schema
# ---------------------------------------------------------------------------


class SchemaError(PylonError):
    """Base for schema-related failures."""


class UnknownTypeError(SchemaError):
    """A type referenced in a query or schema definition does not exist."""


class UnknownLinkError(SchemaError):
    """A link or property referenced in a query does not exist on the type."""


class ConstraintViolationError(SchemaError):
    """A database constraint was violated."""


# ---------------------------------------------------------------------------
# Migration
# ---------------------------------------------------------------------------


class MigrationError(PylonError):
    """Base for migration-related failures."""


class MigrationConflictError(MigrationError):
    """The requested migration conflicts with the current schema state."""


# ---------------------------------------------------------------------------
# Internal
# ---------------------------------------------------------------------------


class InternalServerError(PylonError):
    """Unexpected internal error — indicates a bug in Pylon or the transpiler."""
