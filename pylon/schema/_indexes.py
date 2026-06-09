from __future__ import annotations

from . import _collector

# Characters that distinguish a PyQL expression from a bare field name.
_EXPR_CHARS = frozenset("(). +-*/<>=!|&")


def _is_expression(value: str) -> bool:
    return any(c in _EXPR_CHARS for c in value)


class Index:
    """Non-unique index declared as a class-body expression.

    Usage::

        Index('name')                           # single field
        Index(('last_name', 'first_name'))      # composite
        Index('str_lower(.name)')               # PyQL expression index
        Index('name', unless='.archived_at')    # partial index

    Uniqueness is always expressed via Exclusive, never via Index.
    """

    field: str | tuple[str, ...]
    unless: str | None
    is_expression: bool

    def __init__(
        self,
        field: str | tuple[str, ...],
        *,
        unless: str | None = None,
    ) -> None:
        self.field = tuple(field) if not isinstance(field, str) else field
        self.unless = unless
        self.is_expression = isinstance(self.field, str) and _is_expression(self.field)
        _collector.register(self)

    def __repr__(self) -> str:
        parts = [repr(self.field)]
        if self.unless is not None:
            parts.append(f"unless={self.unless!r}")
        return f"Index({', '.join(parts)})"
