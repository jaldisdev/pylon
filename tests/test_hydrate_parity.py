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

"""`pylon._core.hydrate` must decode exactly what `pylon.query.deserialize`
decodes.

The native walk exists for speed; the Python one is the readable statement of
what the decode contract *is*. Two implementations of one contract only stay
in agreement if something checks, so every case here runs both over the same
rows and compares — including the parts easiest to get subtly wrong in a
port: the `__type__` discriminator skip, polymorphic class resolution from
the per-row type, `__pylon_saved__` excluding multilinks, and unhydrated
`LinkSet` placeholders for multilinks the shape never asked for.

Note the deliberate absence of ``from __future__ import annotations``. These
schema classes must be declared at module scope with *real* annotation
objects: as string annotations, a `MultiLink[Tag]` referring to a
function-local `Tag` does not resolve, and the walker quietly produces a
schema with no multilink at all — which both implementations would then agree
on, passing the test while testing nothing.
"""

import enum as _enum
import sys
from pathlib import Path

import pytest

sys.path.insert(0, str(Path(__file__).parent.parent))

import pylon.schema as pylon
from pylon.datatypes import LinkSet, NamedTupleValue, PylonSet
from pylon.query import _decode, hydration_registry
from pylon.schema import Link, MultiLink
from pylon.schema._registry import clear as clear_registry
from pylon.schema._registry import snapshot
from pylon.schema._walker import walk

# ── Schema under test ───────────────────────────────────────────────────────
# Registered once at import; `_ensure_registered` re-registers after any test
# that clears the registry.


@pylon.enum('RED', 'BLUE')
class Colour(pylon.Enum):
    pass


@pylon.type(module='m', name='Tag')
class Tag:
    label: str


@pylon.type(module='m', name='Author')
class Author:
    name: str


@pylon.type(module='m', name='Post')
class Post:
    title: str
    shade: Colour
    author: Link[Author] | None
    tags: MultiLink[Tag]


# Decorating the classes above registered them in the process-global class
# registry as a side effect of import. Undo that immediately: this module's
# schema must not be visible to any other test module, and pytest imports all
# of them before running anything. `__pylon_config__` stays on the classes,
# so `_ensure_registered` can put them back per test.
clear_registry()


@pytest.fixture(autouse=True)
def _ensure_registered():
    """Install this module's schema for the duration of one test, then put
    the registry back exactly as it was.

    Both directions matter: other modules' classes must not leak in (they
    would change what `walk()` compiles against), and these must not leak
    out (`test_schema.py` asserts on the global registry's contents).
    """
    from pylon.schema import _registry

    saved = {
        name: list(getattr(_registry, name))
        for name in ('_types', '_enums', '_custom_scalars', '_named_tuples', '_functions', '_signals')
    }
    clear_registry()
    _registry.register_enum(Colour)
    for cls in (Tag, Author, Post):
        _registry.register_type(cls)
    try:
        yield
    finally:
        clear_registry()
        for name, values in saved.items():
            getattr(_registry, name).extend(values)


def _compile(pyql: str):
    from pylon.query import compile as pyql_compile

    return pyql_compile(pyql, schema=walk(*snapshot(), []))


def _python_registry() -> dict[str, type]:
    """The plain dict `deserialize` takes — the shape `_hydrate` used to
    build per call."""
    from pylon.schema import schema_snapshot
    from pylon.schema._registry import named_tuples_snapshot

    types, enums, _ = schema_snapshot()
    registry: dict[str, type] = {t.__name__: t for t in types}
    for nt in named_tuples_snapshot():
        mod = getattr(nt, '__pylon_module__', 'default')
        registry[f'{mod}::{nt.__name__}'] = nt
    for en in enums:
        mod = getattr(en, '__pylon_module__', None) or (en.__module__ or 'default').rpartition('.')[-1] or 'default'
        registry[en.__name__] = en
        registry[f'{mod}::{en.__name__}'] = en
    return registry


def _describe(value):
    """A comparable rendering of a decoded value.

    Decoded `@pylon.type` instances define no `__eq__`, so comparing them
    directly compares identity and every assertion would pass vacuously.
    This reduces a value to its class name plus its instance dict,
    recursively — which is what the two implementations actually have to
    agree on. `LinkSet` is checked before `PylonSet`, and `PylonSet` before
    `list`, because each is a subclass of the next.
    """
    if isinstance(value, LinkSet):
        return ('LinkSet', value.is_hydrated, [_describe(v) for v in (value if value.is_hydrated else ())])
    if isinstance(value, PylonSet):
        return ('PylonSet', [_describe(v) for v in value])
    if isinstance(value, NamedTupleValue):
        return ('NamedTupleValue', tuple(_describe(v) for v in value))
    if isinstance(value, _enum.Enum):
        return ('Enum', type(value).__name__, value.value)
    if isinstance(value, dict):
        return {k: _describe(v) for k, v in value.items()}
    if isinstance(value, tuple):
        return tuple(_describe(v) for v in value)
    if isinstance(value, list):
        return [_describe(v) for v in value]
    if hasattr(value, '__pylon_type__'):
        return (type(value).__name__, {k: _describe(v) for k, v in sorted(value.__dict__.items())})
    return value


def assert_parity(rows: list, compiled):
    from pylon._core import hydrate

    native = hydrate(rows, compiled, hydration_registry())
    reference = [_decode(row, compiled.shape, _python_registry()) for row in rows]
    assert [_describe(v) for v in native] == [_describe(v) for v in reference]
    return native


class TestObjects:
    def test_a_plain_object_row(self):
        compiled = _compile('select m::Tag { label }')
        native = assert_parity([('m::Tag', 'red'), ('m::Tag', 'blue')], compiled)
        assert native[0].label == 'red'
        assert native[0].__pylon_type__ == 'm::Tag'

    def test_a_nested_object(self):
        compiled = _compile('select m::Post { title, author: { name } }')
        assert_parity([('m::Post', 'Hello', ('m::Author', 'Ada'))], compiled)

    def test_a_null_nested_object(self):
        compiled = _compile('select m::Post { title, author: { name } }')
        assert_parity([('m::Post', 'Hello', None)], compiled)

    def test_saved_shadow_copy_matches(self):
        compiled = _compile('select m::Tag { label }')
        native = assert_parity([('m::Tag', 'red')], compiled)
        assert native[0].__dict__['__pylon_saved__'] == {'label': 'red'}

    def test_an_explicitly_requested_type_discriminator_is_kept(self):
        """The auto-injected `__type__` at position 0 is skipped, but one the
        user asked for sits at a later position and must survive."""
        compiled = _compile('select m::Tag { __type__, label }')
        native = assert_parity([('m::Tag', 'm::Tag', 'red')], compiled)
        assert native[0].__dict__['__type__'] == 'm::Tag'


class TestMultiLinks:
    def test_a_requested_multilink_is_hydrated(self):
        compiled = _compile('select m::Post { title, tags: { label } }')
        rows = [('m::Post', 'Hello', [('m::Tag', 'red'), ('m::Tag', 'blue')])]
        native = assert_parity(rows, compiled)
        assert native[0].tags.is_hydrated
        assert [t.label for t in native[0].tags] == ['red', 'blue']

    def test_an_unrequested_multilink_gets_an_unhydrated_placeholder(self):
        compiled = _compile('select m::Post { title }')
        native = assert_parity([('m::Post', 'Hello')], compiled)
        assert isinstance(native[0].tags, LinkSet)
        assert not native[0].tags.is_hydrated

    def test_multilinks_are_excluded_from_the_saved_copy(self):
        compiled = _compile('select m::Post { title, tags: { label } }')
        native = assert_parity([('m::Post', 'Hi', [('m::Tag', 'red')])], compiled)
        assert 'tags' not in native[0].__dict__['__pylon_saved__']

    def test_an_empty_multilink(self):
        compiled = _compile('select m::Post { title, tags: { label } }')
        native = assert_parity([('m::Post', 'Hello', None)], compiled)
        assert native[0].tags.is_hydrated
        assert len(native[0].tags) == 0


class TestEnums:
    def test_an_enum_property(self):
        compiled = _compile('select m::Post { shade }')
        native = assert_parity([('m::Post', 'RED')], compiled)
        assert native[0].shade is Colour.RED

    def test_a_null_enum(self):
        compiled = _compile('select m::Post { shade }')
        assert_parity([('m::Post', None)], compiled)


class TestFreeObjectsAndScalars:
    def test_a_free_object_has_no_discriminator(self):
        compiled = _compile('select { a := 1, b := 2 }')
        assert_parity([(1, 2)], compiled)

    def test_a_bare_scalar(self):
        # A bare `select <expr>` compiles to a Scalar node at position 0, so
        # the row is still a one-element tuple, not the value itself.
        compiled = _compile('select 1 + 1')
        native = assert_parity([(2,)], compiled)
        assert native == [2]


class TestRegistryMemo:
    def test_the_memo_is_reused_when_nothing_changed(self):
        assert hydration_registry() is hydration_registry()

    def test_a_new_registration_invalidates_the_memo(self):
        """Registering a type after the registry was built must not serve the
        stale one, or the new class would silently decode as a plain dict."""
        built = hydration_registry()

        @pylon.type(module='m', name='Latecomer')
        class Latecomer:
            note: str

        assert hydration_registry() is not built
        compiled = _compile('select m::Latecomer { note }')
        native = assert_parity([('m::Latecomer', 'x')], compiled)
        assert type(native[0]).__name__ == 'Latecomer'

    def test_clearing_the_registry_invalidates_the_memo(self):
        built = hydration_registry()
        clear_registry()
        assert hydration_registry() is not built
