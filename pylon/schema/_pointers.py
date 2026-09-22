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

import typing
from typing import Any

from . import _collector

# ── Deletion policy types ──────────────────────────────────────────────────────


class _Side:
    __slots__ = ('name',)

    def __init__(self, name: str) -> None:
        self.name = name

    def __repr__(self) -> str:
        return self.name


class _Action:
    __slots__ = ('name',)

    def __init__(self, name: str) -> None:
        self.name = name

    def __repr__(self) -> str:
        return self.name


Target = _Side('Target')
Source = _Side('Source')

Allow = _Action('Allow')
Restrict = _Action('Restrict')
DeferredRestrict = _Action('DeferredRestrict')
DeleteSource = _Action('DeleteSource')
DeleteTarget = _Action('DeleteTarget')
DeleteTargetIfOrphan = _Action('DeleteTargetIfOrphan')


class OnDelete:
    """Deletion policy for a Link or MultiLink.

    Usage::

        chat: Link[MessageThread, OnDelete(Target, DeleteSource)]
        messages: MultiLink[Message, OnDelete(Source, DeleteTargetIfOrphan)]
    """

    __slots__ = ('action', 'side')

    def __init__(self, side: _Side, action: _Action) -> None:
        self.side = side
        self.action = action

    def __repr__(self) -> str:
        return f'OnDelete({self.side!r}, {self.action!r})'


# ── Annotation result types ────────────────────────────────────────────────────
#
# All four annotation result classes implement __or__ so that the | None
# syntax (e.g. Link[Category] | None) produces a typing.Union that
# _unwrap_optional already knows how to decompose.


class PropertyAnnotation:
    __slots__ = ('constraints', 'scalar_type')

    def __init__(self, scalar_type: Any, constraints: list[Any]) -> None:
        self.scalar_type = scalar_type
        self.constraints = constraints

    def __or__(self, other: Any) -> Any:
        if other is None:
            return typing.Union[self, type(None)]
        return NotImplemented

    def __repr__(self) -> str:
        return f'PropertyAnnotation({self.scalar_type!r}, {self.constraints!r})'


class LinkAnnotation:
    __slots__ = ('constraints', 'on_delete', 'target_type', 'through_type')

    def __init__(
        self,
        target_type: Any,
        constraints: list[Any],
        on_delete: list[OnDelete],
        through_type: Any = None,
    ) -> None:
        self.target_type = target_type
        self.constraints = constraints
        self.on_delete = on_delete
        self.through_type = through_type

    def __or__(self, other: Any) -> Any:
        if other is None:
            return typing.Union[self, type(None)]
        return NotImplemented

    def __repr__(self) -> str:
        return f'LinkAnnotation({self.target_type!r}, {self.constraints!r}, through={self.through_type!r})'


class MultiLinkAnnotation:
    __slots__ = ('constraints', 'on_delete', 'target_type', 'through_type')

    def __init__(
        self,
        target_type: Any,
        through_type: Any = None,
        on_delete: list[OnDelete] | None = None,
        constraints: list[Any] | None = None,
    ) -> None:
        self.target_type = target_type
        self.through_type = through_type
        self.on_delete = on_delete or []
        self.constraints = constraints or []

    def __or__(self, other: Any) -> Any:
        if other is None:
            return typing.Union[self, type(None)]
        return NotImplemented

    def __repr__(self) -> str:
        return f'MultiLinkAnnotation({self.target_type!r}, through={self.through_type!r})'


class ComputedAnnotation:
    __slots__ = ('expression', 'return_type')

    def __init__(self, return_type: Any, expression: str) -> None:
        self.return_type = return_type
        self.expression = expression

    def __or__(self, other: Any) -> Any:
        if other is None:
            return typing.Union[self, type(None)]
        return NotImplemented

    def __repr__(self) -> str:
        return f'ComputedAnnotation({self.return_type!r}, {self.expression!r})'


# ── Through ────────────────────────────────────────────────────────────────────


class Through:
    """Declare the intermediate (junction) type for a link with its own
    properties — supported on both a single Link and a MultiLink.

    Usage::

        tags: MultiLink[Tag, Through[ProductTag]]
        spouse: Link[Person, Through[Marriage]] | None
    """

    __slots__ = ('type_',)

    def __init__(self, type_: Any) -> None:
        self.type_ = type_

    @classmethod
    def __class_getitem__(cls, type_: Any) -> Through:
        return cls(type_)

    def __repr__(self) -> str:
        return f'Through[{self.type_!r}]'


# ── Helpers ────────────────────────────────────────────────────────────────────


def _consume_registered(params: list[Any]) -> list[Any]:
    """Pull every self-registering constraint in *params* off the pending list.

    `Description`, `Exclusive(...)` and `Expression(...)` all register
    themselves on construction so a bare one in a class body is picked up as
    type-level. Inside `Property[T, ...]`/`Link[T, ...]` they belong to the
    pointer instead, so they have to come back off the list.

    Leaving one on it was actively wrong under `from __future__ import
    annotations`: the annotation isn't evaluated when the class body runs but
    when something later resolves it, so the stray constraint was drained by
    whichever *other* class happened to be under construction at that moment
    — landing, say, an `ExchangeRate.currency` check on an unrelated type
    that has no `currency` property at all.

    `unregister` is a no-op for anything not on the list, so this is safe to
    call over every parameter.
    """
    for p in params:
        _collector.unregister(p)
    return params


# ── Pointer annotation classes ─────────────────────────────────────────────────


class Property:
    """Scalar pointer annotation.

    Usage::

        name: Property[str]
        name: Property[str, MaxLen(120)]
        name: Property[str, Exclusive, MaxLen(120)]
        price: Property[pylon.Decimal, MinValue(0), Description('Price excl. tax')]
        created_at: Property[pylon.DateTime, Default(Now)]
    """

    @classmethod
    def __class_getitem__(cls, params: Any) -> PropertyAnnotation:
        if not isinstance(params, tuple):
            params = (params,)
        scalar_type = params[0]
        constraints = _consume_registered(list(params[1:]))
        return PropertyAnnotation(scalar_type=scalar_type, constraints=constraints)


class Link:
    """Single-link pointer annotation (many-to-one or one-to-one).

    Usage::

        category: Link[Category]
        category: Link[Category] | None
        category: Link[Category, Description('The owning category')]
        chat: Link[MessageThread, OnDelete(Target, DeleteSource)]
        spouse: Link[Person, Through[Marriage]] | None
    """

    @classmethod
    def __class_getitem__(cls, params: Any) -> LinkAnnotation:
        if not isinstance(params, tuple):
            params = (params,)
        target_type = params[0]
        rest = list(params[1:])
        through_type = None
        on_delete: list[OnDelete] = []
        remaining: list[Any] = []
        for p in rest:
            if isinstance(p, Through):
                through_type = p.type_
            elif isinstance(p, OnDelete):
                on_delete.append(p)
            else:
                remaining.append(p)
        constraints = _consume_registered(remaining)
        return LinkAnnotation(
            target_type=target_type, constraints=constraints, on_delete=on_delete, through_type=through_type
        )


class MultiLink:
    """Multi-link pointer annotation (one-to-many or many-to-many).

    Usage::

        tags: MultiLink[Tag]
        tags: MultiLink[Tag, Through[ProductTag]]
        tags: MultiLink[Tag] | None
        messages: MultiLink[Message, OnDelete(Source, DeleteTargetIfOrphan)]
    """

    @classmethod
    def __class_getitem__(cls, params: Any) -> MultiLinkAnnotation:
        if not isinstance(params, tuple):
            params = (params,)
        target_type = params[0]
        through_type = None
        on_delete: list[OnDelete] = []
        remaining: list[Any] = []
        for p in params[1:]:
            if isinstance(p, Through):
                through_type = p.type_
            elif isinstance(p, OnDelete):
                on_delete.append(p)
            else:
                remaining.append(p)
        return MultiLinkAnnotation(
            target_type=target_type,
            through_type=through_type,
            on_delete=on_delete,
            constraints=_consume_registered(remaining),
        )


class TupleElement:
    """One member of a structural tuple type: `(name, type)` or a bare type."""

    __slots__ = ('name', 'type_')

    def __init__(self, name: str | None, type_: Any) -> None:
        self.name = name
        self.type_ = type_

    def __repr__(self) -> str:
        return f'TupleElement({self.name!r}, {self.type_!r})'


class TupleAnnotation:
    """Structural tuple pointer annotation.

    Usage::

        pair: Tuple[Str, Bool]
        rgb: Tuple[("r", Int16), ("g", Int16), ("b", Int16)]
        shape: Tuple[("origin", Tuple[("x", Float64), ("y", Float64)]), ("size", Float64)]
    """

    __slots__ = ('elements',)

    def __init__(self, elements: list[TupleElement]) -> None:
        self.elements = elements

    def __or__(self, other: Any) -> Any:
        if other is None:
            return typing.Union[self, type(None)]
        return NotImplemented

    def __repr__(self) -> str:
        return f'TupleAnnotation({self.elements!r})'


class Tuple:
    """Structural tuple pointer annotation (an inline `tuple<...>` equivalent).

    Usage::

        pair: Tuple[Str, Bool]                                # unnamed elements
        rgb: Tuple[("r", Int16), ("g", Int16), ("b", Int16)]   # named elements
        shape: Tuple[("origin", Tuple[("x", Float64), ("y", Float64)]), ("size", Float64)]
    """

    @classmethod
    def __class_getitem__(cls, params: Any) -> TupleAnnotation:
        def _is_named_item(p: Any) -> bool:
            return isinstance(p, tuple) and len(p) == 2 and isinstance(p[0], str) and not isinstance(p[1], str)

        if _is_named_item(params):
            # Tuple[("name", Type)] — a single named element. Python's subscript
            # syntax doesn't add an extra wrapping tuple when the bracket
            # contains one already-parenthesized item, so this arrives
            # unwrapped and indistinguishable from Tuple["name", Type] (two
            # unnamed elements) by shape alone; re-wrap it here. Safe because a
            # bare `str` can never legitimately be an unnamed element's type.
            params = (params,)
        elif not isinstance(params, tuple):
            params = (params,)
        if not params:
            raise TypeError('Tuple[...] requires at least one element')

        named_flags = [_is_named_item(p) for p in params]
        if any(named_flags) and not all(named_flags):
            raise TypeError("Tuple[...] elements must be all named ('name', Type) or all unnamed Type, not mixed")

        if all(named_flags):
            elements = [TupleElement(name=p[0], type_=p[1]) for p in params]
        else:
            elements = [TupleElement(name=None, type_=p) for p in params]

        return TupleAnnotation(elements=elements)


class ArrayAnnotation:
    """Structural array pointer annotation.

    Usage::

        tags: Array[Str]
        scores: Array[Int64]
    """

    __slots__ = ('element',)

    def __init__(self, element: Any) -> None:
        self.element = element

    def __or__(self, other: Any) -> Any:
        if other is None:
            return typing.Union[self, type(None)]
        return NotImplemented

    def __repr__(self) -> str:
        return f'ArrayAnnotation({self.element!r})'


class Array:
    """One-dimensional array pointer annotation (an inline `array<...>`
    equivalent). The element type may be anything except another array —
    Pylon arrays are always one-dimensional, matching a plain Postgres
    `T[]` column (never jsonb, unlike Tuple).

    Usage::

        tags: Array[Str]
        scores: Array[Int64]
    """

    @classmethod
    def __class_getitem__(cls, element: Any) -> ArrayAnnotation:
        if isinstance(element, ArrayAnnotation):
            raise TypeError('Array[Array[...]] is not supported; arrays must be one-dimensional')
        return ArrayAnnotation(element=element)


class Computed:
    """Computed pointer — evaluated as a PyQL expression at query time.

    Computed pointers are volatile: they are excluded from __init__ and cannot
    appear in indexes or constraints. Their value is populated from the query
    response.

    Usage::

        full_name: Computed[str, '.first_name ++ " " ++ .last_name']
        recent_orders: Computed[MultiLink[Order], '.orders order by .created_at desc limit 5']
    """

    @classmethod
    def __class_getitem__(cls, params: Any) -> ComputedAnnotation:
        if not isinstance(params, tuple) or len(params) != 2:
            raise TypeError('Computed requires exactly two parameters: Computed[return_type, "pyql_expression"]')
        return_type, expression = params
        if not isinstance(expression, str):
            raise TypeError(f'Computed expression must be a string literal, got {type(expression).__name__!r}')
        return ComputedAnnotation(return_type=return_type, expression=expression)
