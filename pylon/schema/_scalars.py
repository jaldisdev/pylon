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

import datetime
import decimal
import sys
import uuid as _uuid_mod
from typing import Any

from ._constraints import _PointerConstraint

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


class Sequence(_PylonScalar):
    """Marker for auto-incrementing sequence scalars (backed by a PostgreSQL SEQUENCE + DOMAIN)."""

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
    Str: 'text',
    Int16: 'int2',
    Int32: 'int4',
    Int64: 'int8',
    Float32: 'float4',
    Float64: 'float8',
    Decimal: 'numeric',
    Bool: 'boolean',
    DateTime: 'timestamptz',
    LocalDateTime: 'timestamp',
    LocalDate: 'date',
    LocalTime: 'time',
    Duration: 'interval',
    UUID: 'uuid',
    JSON: 'jsonb',
    Bytes: 'bytea',
    Sequence: 'int8',
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
    __pylon_constraints__: tuple[_PointerConstraint, ...]

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
    *constraints: _PointerConstraint,
    name: str | None = None,
    module: str | None = None,
) -> type[Scalar] | Any:
    """Define a custom scalar type.

    Functional form — returns a new scalar type immediately. Pass ``name=``
    to register it as a nominal PostgreSQL DOMAIN: its constraints compile
    into the domain's own CHECK, enforced by Postgres on every write, and
    any property using this scalar gets that domain as its actual column
    type (see `pylon.schema._walker._to_pg_type`)::

        EmailStr = pylon.scalar(pylon.Str, Regexp(r'^[^@]+@[^@]+\\.[^@]+$'), name='EmailStr')
        Rating   = pylon.scalar(pylon.Int16, MinValue(1), MaxValue(5), name='Rating')

    Omitting ``name=`` returns an unregistered scalar with no PostgreSQL
    identity of its own — its constraints still apply, but individually on
    each property that uses it (no named type is created)::

        PositiveInt = pylon.scalar(pylon.Int64, MinValue(0))

    Decorator form — applied to a Scalar subclass for full control (always
    registered under the class's own name; constraints aren't supported
    here since `validate()` covers logic a CHECK can't express)::

        @pylon.scalar(pylon.Str)
        class Email(pylon.Scalar):
            @staticmethod
            def validate(value: str) -> None: ...

            @staticmethod
            def from_db(value: str) -> Email: ...

            @staticmethod
            def to_db(value: Email) -> str: ...
    """
    if constraints or name is not None:
        caller_module = module or sys._getframe(1).f_globals.get('__name__', 'default')
        cls = type(
            name or '_AnonymousScalar',
            (Scalar,),
            {
                '__pylon_base__': base_type,
                '__pylon_constraints__': constraints,
                '__module__': caller_module,
            },
        )
        if name is not None:
            defining = sys.modules.get(caller_module)
            override = getattr(defining, '__pylon_module__', None) if defining else None
            cls.__pylon_module__ = (
                override if isinstance(override, str) else (caller_module or 'default').rpartition('.')[-1] or 'default'
            )
            from . import _registry

            _registry.register_scalar(cls)
        return cls

    def _decorator(cls: type[Scalar]) -> type[Scalar]:
        cls.__pylon_base__ = base_type
        cls.__pylon_constraints__ = ()
        from . import _registry

        _registry.register_scalar(cls)
        return cls

    return _decorator
