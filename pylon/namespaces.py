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

"""Import site for the PyQL stdlib namespaces.

The objects themselves live in `pylon.modelquery` next to the expression tree
they build. This module exists so the generated `namespaces.pyi` has somewhere
to attach: a stub file shadows its module entirely for type checkers, and
stubbing all of `modelquery` by hand would be far more to maintain than the
handful of names re-exported here.

`pylon/__init__.py` imports from this module, so `from pylon import std` picks
up the generated signatures.
"""

from __future__ import annotations

from pylon.modelquery import cal, math, std, sys

__all__ = ['cal', 'math', 'std', 'sys']
