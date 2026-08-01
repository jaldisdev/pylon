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

import threading

_local = threading.local()


def _list() -> list[object]:
    if not hasattr(_local, "items"):
        _local.items = []
    return _local.items


def register(expr: object) -> None:
    """Append expr to the thread-local pending list."""
    _list().append(expr)


def unregister(expr: object) -> None:
    """Remove expr from the pending list; no-op if not present."""
    try:
        _list().remove(expr)
    except ValueError:
        pass


def drain() -> list[object]:
    """Return all pending expressions and clear the list."""
    items = list(_list())
    _list().clear()
    return items
