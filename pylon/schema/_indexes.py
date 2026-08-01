#
# This source file is part of the Pylon open source project.
#
# Copyright (c) 2026 Jaldis B.V.
#
# Licensed under the MIT OR Apache-2.0 license (the "License");
# you may not use this file except in compliance with the License.
# You may obtain a copy of the License at
#
#     https://opensource.org/licenses/MIT
#     https://www.apache.org/licenses/LICENSE-2.0
#
# Unless required by applicable law or agreed to in writing, software
# distributed under the License is distributed on an "AS IS" BASIS,
# WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
# See the License for the specific language governing permissions and
# limitations under the License.
#

from __future__ import annotations

from enum import Enum
from typing import Literal

from . import _collector

# Characters that distinguish a PyQL expression from a bare pointer name.
_EXPR_CHARS = frozenset("(). +-*/<>=!|&")


def _is_expression(value: str) -> bool:
    return any(c in _EXPR_CHARS for c in value)


class Index:
    """Non-unique index declared as a class-body expression.

    Usage::

        Index('name')                           # single pointer
        Index(('last_name', 'first_name'))      # composite
        Index('str_lower(.name)')               # PyQL expression index
        Index('name', unless='.archived_at')    # partial index

    Uniqueness is always expressed via Exclusive, never via Index.
    """

    pointer: str | tuple[str, ...]
    unless: str | None
    is_expression: bool

    def __init__(
        self,
        pointer: str | tuple[str, ...],
        *,
        unless: str | None = None,
    ) -> None:
        self.pointer = tuple(pointer) if not isinstance(pointer, str) else pointer
        self.unless = unless
        self.is_expression = isinstance(self.pointer, str) and _is_expression(self.pointer)
        _collector.register(self)

    def __repr__(self) -> str:
        parts = [repr(self.pointer)]
        if self.unless is not None:
            parts.append(f"unless={self.unless!r}")
        return f"Index({', '.join(parts)})"


Metric = Literal["cosine", "euclidean", "inner_product"]


class VectorPointer:
    """A pointer included in a VectorIndex, referenced as a lazy annotation string.

    Use the ``'TypeName.pointer_name'`` form — the type prefix is validated
    during ``pylon.finalize()`` against the enclosing type::

        pylon.VectorPointer('Product.name')
        pylon.VectorPointer('Product.description')
    """

    def __init__(self, ref: str) -> None:
        self.ref = ref

    def __repr__(self) -> str:
        return f"VectorPointer({self.ref!r})"


class VectorIndex:
    """Deferred embedding index.  Writes enqueue jobs; a background worker
    generates embeddings and writes them back into the vector column.

    Usage (default / bare index)::

        @pylon.type
        class Product(pylon.BaseObject):
            name: Property[str]
            description: Property[str]
            pylon.VectorIndex(
                pointers=[pylon.VectorPointer('Product.name'), pylon.VectorPointer('Product.description')],
                model='mistral-embed',
            )

    Named index (assigned to a class attribute; picks up its name via ``__set_name__``)::

        @pylon.type
        class Product(pylon.BaseObject):
            name: Property[str]
            summary_index = pylon.VectorIndex(
                pointers=[pylon.VectorPointer('Product.name')],
                model='mistral-embed',
            )
    """

    index_name: str | None
    _vector_pointers: list[VectorPointer]
    model: str
    metric: Metric
    dimensions: int

    def __init__(
        self,
        pointers: list[VectorPointer],
        model: str,
        *,
        metric: Metric = "cosine",
        dimensions: int = 1024,
    ) -> None:
        self.index_name = None
        self._vector_pointers = list(pointers)
        self.model = model
        self.metric = metric
        self.dimensions = dimensions
        _collector.register(self)

    def __set_name__(self, owner: type, name: str) -> None:
        self.index_name = name

    def __repr__(self) -> str:
        parts = [f"pointers={self._vector_pointers!r}", f"model={self.model!r}"]
        if self.index_name is not None:
            parts.append(f"index_name={self.index_name!r}")
        return f"VectorIndex({', '.join(parts)})"


# ── SearchIndex ────────────────────────────────────────────────────────────────


class SearchBackend(Enum):
    Postgres = "Postgres"
    OpenSearch = "OpenSearch"
    Meilisearch = "Meilisearch"


class SearchWeight(Enum):
    A = "A"
    B = "B"
    C = "C"
    D = "D"


class SearchMode(Enum):
    BestFields = "BestFields"
    Phrase = "Phrase"
    PhrasePrefix = "PhrasePrefix"


class SearchPointer:
    """A pointer included in a SearchIndex, referenced as a lazy annotation string.

    Use the ``'TypeName.pointer_name'`` form — the type prefix is validated
    during ``pylon.finalize()`` against the enclosing type::

        pylon.SearchPointer('Product.name', weight_category=pylon.SearchWeight.A)
        pylon.SearchPointer('Product.description', weight_category=pylon.SearchWeight.B)
    """

    def __init__(self, ref: str, *, weight_category: SearchWeight = SearchWeight.A) -> None:
        self.ref = ref
        self.weight_category = weight_category

    def __repr__(self) -> str:
        return f"SearchPointer({self.ref!r}, weight_category={self.weight_category!r})"


class SearchIndex:
    """Full-text search index.

    Supports two backends behind one query interface — Postgres (tsvector
    generated column + GIN index, synchronous write path) and OpenSearch
    (deferred, outbox-driven, asynchronous).

    Usage (default / bare index)::

        @pylon.type
        class Product:
            name: pylon.Str
            description: pylon.Property[pylon.Str] | None
            pylon.SearchIndex(
                backend=pylon.SearchBackend.Postgres,
                pointers=[
                    pylon.SearchPointer('Product.name', weight_category=pylon.SearchWeight.A),
                    pylon.SearchPointer('Product.description', weight_category=pylon.SearchWeight.B),
                ],
            )

    Named index::

        @pylon.type
        class Product:
            name: pylon.Str
            typeahead = pylon.SearchIndex(
                backend=pylon.SearchBackend.Postgres,
                pointers=[pylon.SearchPointer('Product.name', weight_category=pylon.SearchWeight.A)],
            )
    """

    index_name: str | None
    backend: SearchBackend
    _search_pointers: list[SearchPointer]

    def __init__(self, backend: SearchBackend, pointers: list[SearchPointer]) -> None:
        self.index_name = None
        self.backend = backend
        self._search_pointers = list(pointers)
        _collector.register(self)

    def __set_name__(self, owner: type, name: str) -> None:
        self.index_name = name

    def __repr__(self) -> str:
        parts = [f"backend={self.backend!r}", f"pointers={self._search_pointers!r}"]
        if self.index_name is not None:
            parts.append(f"index_name={self.index_name!r}")
        return f"SearchIndex({', '.join(parts)})"
