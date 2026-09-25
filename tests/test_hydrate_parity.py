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
import uuid as _uuid
from pathlib import Path

import pytest

sys.path.insert(0, str(Path(__file__).parent.parent))

import pylon.schema as pylon
from pylon.datatypes import LinkSet, NamedTupleValue, Object, PylonSet
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
    palette: pylon.Array[Colour] | None
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


def _rowset(rows: list):
    """Build a `RowSet` — the undecoded form the driver and the cache both
    produce — from plain Python rows, by round-tripping them through the
    cache's own encoder."""
    import tempfile

    from pylon._core import cache_get, cache_init, cache_put

    global _cache_ready
    if not _cache_ready:
        cache_init(tempfile.mkdtemp(), 64)
        _cache_ready = True
    cache_put('parity', ['t'], rows)
    return cache_get('parity')


_cache_ready = False


def _id(n: int) -> _uuid.UUID:
    """A stable id for position 1 of a schema-object row.

    Every object shape now carries an `id` the query never named — the upstream engine does
    the same — so a hand-written row has to supply one right after the type
    discriminator.
    """
    return _uuid.UUID(int=n)


def assert_parity(rows: list, compiled):
    """Run both implementations over the same rows and compare.

    The native walk consumes the undecoded `RowSet`; the reference consumes
    ordinary Python values. That difference is deliberate — it means this
    also checks that the two representations of a row agree, not just that
    the two walks do.
    """
    from pylon._core import hydrate

    native = hydrate(_rowset(rows), compiled, hydration_registry())
    reference = [_decode(row, compiled.shape, _python_registry()) for row in rows]
    assert [_describe(v) for v in native] == [_describe(v) for v in reference]
    return native


class TestObjects:
    def test_a_plain_object_row(self):
        compiled = _compile('select m::Tag { label }')
        native = assert_parity([('m::Tag', _id(1), 'red'), ('m::Tag', _id(2), 'blue')], compiled)
        assert native[0].label == 'red'
        assert native[0].__pylon_type__ == 'm::Tag'

    def test_a_nested_object(self):
        compiled = _compile('select m::Post { title, author: { name } }')
        assert_parity([('m::Post', _id(1), 'Hello', ('m::Author', _id(2), 'Ada'))], compiled)

    def test_a_null_nested_object(self):
        compiled = _compile('select m::Post { title, author: { name } }')
        assert_parity([('m::Post', _id(1), 'Hello', None)], compiled)

    def test_saved_shadow_copy_matches(self):
        compiled = _compile('select m::Tag { label }')
        native = assert_parity([('m::Tag', _id(1), 'red')], compiled)
        assert native[0].__dict__['__pylon_saved__'] == {'id': _id(1), 'label': 'red'}

    def test_an_explicitly_requested_type_discriminator_is_kept(self):
        """The auto-injected `__type__` at position 0 is skipped, but one the
        user asked for sits at a later position and must survive."""
        compiled = _compile('select m::Tag { __type__, label }')
        native = assert_parity([('m::Tag', _id(1), 'm::Tag', 'red')], compiled)
        assert native[0].__dict__['__type__'] == 'm::Tag'


class TestUnfetchedPointers:
    """A pointer the shape skipped must not read as a legitimate value.

    Measured against the upstream Python client on the same kind of shape
    (`select … { value }`, reading an unselected optional property):

        o.order                    -> AttributeError: 'the upstream Object' object has
                                      no attribute 'order'
        hasattr(o, 'order')        -> False
        getattr(o, 'order', None)  -> None

    All three have to hold here too, or code that ran against the upstream engine changes
    behaviour on the way over — `getattr(o, x, None)` silently handing back a
    sentinel would be its own version of the bug this closes.
    """

    def test_an_unselected_property_raises_rather_than_reading_as_none(self):
        compiled = _compile('select m::Post { title }')
        native = assert_parity([('m::Post', _id(1), 'Hello')], compiled)
        post = native[0]

        assert hasattr(post, 'title')
        assert not hasattr(post, 'shade')
        assert getattr(post, 'shade', None) is None
        with pytest.raises(AttributeError, match="no attribute 'shade'"):
            getattr(post, 'shade')  # noqa: B009 — the point is that it raises

    def test_a_selected_null_is_still_a_null(self):
        """The distinction the upstream engine cannot draw and this one can: selected-and-null
        reads as None, unselected raises."""
        compiled = _compile('select m::Post { title, shade }')
        native = assert_parity([('m::Post', _id(1), 'Hello', None)], compiled)
        assert native[0].shade is None
        assert hasattr(native[0], 'shade')

    def test_a_constructed_instance_keeps_its_declared_defaults(self):
        """Nothing left anything out of a shape here — there was no shape."""
        post = Post(title='Hello', shade=Colour.RED)
        assert post.palette is None
        assert post.author is None


class TestMultiLinks:
    def test_a_requested_multilink_is_hydrated(self):
        compiled = _compile('select m::Post { title, tags: { label } }')
        rows = [('m::Post', _id(1), 'Hello', [('m::Tag', _id(2), 'red'), ('m::Tag', _id(3), 'blue')])]
        native = assert_parity(rows, compiled)
        assert native[0].tags.is_hydrated
        assert [t.label for t in native[0].tags] == ['red', 'blue']

    def test_an_unrequested_multilink_gets_an_unhydrated_placeholder(self):
        compiled = _compile('select m::Post { title }')
        native = assert_parity([('m::Post', _id(1), 'Hello')], compiled)
        assert isinstance(native[0].tags, LinkSet)
        assert not native[0].tags.is_hydrated

    def test_multilinks_are_excluded_from_the_saved_copy(self):
        compiled = _compile('select m::Post { title, tags: { label } }')
        native = assert_parity([('m::Post', _id(1), 'Hi', [('m::Tag', _id(2), 'red')])], compiled)
        assert 'tags' not in native[0].__dict__['__pylon_saved__']

    def test_an_empty_multilink(self):
        compiled = _compile('select m::Post { title, tags: { label } }')
        native = assert_parity([('m::Post', _id(1), 'Hello', None)], compiled)
        assert native[0].tags.is_hydrated
        assert len(native[0].tags) == 0


class TestEnums:
    def test_an_enum_property(self):
        compiled = _compile('select m::Post { shade }')
        native = assert_parity([('m::Post', _id(1), 'RED')], compiled)
        assert native[0].shade is Colour.RED

    def test_a_null_enum(self):
        compiled = _compile('select m::Post { shade }')
        assert_parity([('m::Post', _id(1), None)], compiled)

    def test_an_enum_array_property(self):
        compiled = _compile('select m::Post { palette }')
        native = assert_parity([('m::Post', _id(1), ['RED', 'BLUE'])], compiled)
        assert list(native[0].palette) == [Colour.RED, Colour.BLUE]

    def test_an_unset_enum_array_property(self):
        compiled = _compile('select m::Post { palette }')
        native = assert_parity([('m::Post', _id(1), None)], compiled)
        assert native[0].palette is None


class TestFreeObjectsAndScalars:
    def test_a_free_object_has_no_discriminator(self):
        compiled = _compile('select { a := 1, b := 2 }')
        native = assert_parity([(1, 2)], compiled)
        assert isinstance(native[0], Object)
        assert (native[0].a, native[0].b) == (1, 2)

    def test_a_tuple_element_holding_an_object(self):
        """The object branch reads position 0 as "this is the whole row",
        which is true of the query's root but not of a tuple's first element:
        the object used to decode against the outer tuple and pick up its
        neighbours' values."""

        compiled = _compile('with a := (select m::Author limit 1) select (a { name }, 1)')
        native = assert_parity([(('m::Author', _id(1), 'Alice'), 1)], compiled)
        author, number = native[0]
        assert (author.name, number) == ('Alice', 1)

    def test_a_named_tuple_element_holding_an_object(self):
        """jsonb has no member kind for an object, so a named tuple holding
        one is emitted as a composite -- and must still come back as the value
        a named tuple gives, not as a plain tuple."""

        compiled = _compile('with a := (select m::Author limit 1) select (who := a { name }, n := 1)')
        native = assert_parity([(('m::Author', _id(1), 'Alice'), 1)], compiled)
        assert isinstance(native[0], NamedTupleValue)
        assert (native[0].who.name, native[0].n) == ('Alice', 1)

    def test_a_named_tuple_of_scalars_still_travels_as_jsonb(self):
        """Nothing forced the composite, so the encoding is unchanged."""

        compiled = _compile('select (x := 1, y := 2)')
        assert compiled.shape['kind'] == 'named_tuple'

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
        native = assert_parity([('m::Latecomer', _id(1), 'x')], compiled)
        assert type(native[0]).__name__ == 'Latecomer'

    def test_clearing_the_registry_invalidates_the_memo(self):
        built = hydration_registry()
        clear_registry()
        assert hydration_registry() is not built
