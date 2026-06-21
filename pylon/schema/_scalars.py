from __future__ import annotations

import datetime
import decimal
import uuid as _uuid_mod
from typing import Any

from ._constraints import _FieldConstraint

# ── Built-in scalar marker types ──────────────────────────────────────────────


class _PylonScalar:
    """Marker base for all built-in Pylon scalar types."""

    __slots__ = ()


class Str(_PylonScalar):
    pass


class Int16(_PylonScalar):
    pass


class Int32(_PylonScalar):
    pass


class Int64(_PylonScalar):
    pass


class Float32(_PylonScalar):
    pass


class Float64(_PylonScalar):
    pass


class Decimal(_PylonScalar):
    pass


class Bool(_PylonScalar):
    pass


class DateTime(_PylonScalar):
    pass


class LocalDateTime(_PylonScalar):
    pass


class LocalDate(_PylonScalar):
    pass


class LocalTime(_PylonScalar):
    pass


class Duration(_PylonScalar):
    pass


class UUID(_PylonScalar):
    pass


class JSON(_PylonScalar):
    pass


class Bytes(_PylonScalar):
    pass


# Python shorthand → canonical Pylon scalar.
SHORTHAND_MAP: dict[type, type[_PylonScalar]] = {
    str: Str,
    int: Int64,
    float: Float64,
    bool: Bool,
    decimal.Decimal: Decimal,
    datetime.datetime: DateTime,
    datetime.date: LocalDate,
    datetime.time: LocalTime,
    datetime.timedelta: Duration,
    _uuid_mod.UUID: UUID,
}

# Pylon scalar → PostgreSQL type name; consumed by the DDL generator.
PG_TYPE_MAP: dict[type[_PylonScalar], str] = {
    Str: "text",
    Int16: "int2",
    Int32: "int4",
    Int64: "int8",
    Float32: "float4",
    Float64: "float8",
    Decimal: "numeric",
    Bool: "boolean",
    DateTime: "timestamptz",
    LocalDateTime: "timestamp",
    LocalDate: "date",
    LocalTime: "time",
    Duration: "interval",
    UUID: "uuid",
    JSON: "jsonb",
    Bytes: "bytea",
}


# ── Custom scalar base ─────────────────────────────────────────────────────────


class Scalar:
    """Base class for custom Pylon scalars.

    All three hook methods are optional; the defaults are no-op pass-throughs.

    Class form usage::

        @pylon.scalar(pylon.Str)
        class Email(pylon.Scalar):
            @staticmethod
            def validate(value: str) -> None:
                if '@' not in value:
                    raise ValueError(f'Invalid email: {value!r}')

            @staticmethod
            def from_db(value: str) -> 'Email':
                return Email(value)

            @staticmethod
            def to_db(value: 'Email') -> str:
                return str(value)
    """

    __pylon_base__: type[_PylonScalar]
    __pylon_constraints__: tuple[_FieldConstraint, ...]

    @staticmethod
    def validate(value: Any) -> None:
        """Python-side validation. Raise ValueError on failure."""

    @staticmethod
    def from_db(value: Any) -> Any:
        """Convert a raw database value to its Python representation."""
        return value

    @staticmethod
    def to_db(value: Any) -> Any:
        """Convert a Python value to its raw database representation."""
        return value


# ── scalar() function / decorator ─────────────────────────────────────────────


def scalar(
    base_type: type[_PylonScalar],
    *constraints: _FieldConstraint,
) -> type[Scalar] | Any:
    """Define a custom scalar type.

    Functional form — returns a new anonymous scalar type immediately::

        PositiveInt = pylon.scalar(pylon.Int64, MinValue(0))
        EmailStr    = pylon.scalar(pylon.Str, Regexp(r'^[^@]+@[^@]+\\.[^@]+$'))

    Decorator form — applied to a Scalar subclass for full control::

        @pylon.scalar(pylon.Str)
        class Email(pylon.Scalar):
            @staticmethod
            def validate(value: str) -> None: ...

            @staticmethod
            def from_db(value: str) -> Email: ...

            @staticmethod
            def to_db(value: Email) -> str: ...
    """
    if constraints:
        return type(
            "_AnonymousScalar",
            (Scalar,),
            {
                "__pylon_base__": base_type,
                "__pylon_constraints__": constraints,
            },
        )

    def _decorator(cls: type[Scalar]) -> type[Scalar]:
        cls.__pylon_base__ = base_type
        cls.__pylon_constraints__ = ()
        from . import _registry
        _registry.register_scalar(cls)
        return cls

    return _decorator
