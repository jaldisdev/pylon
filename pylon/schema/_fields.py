from __future__ import annotations

import typing
from typing import Any

from . import _collector
from ._constraints import Description

# ── Deletion policy types ──────────────────────────────────────────────────────


class _Side:
    __slots__ = ("name",)

    def __init__(self, name: str) -> None:
        self.name = name

    def __repr__(self) -> str:
        return self.name


class _Action:
    __slots__ = ("name",)

    def __init__(self, name: str) -> None:
        self.name = name

    def __repr__(self) -> str:
        return self.name


Target = _Side("Target")
Source = _Side("Source")

Allow = _Action("Allow")
Restrict = _Action("Restrict")
DeferredRestrict = _Action("DeferredRestrict")
DeleteSource = _Action("DeleteSource")
DeleteTarget = _Action("DeleteTarget")
DeleteTargetIfOrphan = _Action("DeleteTargetIfOrphan")


class OnDelete:
    """Deletion policy for a Link or MultiLink.

    Usage::

        chat: Link[MessageThread, OnDelete(Target, DeleteSource)]
        messages: MultiLink[Message, OnDelete(Source, DeleteTargetIfOrphan)]
    """

    __slots__ = ("side", "action")

    def __init__(self, side: _Side, action: _Action) -> None:
        self.side = side
        self.action = action

    def __repr__(self) -> str:
        return f"OnDelete({self.side!r}, {self.action!r})"


# ── Annotation result types ────────────────────────────────────────────────────
#
# All four annotation result classes implement __or__ so that the | None
# syntax (e.g. Link[Category] | None) produces a typing.Union that
# _unwrap_optional already knows how to decompose.


class PropertyAnnotation:
    __slots__ = ("scalar_type", "constraints")

    def __init__(self, scalar_type: Any, constraints: list[Any]) -> None:
        self.scalar_type = scalar_type
        self.constraints = constraints

    def __or__(self, other: Any) -> Any:
        if other is None:
            return typing.Union[self, type(None)]
        return NotImplemented

    def __repr__(self) -> str:
        return f"PropertyAnnotation({self.scalar_type!r}, {self.constraints!r})"


class LinkAnnotation:
    __slots__ = ("target_type", "constraints", "on_delete")

    def __init__(
        self, target_type: Any, constraints: list[Any], on_delete: list[OnDelete]
    ) -> None:
        self.target_type = target_type
        self.constraints = constraints
        self.on_delete = on_delete

    def __or__(self, other: Any) -> Any:
        if other is None:
            return typing.Union[self, type(None)]
        return NotImplemented

    def __repr__(self) -> str:
        return f"LinkAnnotation({self.target_type!r}, {self.constraints!r})"


class MultiLinkAnnotation:
    __slots__ = ("target_type", "through_type", "on_delete")

    def __init__(
        self, target_type: Any, through_type: Any = None, on_delete: list[OnDelete] | None = None
    ) -> None:
        self.target_type = target_type
        self.through_type = through_type
        self.on_delete = on_delete or []

    def __or__(self, other: Any) -> Any:
        if other is None:
            return typing.Union[self, type(None)]
        return NotImplemented

    def __repr__(self) -> str:
        return (
            f"MultiLinkAnnotation({self.target_type!r}, through={self.through_type!r})"
        )


class ComputedAnnotation:
    __slots__ = ("return_type", "expression")

    def __init__(self, return_type: Any, expression: str) -> None:
        self.return_type = return_type
        self.expression = expression

    def __or__(self, other: Any) -> Any:
        if other is None:
            return typing.Union[self, type(None)]
        return NotImplemented

    def __repr__(self) -> str:
        return f"ComputedAnnotation({self.return_type!r}, {self.expression!r})"


# ── Through helper ─────────────────────────────────────────────────────────────


class _ThroughParam:
    """Marker produced by through(); recognised inside MultiLink[T, through(X)]."""

    __slots__ = ("type_",)

    def __init__(self, type_: Any) -> None:
        self.type_ = type_

    def __repr__(self) -> str:
        return f"through({self.type_!r})"


def through(type_: Any) -> _ThroughParam:
    """Declare the intermediate type for a MultiLink with link properties.

    Usage::

        tags: MultiLink[Tag, through(ProductTag)]
    """
    return _ThroughParam(type_)


# ── Helpers ────────────────────────────────────────────────────────────────────


def _consume_descriptions(params: list[Any]) -> list[Any]:
    """Unregister any Description instances from the collector.

    Description.__init__ always registers; when a Description appears inside
    Property[T, ...] or Link[T, ...] it belongs to the field, not the type,
    so we pull it back out of the pending list.
    """
    for p in params:
        if isinstance(p, Description):
            _collector.unregister(p)
    return params


# ── Field annotation classes ───────────────────────────────────────────────────


class Property:
    """Scalar field annotation.

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
        constraints = _consume_descriptions(list(params[1:]))
        return PropertyAnnotation(scalar_type=scalar_type, constraints=constraints)


class Link:
    """Single-link field annotation (many-to-one or one-to-one).

    Usage::

        category: Link[Category]
        category: Link[Category] | None
        category: Link[Category, Description('The owning category')]
        chat: Link[MessageThread, OnDelete(Target, DeleteSource)]
    """

    @classmethod
    def __class_getitem__(cls, params: Any) -> LinkAnnotation:
        if not isinstance(params, tuple):
            params = (params,)
        target_type = params[0]
        rest = list(params[1:])
        on_delete = [p for p in rest if isinstance(p, OnDelete)]
        constraints = _consume_descriptions([p for p in rest if not isinstance(p, OnDelete)])
        return LinkAnnotation(target_type=target_type, constraints=constraints, on_delete=on_delete)


class MultiLink:
    """Multi-link field annotation (one-to-many or many-to-many).

    Usage::

        tags: MultiLink[Tag]
        tags: MultiLink[Tag, through(ProductTag)]
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
        for p in params[1:]:
            if isinstance(p, _ThroughParam):
                through_type = p.type_
            elif isinstance(p, OnDelete):
                on_delete.append(p)
        return MultiLinkAnnotation(target_type=target_type, through_type=through_type, on_delete=on_delete)


class Computed:
    """Computed field — evaluated as a PyQL expression at query time.

    Computed fields are volatile: they are excluded from __init__ and cannot
    appear in indexes or constraints. Their value is populated from the query
    response.

    Usage::

        full_name: Computed[str, '.first_name ++ " " ++ .last_name']
        recent_orders: Computed[MultiLink[Order], '.orders order by .created_at desc limit 5']
    """

    @classmethod
    def __class_getitem__(cls, params: Any) -> ComputedAnnotation:
        if not isinstance(params, tuple) or len(params) != 2:
            raise TypeError(
                "Computed requires exactly two parameters: "
                'Computed[return_type, "pyql_expression"]'
            )
        return_type, expression = params
        if not isinstance(expression, str):
            raise TypeError(
                f"Computed expression must be a string literal, "
                f"got {type(expression).__name__!r}"
            )
        return ComputedAnnotation(return_type=return_type, expression=expression)
