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


class _Lazy:
    """Annotated metadata marker for resolving a forward-referenced type.

    The module_path is a dot-prefixed path relative to the defining module.
    Pylon resolves the string at schema build time using this path as the
    import anchor.
    """

    __slots__ = ("module_path",)

    def __init__(self, module_path: str) -> None:
        self.module_path = module_path

    def __repr__(self) -> str:
        return f"pylon.lazy({self.module_path!r})"


def lazy(module_path: str) -> _Lazy:
    """Break circular imports in link annotations.

    Usage::

        from __future__ import annotations
        from typing import Annotated
        import pylon

        @pylon.type
        class Order:
            product: Link[Annotated['Product', pylon.lazy('.product')]]

    The string ``'Product'`` is a forward reference. pylon.lazy supplies the
    module path so the schema builder can locate the type without requiring
    the import to be resolved at class-definition time.
    """
    return _Lazy(module_path)
