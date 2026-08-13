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

"""Python-side view of the PyQL stdlib registry.

The registry itself lives in Rust (`pylon-core/src/stdlib`) and is the single
source of truth for every `std::`/`math::`/`cal::` overload. This module loads
it once via `pylon._core.stdlib_registry()` and uses it to reject bad calls at
*build* time — when `std.foo(...)` is written — instead of letting them travel
all the way to the compiler as unresolvable text.

The gate is context-dependent, because the two Python entry points want
opposite halves of the namespace:

* a **filter/query expression** wants pure scalar functions and aggregates,
  and almost never wants a volatile one — `std.random()` inside a predicate
  re-rolls per row, which is virtually always a bug;
* a **pointer default** wants exactly the volatile ones
  (`std.uuid_generate_v7()`, `std.datetime_current()`) and can't accept an
  aggregate or a set-valued result at all.

Both gates are derived from registry metadata rather than a hand-written
allowlist, so they stay correct as the registry grows.
"""

from __future__ import annotations

import difflib
import keyword
from functools import lru_cache
from typing import Any

from pylon.exceptions import InterfaceError

#: Namespaces exposed as Python objects by default. The extension-backed
#: namespaces (`postgis` alone is several thousand overloads) are deliberately
#: excluded — they're reachable from PyQL text, just not worth materializing
#: into the Python surface for every process.
DEFAULT_NAMESPACES = ('std', 'math', 'cal', 'sys')

#: Where a stdlib call is being built. See the module docstring.
CONTEXT_EXPRESSION = 'expression'
CONTEXT_DEFAULT = 'default'


@lru_cache(maxsize=1)
def registry() -> tuple[dict[str, Any], ...]:
    """Every overload in the default namespaces, one dict per overload."""
    from pylon._core import stdlib_registry

    return tuple(stdlib_registry(list(DEFAULT_NAMESPACES)))


@lru_cache(maxsize=1)
def _by_namespace() -> dict[str, dict[str, list[dict[str, Any]]]]:
    """`{namespace: {name: [overload, ...]}}` — built once, read many."""
    index: dict[str, dict[str, list[dict[str, Any]]]] = {}
    for entry in registry():
        index.setdefault(entry['namespace'], {}).setdefault(entry['name'], []).append(entry)
    return index


def overloads(namespace: str, name: str) -> list[dict[str, Any]]:
    """Every overload for `namespace::name`, or an empty list if unknown."""
    return _by_namespace().get(namespace, {}).get(name, [])


def names(namespace: str) -> frozenset[str]:
    """Every function name in `namespace`."""
    return frozenset(_by_namespace().get(namespace, {}))


def python_name(name: str) -> str:
    """The attribute a PyQL function is reachable under in Python.

    `std::assert` is a real stdlib function, but `std.assert(...)` doesn't
    parse — `assert` is a Python keyword. Such names get the conventional
    trailing underscore (`std.assert_`), the same shape the `_INFIX_ALIASES`
    table already uses for `in_`. Every other name is unchanged.
    """
    if keyword.iskeyword(name) or keyword.issoftkeyword(name):
        return f'{name}_'
    return name


def pyql_name(attr: str) -> str:
    """Inverse of `python_name` — the registry name behind an attribute."""
    if attr.endswith('_'):
        stripped = attr[:-1]
        if keyword.iskeyword(stripped) or keyword.issoftkeyword(stripped):
            return stripped
    return attr


def is_constant(namespace: str, name: str) -> bool:
    """True for a zero-argument immutable entry such as `math::pi`.

    These read badly as calls in Python — `math.pi()` next to the standard
    library's `math.pi` — so the namespace objects expose them as plain
    attributes instead. Volatile zero-arg entries (`std::random`) are
    deliberately *not* included: they must stay calls, because writing
    `std.random` as a value would hide that it re-evaluates.
    """
    entries = overloads(namespace, name)
    return bool(entries) and all(not e['params'] and e['volatility'] == 'immutable' for e in entries)


def _suggest(namespace: str, name: str, extra: frozenset[str] = frozenset()) -> str:
    """A ` — did you mean ...?` fragment, or an empty string.

    `extra` widens the candidate pool beyond the registry — the namespace
    objects also accept the grammar-keyword aliases (`std.ilike`), which have
    no registry entry but are exactly the names a typo is most likely aimed
    at.
    """
    close = difflib.get_close_matches(name, names(namespace) | extra, n=1, cutoff=0.7)
    return f' — did you mean {namespace}.{close[0]}?' if close else ''


def _other_namespace(namespace: str, name: str) -> str | None:
    """The namespace that *does* define `name`, if some other one does.

    Worth its own check because several functions a Python developer expects
    in `math` deliberately live in `std` (`std.sqrt`, `std.abs`, `std.ceil`).
    Reaching for `math.sqrt` — which is exactly where Python's own standard
    library puts it — is the predictable first guess, and an unqualified
    "unknown function" gives no way to recover from it.
    """
    for other in DEFAULT_NAMESPACES:
        if other != namespace and name in names(other):
            return other
    return None


def unknown_function_message(namespace: str, name: str, extra: frozenset[str] = frozenset()) -> str:
    """Shared wording for an unresolvable name, with a suggestion when one is
    close enough to be worth offering."""
    elsewhere = _other_namespace(namespace, name)
    if elsewhere is not None:
        return f'unknown function {namespace}.{name}() — it lives in {elsewhere}, use {elsewhere}.{name}()'
    return f'unknown function {namespace}.{name}(){_suggest(namespace, name, extra)}'


def _arity_matches(entry: dict[str, Any], argc: int) -> bool:
    if entry['variadic']:
        # The trailing variadic parameter absorbs zero or more arguments, so
        # everything from "all the fixed params" upward is legal.
        return argc >= len(entry['params']) - 1
    return argc == len(entry['params'])


def _arity_error(namespace: str, name: str, entries: list[dict[str, Any]], argc: int) -> str:
    arities = sorted({len(e['params']) for e in entries})
    if any(e['variadic'] for e in entries):
        expected = f'at least {min(arities) - 1}'
    elif len(arities) == 1:
        expected = str(arities[0])
    else:
        expected = ' or '.join(str(a) for a in arities)
    return f'{namespace}.{name}() takes {expected} argument(s), got {argc}'


def check_call(namespace: str, name: str, argc: int) -> None:
    """Validate that `namespace.name(...)` names a real overload with a legal
    argument count. Context-independent, so it can run the moment the call is
    written — a `std.*` node doesn't yet know whether it will end up in a
    filter or in a pointer default."""
    entries = overloads(namespace, name)
    if not entries:
        raise InterfaceError(unknown_function_message(namespace, name))

    if not any(_arity_matches(e, argc) for e in entries):
        raise InterfaceError(_arity_error(namespace, name, entries, argc))


def check_context(namespace: str, name: str, argc: int, context: str) -> None:
    """Validate that a known-good call is admissible where it's being used.

    Runs at render time rather than call time, because the same `std.*`
    expression object is legal in one context and not the other. Judged
    against the overloads the caller actually selected by arity, so a name
    with mixed overloads isn't rejected on the strength of one it didn't use.
    """
    matching = [e for e in overloads(namespace, name) if _arity_matches(e, argc)]
    if not matching:
        return  # check_call already reported this

    if context == CONTEXT_DEFAULT:
        if all(e['aggregate'] for e in matching):
            raise InterfaceError(
                f'{namespace}.{name}() is an aggregate and cannot be used as a pointer default — '
                'a default is evaluated per row, with no set to aggregate over'
            )
        if all(e['returns_set'] for e in matching):
            raise InterfaceError(
                f'{namespace}.{name}() returns a set and cannot be used as a pointer default — '
                'a default must produce exactly one value'
            )
        return

    # Expression context. Note that volatility alone is deliberately *not*
    # disqualifying here: `filter .expires_at > std.datetime_current()` is
    # both volatile and completely legitimate. Only a function that writes is
    # rejected, because it would fire once per row as a side effect of what
    # the caller wrote as a predicate.
    if all(e['volatility'] == 'modifying' for e in matching):
        raise InterfaceError(
            f'{namespace}.{name}() modifies database state and cannot be used in a query expression — '
            'it would be evaluated once per row'
        )
