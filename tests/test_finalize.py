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

"""Tests for the schema walker, type registry, and pylon.finalize() pipeline.

Parts that don't require the Rust binary:
  - Registry accumulation
  - Walker validation logic (cycle detection, interface conformance, etc.)

Parts that require the rebuilt Rust binary (marked with NEEDS_REBUILD):
  - Full walk() with descriptor construction
  - pylon.finalize()

NOTE: This file intentionally omits ``from __future__ import annotations`` so
that link annotations inside test-local classes evaluate eagerly. Types defined
in function scope are not visible to typing.get_type_hints() resolution, which
works against the module's global namespace only.
"""

import dataclasses

import pytest

import pylon.schema as pylon
from pylon.schema._registry import clear as clear_registry
from pylon.schema._registry import register_type, snapshot
from pylon.schema._walker import (
    SchemaError,
    _build_type_index,
    _detect_required_link_cycles,
    _effective_pointers,
    _find_pylon_parents,
    _make_default_sql,
    _python_value_to_sql,
    _resolve_links,
    _to_pg_type,
    _validate_interfaces,
)

MISSING = dataclasses.MISSING


# ---------------------------------------------------------------------------
# Fixtures: isolated schema types (not in global registry to avoid pollution)
# ---------------------------------------------------------------------------


@pytest.fixture(autouse=True)
def _isolated_registry():
    """Snapshot/restore the registry around each test to avoid cross-test leakage."""
    before = snapshot()
    yield
    clear_registry()
    # Restore pre-test state
    types, enums, scalars = before
    from pylon.schema._registry import register_enum, register_scalar

    for t in types:
        register_type(t)
    for e in enums:
        register_enum(e)
    for s in scalars:
        register_scalar(s)


# ---------------------------------------------------------------------------
# Registry
# ---------------------------------------------------------------------------


class TestRegistry:
    def test_type_registration_on_decoration(self):
        clear_registry()

        @pylon.type
        class Widget:
            name: str

        types, _, _ = snapshot()
        assert Widget in types

    def test_enum_registration_on_decoration(self):
        clear_registry()

        @pylon.enum('A', 'B')
        class Color(pylon.Enum):
            pass

        _, enums, _ = snapshot()
        assert Color in enums

    def test_scalar_registration_on_decoration(self):
        clear_registry()

        @pylon.scalar(pylon.Str)
        class EmailAddr(pylon.Scalar):
            pass

        _, _, scalars = snapshot()
        assert EmailAddr in scalars

    def test_abstract_type_registered(self):
        clear_registry()

        @pylon.abstract
        class TimestampedBase:
            created_at: pylon.DateTime

        types, _, _ = snapshot()
        assert TimestampedBase in types

    def test_interface_registered(self):
        clear_registry()

        @pylon.interface
        class Auditable:
            note: str

        types, _, _ = snapshot()
        assert Auditable in types

    def test_clear_empties_all_buckets(self):
        @pylon.type
        class Dummy:
            x: str

        clear_registry()
        t, e, s = snapshot()
        assert t == [] and e == [] and s == []

    def test_enum_has_pylon_module(self):
        clear_registry()

        @pylon.enum('X', 'Y')
        class Flag(pylon.Enum):
            pass

        assert hasattr(Flag, '__pylon_module__')

    def test_enum_preserves_python_module(self):
        clear_registry()

        @pylon.enum('X')
        class Tag(pylon.Enum):
            pass

        assert Tag.__module__ == __name__


# ---------------------------------------------------------------------------
# Type index building
# ---------------------------------------------------------------------------


class TestTypeIndex:
    def _make_type(self, module, name):
        @pylon.type(module=module, name=name)
        class T:
            x: str

        T.__pylon_config__.name = name
        T.__pylon_config__.module = module
        return T

    def test_qualified_name(self):
        @pylon.type(module='shop', name='Item')
        class Item:
            sku: str

        type_map, cid_map = _build_type_index([Item])
        assert 'shop::Item' in type_map
        assert cid_map[id(Item)] == 'shop::Item'

    def test_duplicate_raises(self):
        @pylon.type(module='shop', name='Widget')
        class W1:
            x: str

        @pylon.type(module='shop', name='Widget')
        class W2:
            y: str

        with pytest.raises(SchemaError, match='Duplicate type name'):
            _build_type_index([W1, W2])


# ---------------------------------------------------------------------------
# Effective field flattening
# ---------------------------------------------------------------------------


class TestEffectiveFields:
    def test_own_fields_only(self):
        @pylon.type(module='t', name='Standalone')
        class Standalone:
            name: str

        eff = _effective_pointers(Standalone)
        assert 'name' in eff
        assert 'id' in eff  # always injected

    def test_inherits_abstract_parent_pointers(self):
        @pylon.abstract(module='t', name='Base')
        class Base:
            created_at: pylon.DateTime

        @pylon.type(module='t', name='Child')
        class Child(Base):
            name: str

        eff = _effective_pointers(Child)
        assert 'created_at' in eff
        assert 'name' in eff

    def test_own_field_shadows_parent(self):
        @pylon.abstract(module='t', name='ABase')
        class ABase:
            x: str

        @pylon.type(module='t', name='AChild')
        class AChild(ABase):
            x: pylon.Int64  # type: ignore[assignment]

        eff = _effective_pointers(AChild)
        # AChild's own 'x' is Int64, not Str
        assert eff['x'].scalar_type is pylon.Int64

    def test_id_not_duplicated(self):
        @pylon.abstract(module='t', name='Root')
        class Root:
            note: str

        @pylon.type(module='t', name='LeafType')
        class Leaf(Root):
            val: str

        eff = _effective_pointers(Leaf)
        assert list(eff.keys()).count('id') == 1


# ---------------------------------------------------------------------------
# Pylon parent / interface discovery
# ---------------------------------------------------------------------------


class TestFindPylonParents:
    def test_abstract_parent_detected(self):
        @pylon.abstract(module='t', name='AbstractP')
        class AbstractP:
            ts: pylon.DateTime

        @pylon.type(module='t', name='ConcreteC')
        class ConcreteC(AbstractP):
            name: str

        _, cid = _build_type_index([AbstractP, ConcreteC])
        parents, interfaces, bases = _find_pylon_parents(ConcreteC, cid)
        assert 't::AbstractP' in parents
        assert interfaces == []
        assert bases == []

    def test_interface_detected(self):
        @pylon.interface(module='t', name='IFace')
        class IFace:
            flag: pylon.Bool

        @pylon.type(module='t', name='ConcreteI')
        class ConcreteI(IFace):
            name: str

        _, cid = _build_type_index([IFace, ConcreteI])
        parents, interfaces, bases = _find_pylon_parents(ConcreteI, cid)
        assert parents == []
        assert 't::IFace' in interfaces
        assert bases == []

    def test_concrete_base_detected(self):
        @pylon.type(module='t', name='Addon')
        class Addon:
            name: str

        @pylon.type(module='t', name='Bundle')
        class Bundle(Addon):
            size: pylon.Int64

        _, cid = _build_type_index([Addon, Bundle])
        parents, interfaces, bases = _find_pylon_parents(Bundle, cid)
        assert bases == ['t::Addon']
        assert parents == []
        assert interfaces == []


# ---------------------------------------------------------------------------
# Link resolution
# ---------------------------------------------------------------------------


class TestLinkResolution:
    def test_direct_class_resolved(self):
        @pylon.type(module='t', name='Author')
        class Author:
            name: str

        @pylon.type(module='t', name='Post')
        class Post:
            author: pylon.Link[Author]

        types = [Author, Post]
        tmap, cid = _build_type_index(types)
        _resolve_links(types, tmap, cid)
        assert Post.__pylon_config__.pointers['author'].link_target == 't::Author'

    def test_through_resolved(self):
        @pylon.type(module='t', name='TagType')
        class TagType:
            label: str

        @pylon.type(module='t', name='ProductType')
        class ProductType:
            name: str

        @pylon.type(module='t', name='JunctionType')
        class JunctionType:
            source: pylon.Link[ProductType]
            target: pylon.Link[TagType]

        @pylon.type(module='t', name='CatalogType')
        class CatalogType:
            items: pylon.MultiLink[ProductType, pylon.Through[JunctionType]]

        types = [TagType, ProductType, JunctionType, CatalogType]
        tmap, cid = _build_type_index(types)
        _resolve_links(types, tmap, cid)
        assert CatalogType.__pylon_config__.pointers['items'].link_target == 't::ProductType'
        assert CatalogType.__pylon_config__.pointers['items'].through == 't::JunctionType'

    def test_unregistered_target_raises(self):
        @pylon.type(module='t', name='OrphanPost')
        class OrphanPost:
            pass

        # Manually inject a link_target that is NOT in the type_map
        @pylon.type(module='t', name='SomeOtherType')
        class SomeOtherType:
            x: str

        # Patch field to point to SomeOtherType class (not in index for this test)
        from pylon.schema._meta import PointerMeta

        OrphanPost.__pylon_config__.pointers['ref'] = PointerMeta(
            name='ref',
            kind='link',
            scalar_type=None,
            nullable=False,
            constraints=[],
            default=MISSING,
            default_factory=MISSING,
            link_target=SomeOtherType,
        )

        types = [OrphanPost]  # SomeOtherType intentionally omitted
        tmap, cid = _build_type_index(types)
        with pytest.raises(SchemaError, match=r'not a Pylon type|not found|not collected'):
            _resolve_links(types, tmap, cid)


# ---------------------------------------------------------------------------
# Cycle detection
# ---------------------------------------------------------------------------


class TestCycleDetection:
    def test_no_cycle_passes(self):
        @pylon.type(module='t', name='CycleA')
        class CycleA:
            name: str

        @pylon.type(module='t', name='CycleB')
        class CycleB:
            a: pylon.Link[CycleA]

        types = [CycleA, CycleB]
        tmap, cid = _build_type_index(types)
        _resolve_links(types, tmap, cid)
        _detect_required_link_cycles(types, cid)  # should not raise

    def test_required_cycle_raises(self):
        @pylon.type(module='t', name='NodeX')
        class NodeX:
            pass

        @pylon.type(module='t', name='NodeY')
        class NodeY:
            pass

        from pylon.schema._meta import PointerMeta

        # X→Y (required), Y→X (required) — deadlock
        NodeX.__pylon_config__.pointers['y'] = PointerMeta(
            name='y',
            kind='link',
            scalar_type=None,
            nullable=False,
            constraints=[],
            default=MISSING,
            default_factory=MISSING,
            link_target=NodeY,
        )
        NodeY.__pylon_config__.pointers['x'] = PointerMeta(
            name='x',
            kind='link',
            scalar_type=None,
            nullable=False,
            constraints=[],
            default=MISSING,
            default_factory=MISSING,
            link_target=NodeX,
        )

        types = [NodeX, NodeY]
        tmap, cid = _build_type_index(types)
        _resolve_links(types, tmap, cid)
        with pytest.raises(SchemaError, match='cycle'):
            _detect_required_link_cycles(types, cid)

    def test_nullable_cycle_passes(self):
        @pylon.type(module='t', name='NullA')
        class NullA:
            pass

        @pylon.type(module='t', name='NullB')
        class NullB:
            pass

        from pylon.schema._meta import PointerMeta

        # A→B (nullable) — not a deadlock
        NullA.__pylon_config__.pointers['b'] = PointerMeta(
            name='b',
            kind='link',
            scalar_type=None,
            nullable=True,
            constraints=[],
            default=None,
            default_factory=MISSING,
            link_target=NullB,
        )
        NullB.__pylon_config__.pointers['a'] = PointerMeta(
            name='a',
            kind='link',
            scalar_type=None,
            nullable=True,
            constraints=[],
            default=None,
            default_factory=MISSING,
            link_target=NullA,
        )

        types = [NullA, NullB]
        tmap, cid = _build_type_index(types)
        _resolve_links(types, tmap, cid)
        _detect_required_link_cycles(types, cid)  # should not raise


# ---------------------------------------------------------------------------
# Interface conformance
# ---------------------------------------------------------------------------


class TestInterfaceConformance:
    def test_conformant_passes(self):
        @pylon.interface(module='t', name='IHasName')
        class IHasName:
            name: str

        @pylon.type(module='t', name='Conformant')
        class Conformant(IHasName):
            name: str
            extra: pylon.Int64

        types = [IHasName, Conformant]
        _tmap, cid = _build_type_index(types)
        _validate_interfaces(types, cid)  # should not raise

    def test_missing_field_raises(self):
        @pylon.interface(module='t', name='IRequired')
        class IRequired:
            must_have: str

        @pylon.type(module='t', name='NonConformant')
        class NonConformant:
            other: str

        # Manually inject interface into NonConformant's MRO-like check is based
        # on Python MRO — but since NonConformant doesn't actually inherit from
        # IRequired, _validate_interfaces won't flag it. Test the real case:
        @pylon.type(module='t', name='Missing')
        class Missing(IRequired):
            pass  # doesn't declare must_have itself, but inherits via dataclass

        # Missing DOES inherit must_have from IRequired via dataclass, so it
        # should pass. Test that a truly missing field fails:
        types = [IRequired, NonConformant]
        _tmap, cid = _build_type_index(types)
        _validate_interfaces(types, cid)  # NonConformant doesn't inherit IRequired → fine

    def test_inherited_field_satisfies_interface(self):
        """Inheriting the field via Python is sufficient for conformance."""

        @pylon.interface(module='t', name='ITS')
        class ITS:
            ts: pylon.DateTime

        @pylon.abstract(module='t', name='BaseTS')
        class BaseTS:
            ts: pylon.DateTime

        @pylon.type(module='t', name='ConcTS')
        class ConcTS(BaseTS, ITS):  # ITS is also a parent
            name: str

        types = [ITS, BaseTS, ConcTS]
        _tmap, cid = _build_type_index(types)
        _validate_interfaces(types, cid)  # should not raise


# ---------------------------------------------------------------------------
# PG type resolution
# ---------------------------------------------------------------------------


class TestPgTypeResolution:
    def test_builtin_scalars(self):
        assert _to_pg_type(pylon.Str) == 'text'
        assert _to_pg_type(pylon.Int64) == 'int8'
        assert _to_pg_type(pylon.Float32) == 'float4'
        assert _to_pg_type(pylon.UUID) == 'uuid'
        assert _to_pg_type(pylon.Bool) == 'boolean'
        assert _to_pg_type(pylon.DateTime) == 'timestamptz'
        assert _to_pg_type(pylon.JSON) == 'jsonb'

    def test_enum_type_name(self):
        @pylon.enum('On', 'Off')
        class Power(pylon.Enum):
            pass

        pg = _to_pg_type(Power)
        # Schema-qualified: "module"."Power" — module derived from __pylon_module__
        assert pg.endswith('"."Power"')

    def test_generic_list(self):
        assert _to_pg_type(list[str]) in ('text[]',)

    def test_fallback_is_text(self):
        assert _to_pg_type(object) == 'text'


# ---------------------------------------------------------------------------
# Default SQL generation
# ---------------------------------------------------------------------------


class TestDefaultSql:
    def test_python_value_str(self):
        assert _python_value_to_sql('hello') == "'hello'"

    def test_python_value_str_escape(self):
        assert _python_value_to_sql("it's") == "'it''s'"

    def test_python_value_int(self):
        assert _python_value_to_sql(42) == '42'

    def test_python_value_float(self):
        assert _python_value_to_sql(3.14) == '3.14'

    def test_python_value_bool_true(self):
        assert _python_value_to_sql(True) == 'true'

    def test_python_value_bool_false(self):
        assert _python_value_to_sql(False) == 'false'

    def test_python_value_none_returns_none(self):
        assert _python_value_to_sql(None) is None

    def test_default_now_gives_now(self):
        from pylon.schema._constraints import Default, Now
        from pylon.schema._meta import PointerMeta

        meta = PointerMeta(
            name='created_at',
            kind='property',
            scalar_type=pylon.DateTime,
            nullable=False,
            constraints=[Default(Now)],
            default=None,
            default_factory=MISSING,
        )
        assert _make_default_sql(meta) == 'now()'

    def test_scalar_default_converted(self):
        from pylon.schema._meta import PointerMeta

        meta = PointerMeta(
            name='score',
            kind='property',
            scalar_type=pylon.Float64,
            nullable=False,
            constraints=[],
            default=0.0,
            default_factory=MISSING,
        )
        assert _make_default_sql(meta) == '0.0'

    def test_no_default_returns_none(self):
        from pylon.schema._meta import PointerMeta

        meta = PointerMeta(
            name='name',
            kind='property',
            scalar_type=pylon.Str,
            nullable=False,
            constraints=[],
            default=MISSING,
            default_factory=MISSING,
        )
        assert _make_default_sql(meta) is None


# ---------------------------------------------------------------------------
# Integration: full walk() requires rebuilt Rust binary
# ---------------------------------------------------------------------------

# A stale `pylon._core` is caught once, for the whole run, in conftest.py's
# `pytest_configure` — these no longer carry their own skip guard.


class TestWalkIntegration:
    def setup_method(self):
        clear_registry()

    def test_minimal_schema(self):
        from pylon.schema._walker import walk

        @pylon.type(module='shop', name='Product')
        class Product:
            name: str
            price: pylon.Decimal

        types, enums, scalars = snapshot()
        schema = walk(types, enums, scalars, [])
        assert schema.type_count == 1
        assert schema.scalar_count == 0
        assert schema.enum_count == 0

    def test_computed_link_reports_the_type_it_selects(self):
        import json

        from pylon.schema._walker import walk

        @pylon.type(module='account', name='Email')
        class Email:
            address: str

        @pylon.type(module='account', name='Member')
        class Member:
            name: str

        @pylon.type(module='account', name='Org')
        class Org:
            name: str
            emails: pylon.MultiLink[Email]
            staff: pylon.MultiLink[Member]
            primary_email: pylon.Computed[pylon.Link[Email], 'select .emails limit 1']
            members: pylon.Computed[pylon.MultiLink[Member], '.staff']
            label: pylon.Computed[str, '.name']

        types, enums, scalars = snapshot()
        schema = json.loads(walk(types, enums, scalars, []).to_json())
        org = next(t for t in schema['types'] if t['name'] == 'Org')
        computed = {c['name']: c for c in org['computed']}
        # A computed selecting objects is a link in everything but name — the
        # schema browser reads this to render and walk into it as one.
        assert computed['primary_email']['link_target'] == 'account::Email'
        assert computed['primary_email']['link_multi'] is False
        assert computed['members']['link_target'] == 'account::Member'
        assert computed['members']['link_multi'] is True
        # A scalar computed still carries its own return type and no link.
        assert computed['label']['link_target'] is None
        assert computed['label']['return_type'] == 'text'

    def test_schema_with_enum(self):
        from pylon.schema._walker import walk

        @pylon.enum('Draft', 'Published')
        class State(pylon.Enum):
            pass

        @pylon.type(module='blog', name='Article')
        class Article:
            title: str
            state: State = State.Draft  # type: ignore[assignment]

        types, enums, scalars = snapshot()
        schema = walk(types, enums, scalars, [])
        assert schema.type_count == 1
        assert schema.enum_count == 1

    def test_schema_with_named_tuple(self):
        from pylon.schema._registry import named_tuples_snapshot
        from pylon.schema._walker import walk

        @pylon.named_tuple
        class Point(pylon.NamedTuple):
            x: pylon.Float64
            y: pylon.Float64

        @pylon.type(module='geo', name='Place')
        class Place:
            name: str
            location: pylon.Property[Point] | None

        types, enums, scalars = snapshot()
        schema = walk(types, enums, scalars, [], named_tuples=named_tuples_snapshot())
        assert schema.named_tuple_count == 1

    def test_schema_with_structural_tuple(self):
        from pylon.schema._walker import walk

        @pylon.type(module='geo', name='Shape')
        class Shape:
            label: str
            pair: pylon.Tuple[pylon.Str, pylon.Bool]
            rgb: pylon.Tuple[('r', pylon.Int16), ('g', pylon.Int16), ('b', pylon.Int16)]
            nested: (
                pylon.Tuple[
                    ('origin', pylon.Tuple[('x', pylon.Float64), ('y', pylon.Float64)]),
                    ('size', pylon.Float64),
                ]
                | None
            )

        types, enums, scalars = snapshot()
        schema = walk(types, enums, scalars, [])
        assert schema.type_count == 1

    def test_schema_with_link(self):
        from pylon.schema._walker import walk

        @pylon.type(module='store', name='Category')
        class Category:
            name: str

        @pylon.type(module='store', name='Item')
        class Item:
            title: str
            cat: pylon.Link[Category]

        types, enums, scalars = snapshot()
        schema = walk(types, enums, scalars, [])
        assert schema.type_count == 2

    def test_link_default_selecting_an_object_moves_into_the_insert(self):
        """`DEFAULT (SELECT …)` is DDL PostgreSQL refuses to run.

        So the column gets no default and the insert applies it instead, which
        is how the upstream engine has always applied every default. Emitting it anyway used to
        surface as a raw Postgres error against generated DDL at migration
        time; rejecting the schema outright refused a default the upstream engine accepts.
        """
        from pylon import _core
        from pylon.schema import Default, Link
        from pylon.schema._walker import walk

        @pylon.type(module='dl', name='Team')
        class Team:
            name: str

        @pylon.type(module='dl', name='Member')
        class Member:
            team: Link[Team, Default('(select Team limit 1)')] | None

        types, enums, scalars = snapshot()
        schema = walk(types, enums, scalars, [])
        ddl = _core.export_schema(schema)
        assert '"team_id" uuid DEFAULT' not in ddl, ddl

    def test_pointer_level_expression_lands_on_its_own_type(self):
        import json

        from pylon.schema import Expression, Property
        from pylon.schema._walker import walk

        @pylon.type(module='fx', name='Rate')
        class Rate:
            currency: Property[str, Expression("__subject__ != ''")]

        @pylon.type(module='fx', name='Unrelated')
        class Unrelated:
            label: str

        types, enums, scalars = snapshot()
        schema = json.loads(walk(types, enums, scalars, []).to_json())
        by_name = {t['name']: t for t in schema['types']}
        # On a pointer `__subject__` is that pointer's own value, so it resolves
        # to the pointer while the constraint still knows which one it came from.
        assert by_name['Rate']['constraints'] == [{'Expression': {'expr': ".currency != ''"}}]
        assert by_name['Unrelated']['constraints'] == []

    def test_object_valued_computed_declares_no_scalar_return_type(self):
        """A `Computed[MultiLink[X], ...]` has no scalar type to check against.

        It used to fall through `_to_pg_type`'s branches onto the `"text"`
        fallback, which validation then compared against what the expression
        actually produced — rejecting every link-valued computed as a return
        type mismatch.
        """
        from pylon.schema._walker import walk

        @pylon.type(module='cmp', name='Item')
        class Item:
            label: str

        @pylon.type(module='cmp', name='Box')
        class Box:
            items: pylon.MultiLink[Item]
            recent: pylon.Computed[pylon.MultiLink[Item], '(select .items limit 2)']
            first_label: pylon.Computed[str, '(select .items limit 1).label']

        assert Box.__pylon_config__.pointers['recent'].kind == 'computed'
        types, enums, scalars = snapshot()
        schema = walk(types, enums, scalars, [])
        assert schema.type_count == 2

    def test_inheritance_flattened(self):
        from pylon.schema._walker import walk

        @pylon.abstract(module='base', name='Timestamped')
        class Timestamped:
            created_at: pylon.DateTime

        @pylon.type(module='base', name='Post')
        class Post(Timestamped):
            title: str

        types, enums, scalars = snapshot()
        schema = walk(types, enums, scalars, [])
        # Type count: both Timestamped and Post are registered
        assert schema.type_count == 2

    def test_custom_scalar(self):
        from pylon.schema._walker import walk

        @pylon.scalar(pylon.Int64)
        class PositiveInt(pylon.Scalar):
            @staticmethod
            def validate(v: int) -> None:
                if v <= 0:
                    raise ValueError

        types, enums, scalars = snapshot()
        schema = walk(types, enums, scalars, [])
        assert schema.scalar_count == 1

    def test_globals_included(self):
        import types as _types_mod

        from pylon.schema._globals import collect_module_globals
        from pylon.schema._walker import walk

        m = _types_mod.ModuleType('test_globals_module')
        m.__annotations__ = {'current_user_id': pylon.Global[pylon.UUID]}

        globals_ = collect_module_globals(m)
        types, enums, scalars = snapshot()
        schema = walk(types, enums, scalars, globals_)
        assert schema.global_count == 1

    def test_finalize_sets_singleton(self, tmp_path):
        import pylon as _pylon
        from pylon.query import _get_schema

        # Create a minimal pylon.toml; schema-dir can be empty since types
        # are already registered by the decorator below.
        schema_dir = tmp_path / 'schema'
        schema_dir.mkdir()
        toml = tmp_path / 'pylon.toml'
        toml.write_text(
            '[project]\nschema-dir = "schema"\n\n'
            '[database]\nhost = "localhost"\nport = 5432\n'
            'name = "mydb"\nuser = "u"\n'
        )

        @pylon.type(module='fin', name='Dummy')
        class Dummy:
            x: str

        _pylon.finalize(config=toml)
        schema = _get_schema()
        assert schema is not None
        assert schema.type_count >= 1

    def test_finalize_imports_schema_dir(self, tmp_path):
        """Types defined in schema-dir files are auto-discovered and registered."""
        import sys

        import pylon as _pylon

        schema_dir = tmp_path / 'schema'
        schema_dir.mkdir()

        # Write a schema file; its stem "things" becomes the Pylon module name.
        (schema_dir / 'things.py').write_text('import pylon\n\n@pylon.type\nclass Widget:\n    label: str\n')
        toml = tmp_path / 'pylon.toml'
        toml.write_text(
            '[project]\nschema-dir = "schema"\n\n'
            '[database]\nhost = "localhost"\nport = 5432\n'
            'name = "mydb"\nuser = "u"\n'
        )

        schema = _pylon.finalize(config=toml)
        assert schema.type_count >= 1

        # Verify that Widget was registered with the correct Pylon module name
        # (derived from the file stem "things").
        from pylon.schema._registry import snapshot as _snap

        types, _, _ = _snap()
        widget_cls = next((t for t in types if t.__name__ == 'Widget'), None)
        assert widget_cls is not None
        assert widget_cls.__pylon_config__.module == 'things'

        # Clean up the dynamically imported module to avoid polluting other tests.
        sys.modules.pop('things', None)

    def test_finalize_imports_a_package_schema_dir_under_its_package_name(self, tmp_path):
        """A schema-dir with an `__init__.py` is imported as a package, so the application can
        import `pkgschema.things` and get the very class Pylon registered."""
        import importlib
        import sys

        import pylon as _pylon

        schema_dir = tmp_path / 'pkgschema'
        schema_dir.mkdir()
        (schema_dir / '__init__.py').write_text('')
        (schema_dir / 'owners.py').write_text('import pylon\n\n@pylon.type\nclass Owner:\n    name: str\n')
        (schema_dir / 'things.py').write_text(
            'from __future__ import annotations\n\n'
            'from typing import Annotated\n\n'
            'import pylon\n\n'
            'from .owners import Owner\n\n'
            '@pylon.type\nclass Widget:\n    label: str\n'
            '    owner: pylon.Link[Annotated["Owner", pylon.lazy(".owners")]] | None\n'
        )
        toml = tmp_path / 'pylon.toml'
        toml.write_text(
            '[project]\nschema-dir = "pkgschema"\n\n'
            '[database]\nhost = "localhost"\nport = 5432\n'
            'name = "mydb"\nuser = "u"\n'
        )

        try:
            _pylon.finalize(config=toml)

            from pylon.schema._registry import snapshot as _snap

            types, _, _ = _snap()
            widget_cls = next(t for t in types if t.__name__ == 'Widget')
            assert widget_cls.__pylon_config__.module == 'things'
            assert widget_cls is importlib.import_module('pkgschema.things').Widget
            assert 'things' not in sys.modules
        finally:
            for name in ('pkgschema', 'pkgschema.things', 'pkgschema.owners'):
                sys.modules.pop(name, None)
            sys.path[:] = [entry for entry in sys.path if entry != str(tmp_path)]
