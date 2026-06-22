from __future__ import annotations

from . import _collector


class _FieldConstraint:
    """Marker base for all constraint objects."""


# ── Sentinels ──────────────────────────────────────────────────────────────────


class _NowType:
    _inst: "_NowType | None" = None

    def __new__(cls) -> "_NowType":
        if cls._inst is None:
            cls._inst = super().__new__(cls)
        return cls._inst

    def __repr__(self) -> str:
        return "Now"


Now = _NowType()


class Default(_FieldConstraint):
    """Server-side default. Only Default(Now) is supported in the initial spec.

    At the Python level the field is optional and defaults to None; the DDL
    generator emits DEFAULT NOW() (or the appropriate expression) in the column
    definition.
    """

    def __init__(self, sentinel: object) -> None:
        self.sentinel = sentinel

    def __repr__(self) -> str:
        return f"Default({self.sentinel!r})"


# ── Field-level constraints ────────────────────────────────────────────────────


class OneOf(_FieldConstraint):
    """Restricts a property to an explicit set of allowed values."""

    def __init__(self, *values: object) -> None:
        self.values = values

    def __repr__(self) -> str:
        return f"OneOf({', '.join(repr(v) for v in self.values)})"


class MaxValue(_FieldConstraint):
    """Inclusive upper bound on a numeric property."""

    def __init__(self, value: int | float) -> None:
        self.value = value

    def __repr__(self) -> str:
        return f"MaxValue({self.value!r})"


class MaxExValue(_FieldConstraint):
    """Exclusive upper bound on a numeric property."""

    def __init__(self, value: int | float) -> None:
        self.value = value

    def __repr__(self) -> str:
        return f"MaxExValue({self.value!r})"


class MinValue(_FieldConstraint):
    """Inclusive lower bound on a numeric property."""

    def __init__(self, value: int | float) -> None:
        self.value = value

    def __repr__(self) -> str:
        return f"MinValue({self.value!r})"


class MinExValue(_FieldConstraint):
    """Exclusive lower bound on a numeric property."""

    def __init__(self, value: int | float) -> None:
        self.value = value

    def __repr__(self) -> str:
        return f"MinExValue({self.value!r})"


class MaxLen(_FieldConstraint):
    """Maximum character/element length for string properties."""

    def __init__(self, length: int) -> None:
        self.length = length

    def __repr__(self) -> str:
        return f"MaxLen({self.length!r})"


class MinLen(_FieldConstraint):
    """Minimum character/element length for string properties."""

    def __init__(self, length: int) -> None:
        self.length = length

    def __repr__(self) -> str:
        return f"MinLen({self.length!r})"


class Regexp(_FieldConstraint):
    """Regular-expression pattern constraint for string properties."""

    def __init__(self, pattern: str) -> None:
        self.pattern = pattern

    def __repr__(self) -> str:
        return f"Regexp({self.pattern!r})"


# ── Dual-use: field annotation or class-body type description ──────────────────


class Description(_FieldConstraint):
    """Human-readable description for a type, field, or link.

    Used inside a field annotation::

        price: Property[pylon.Decimal, Description('Price excl. tax')]

    Or as a standalone class-body expression (overrides the docstring)::

        @pylon.type
        class Product:
            Description('A product available for purchase.')

    When used inside Property[T, ...] or Link[T, ...], the field annotation
    builder calls _collector.unregister() to remove it from the drain list so
    it is not mistakenly treated as the type-level description.
    """

    def __init__(self, text: str) -> None:
        self.text = text
        _collector.register(self)

    def __repr__(self) -> str:
        return f"Description({self.text!r})"


# ── Constraints with both field-level and class-body forms ─────────────────────


class Exclusive(_FieldConstraint):
    """Unique constraint.

    Bare class reference inside ``Property[T, Exclusive]`` or
    ``Link[T, Exclusive]``: marks the field unique; the class itself is used,
    no instance is created and nothing is registered.

    Instantiated in the class body for composite uniqueness::

        Exclusive(('tenant_id', 'slug'))
        Exclusive(('tenant_id', 'slug'), unless='.deleted')

    The instance auto-registers in the collector so the type decorator picks
    it up as a class-level constraint.
    """

    fields: tuple[str, ...]
    unless: str | None

    def __init__(
        self,
        fields: str | tuple[str, ...],
        *,
        unless: str | None = None,
    ) -> None:
        self.fields = (fields,) if isinstance(fields, str) else tuple(fields)
        self.unless = unless
        _collector.register(self)

    def __repr__(self) -> str:
        parts = [repr(self.fields)]
        if self.unless is not None:
            parts.append(f"unless={self.unless!r}")
        return f"Exclusive({', '.join(parts)})"


class Readonly(_FieldConstraint):
    """Marks a property or link as read-only in PyQL.

    The field can still be written at the database level; the transpiler rejects
    any PyQL update that tries to assign to it.  Used as a bare class reference::

        created_by: Link[User, Readonly]
        slug: Property[str, Readonly, MaxLen(120)]
    """


class Expression(_FieldConstraint):
    """Arbitrary PyQL boolean expression declared in the class body.

    Uses ``__subject__`` to reference the current object::

        Expression('__subject__.start_date <= __subject__.end_date')
    """

    def __init__(self, expr: str) -> None:
        self.expr = expr
        _collector.register(self)

    def __repr__(self) -> str:
        return f"Expression({self.expr!r})"
