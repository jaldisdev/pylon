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

import dataclasses
import sys


class NamedTuple:
    """Base class for all Pylon named tuple types.

    Instances are value types stored as jsonb in PostgreSQL.  Query results
    are decoded back to instances of the registered subclass.

    Usage::

        @pylon.named_tuple
        class Point(pylon.NamedTuple):
            x: pylon.Float64
            y: pylon.Float64

        Point(x=1.0, y=2.0)
    """


def _inject_named_tuple_repr(cls: type, pylon_module: str) -> None:
    """Replace the dataclass-generated `__repr__` with the same
    `(field := value, ...)` format `pylon.datatypes.NamedTupleValue` uses
    for anonymous named tuples, prefixed with the qualified type name —
    e.g. `default::Point (x := 1.0, y := 2.0)` — rather than the
    dataclass-default `Point(x=1.0, y=2.0)`, which reads more like a
    regular object than the tuple value it actually is.
    """

    def __repr__(self) -> str:
        pairs = ', '.join(f'{k} := {{}}' if v is None else f'{k} := {v!r}' for k, v in vars(self).items())
        return f'{pylon_module}::{cls.__name__} ({pairs})'

    cls.__repr__ = __repr__  # type: ignore[method-assign]


def named_tuple_decorator(cls: type) -> type:
    """Class decorator that registers a NamedTuple subclass as a Pylon named tuple type."""
    dc = dataclasses.dataclass(cls)

    defining = sys.modules.get(cls.__module__)
    if defining is not None:
        override = getattr(defining, '__pylon_module__', None)
        pylon_module: str = (
            override if isinstance(override, str) else (cls.__module__ or 'default').rpartition('.')[-1] or 'default'
        )
    else:
        pylon_module = (cls.__module__ or 'default').rpartition('.')[-1] or 'default'

    dc.__pylon_module__ = pylon_module  # type: ignore[attr-defined]
    _inject_named_tuple_repr(dc, pylon_module)

    from . import _registry

    _registry.register_named_tuple(dc)
    return dc
