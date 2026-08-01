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
import typing
from typing import Any


class AliasAnnotation:
    __slots__ = ("expr",)

    def __init__(self, expr: str) -> None:
        if not isinstance(expr, str):
            raise TypeError(
                f"Alias expression must be a string literal, got {type(expr).__name__!r}"
            )
        self.expr = expr

    def __repr__(self) -> str:
        return f"AliasAnnotation({self.expr!r})"


class Alias:
    """Schema-level named PyQL expression.

    Usage::

        published_posts: pylon.Alias['select Post filter .is_published = true']
        male_persons: pylon.Alias['select Person filter .gender = Gender.Male']
    """

    @classmethod
    def __class_getitem__(cls, expr: Any) -> AliasAnnotation:
        return AliasAnnotation(expr)


@dataclasses.dataclass
class AliasDescriptor:
    """Collected metadata for a single module-level alias."""

    name: str
    module: str
    expr: str

    def __repr__(self) -> str:
        return f"AliasDescriptor({self.name!r}, module={self.module!r}, expr={self.expr!r})"


def _infer_module_name(module: Any) -> str:
    override = getattr(module, "__pylon_module__", None)
    if isinstance(override, str):
        return override
    module_path = getattr(module, "__name__", "default")
    return module_path.rpartition(".")[-1] or module_path


def collect_module_aliases(module: Any) -> list[AliasDescriptor]:
    """Scan a Python module for Alias['expr'] annotations and return descriptors."""
    try:
        hints = typing.get_type_hints(module)
    except Exception:
        hints = dict(getattr(module, "__annotations__", {}))

    module_name = _infer_module_name(module)
    result: list[AliasDescriptor] = []
    for name, annotation in hints.items():
        if name.startswith("_"):
            continue
        if not isinstance(annotation, AliasAnnotation):
            continue
        result.append(AliasDescriptor(name=name, module=module_name, expr=annotation.expr))
    return result
