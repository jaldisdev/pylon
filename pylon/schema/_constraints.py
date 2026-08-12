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

from . import _collector


class _PointerConstraint:
    """Marker base for all constraint objects."""


# ── Sentinels ──────────────────────────────────────────────────────────────────


class _NowType:
    _inst: _NowType | None = None

    def __new__(cls) -> _NowType:
        if cls._inst is None:
            cls._inst = super().__new__(cls)
        return cls._inst

    def __repr__(self) -> str:
        return 'Now'


Now = _NowType()


class _SequenceNextType:
    _inst: _SequenceNextType | None = None

    def __new__(cls) -> _SequenceNextType:
        if cls._inst is None:
            cls._inst = super().__new__(cls)
        return cls._inst

    def __repr__(self) -> str:
        return 'SequenceNext'


SequenceNext = _SequenceNextType()


class Default(_PointerConstraint):
    """Server-side default. Only Default(Now) is supported in the initial spec.

    At the Python level the pointer is optional and defaults to None; the DDL
    generator emits DEFAULT NOW() (or the appropriate expression) in the column
    definition.
    """

    def __init__(self, sentinel: object) -> None:
        self.sentinel = sentinel

    def __repr__(self) -> str:
        return f'Default({self.sentinel!r})'


# ── Pointer-level constraints ───────────────────────────────────────────────────


class OneOf(_PointerConstraint):
    """Restricts a property to an explicit set of allowed values."""

    def __init__(self, *values: object) -> None:
        self.values = values

    def __repr__(self) -> str:
        return f'OneOf({", ".join(repr(v) for v in self.values)})'


class MaxValue(_PointerConstraint):
    """Inclusive upper bound on a numeric property."""

    def __init__(self, value: int | float) -> None:
        self.value = value

    def __repr__(self) -> str:
        return f'MaxValue({self.value!r})'


class MaxExValue(_PointerConstraint):
    """Exclusive upper bound on a numeric property."""

    def __init__(self, value: int | float) -> None:
        self.value = value

    def __repr__(self) -> str:
        return f'MaxExValue({self.value!r})'


class MinValue(_PointerConstraint):
    """Inclusive lower bound on a numeric property."""

    def __init__(self, value: int | float) -> None:
        self.value = value

    def __repr__(self) -> str:
        return f'MinValue({self.value!r})'


class MinExValue(_PointerConstraint):
    """Exclusive lower bound on a numeric property."""

    def __init__(self, value: int | float) -> None:
        self.value = value

    def __repr__(self) -> str:
        return f'MinExValue({self.value!r})'


class MaxLen(_PointerConstraint):
    """Maximum character/element length for string properties."""

    def __init__(self, length: int) -> None:
        self.length = length

    def __repr__(self) -> str:
        return f'MaxLen({self.length!r})'


class MinLen(_PointerConstraint):
    """Minimum character/element length for string properties."""

    def __init__(self, length: int) -> None:
        self.length = length

    def __repr__(self) -> str:
        return f'MinLen({self.length!r})'


class Regexp(_PointerConstraint):
    """Regular-expression pattern constraint for string properties."""

    def __init__(self, pattern: str) -> None:
        self.pattern = pattern

    def __repr__(self) -> str:
        return f'Regexp({self.pattern!r})'


# ── Dual-use: pointer annotation or class-body type description ────────────────


class Description(_PointerConstraint):
    """Human-readable description for a type, property, or link.

    Used inside a pointer annotation::

        price: Property[pylon.Decimal, Description('Price excl. tax')]

    Or as a standalone class-body expression (overrides the docstring)::

        @pylon.type
        class Product:
            Description('A product available for purchase.')

    When used inside Property[T, ...] or Link[T, ...], the pointer annotation
    builder calls _collector.unregister() to remove it from the drain list so
    it is not mistakenly treated as the type-level description.
    """

    def __init__(self, text: str) -> None:
        self.text = text
        _collector.register(self)

    def __repr__(self) -> str:
        return f'Description({self.text!r})'


# ── Constraints with both pointer-level and class-body forms ───────────────────


class Exclusive(_PointerConstraint):
    """Unique constraint.

    Bare class reference inside ``Property[T, Exclusive]`` or
    ``Link[T, Exclusive]``: marks the pointer unique; the class itself is used,
    no instance is created and nothing is registered.

    Instantiated in the class body for composite uniqueness::

        Exclusive(('tenant_id', 'slug'))
        Exclusive(('tenant_id', 'slug'), unless='.deleted')

    The instance auto-registers in the collector so the type decorator picks
    it up as a class-level constraint.
    """

    pointers: tuple[str, ...]
    unless: str | None

    def __init__(
        self,
        pointers: str | tuple[str, ...],
        *,
        unless: str | None = None,
    ) -> None:
        self.pointers = (pointers,) if isinstance(pointers, str) else tuple(pointers)
        self.unless = unless
        _collector.register(self)

    def __repr__(self) -> str:
        parts = [repr(self.pointers)]
        if self.unless is not None:
            parts.append(f'unless={self.unless!r}')
        return f'Exclusive({", ".join(parts)})'


class Readonly(_PointerConstraint):
    """Marks a property or link as read-only in PyQL.

    The pointer can still be written at the database level; the transpiler
    rejects any PyQL update that tries to assign to it.  Used as a bare class
    reference::

        created_by: Link[User, Readonly]
        slug: Property[str, Readonly, MaxLen(120)]
    """


class Expression(_PointerConstraint):
    """Arbitrary PyQL boolean expression declared in the class body.

    Uses ``__subject__`` to reference the current object::

        Expression('__subject__.start_date <= __subject__.end_date')
    """

    def __init__(self, expr: str) -> None:
        self.expr = expr
        _collector.register(self)

    def __repr__(self) -> str:
        return f'Expression({self.expr!r})'
