from __future__ import annotations

from typing import Literal

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


Metric = Literal["cosine", "euclidean", "inner_product"]


class VectorIndex:
    """Deferred embedding index.  Writes enqueue jobs; a background worker
    generates embeddings and writes them back into the vector column.

    Usage (default / bare index)::

        @pylon.type
        class Product(pylon.BaseObject):
            name: Property[str]
            description: Property[str]
            pylon.VectorIndex(fields=['name', 'description'], model='mistral-embed')

    Named index (assigned to a class attribute; picks up its name via ``__set_name__``)::

        @pylon.type
        class Product(pylon.BaseObject):
            name: Property[str]
            summary: Property[str]
            summary_index = pylon.VectorIndex(fields=['summary'], model='mistral-embed')
    """

    index_name: str | None
    fields: list[str]
    model: str
    metric: Metric
    dimensions: int

    def __init__(
        self,
        fields: list[str],
        model: str,
        *,
        metric: Metric = "cosine",
        dimensions: int = 1024,
    ) -> None:
        self.index_name = None
        self.fields = list(fields)
        self.model = model
        self.metric = metric
        self.dimensions = dimensions
        _collector.register(self)

    def __set_name__(self, owner: type, name: str) -> None:
        self.index_name = name

    def __repr__(self) -> str:
        parts = [f"fields={self.fields!r}", f"model={self.model!r}"]
        if self.index_name is not None:
            parts.append(f"index_name={self.index_name!r}")
        return f"VectorIndex({', '.join(parts)})"
