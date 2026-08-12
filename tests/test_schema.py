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
import decimal
import inspect
import json
import types as _types
import uuid

import pytest

import pylon.schema as pylon
from pylon.datatypes import Object as PylonObject
from pylon.schema import (
    Array,
    Computed,
    Default,
    Description,
    Exclusive,
    Expression,
    Index,
    Link,
    MaxLen,
    MinValue,
    MultiLink,
    Now,
    On,
    Property,
    Readonly,
    Rewrite,
    Through,
    Timing,
    Trigger,
    Tuple,
)
from pylon.schema._channels import (
    RESERVED_WIRE_NAME_PREFIX,
    Channel,
    ChannelDescriptor,
    _to_snake_case,
    collect_module_channels,
    wire_name_for_channel,
)
from pylon.schema._globals import GlobalAnnotation, GlobalDescriptor, collect_module_globals
from pylon.schema._pointers import ArrayAnnotation, TupleAnnotation
from pylon.schema._scalars import PG_TYPE_MAP, SHORTHAND_MAP
from pylon.schema._walker import SchemaError, _build_channel_descriptor, _to_pg_type, _validate_channels

MISSING = dataclasses.MISSING
REQUIRED = inspect.Parameter.empty


# ---------------------------------------------------------------------------
# Shared schema definitions
# ---------------------------------------------------------------------------


@pylon.abstract
class Auditable:
    "Base type for audited records."

    created_at: Property[pylon.DateTime, Default(Now)]
    updated_at: Property[pylon.DateTime, Default(Now)]


@pylon.enum('Active', 'Inactive', 'Pending')
class Status(pylon.Enum):
    pass


@pylon.type
class Category:
    name: str


@pylon.type
class Tag:
    label: str


@pylon.type
class Product(Auditable):
    Description('A product available for purchase.')
    name: str
    slug: Property[str, Exclusive, MaxLen(120)]
    status: Status = Status.Active
    description: str | None
    tags_list: list[str] = []
    score: float = 0.0
    price: Property[pylon.Decimal, MinValue(0), Description('Price excl. tax')]
    category: Link[Category]
    alt_category: Link[Category] | None
    full_name: Computed[str, '.first ++ " " ++ .last']

    Exclusive(('category', 'slug'))
    Index('name')
    Index(('slug', 'name'))
    Index('str_lower(.name)', unless='.status')


@pylon.type
class ProductTag(Auditable):
    source: Link[Product]
    target: Link[Tag]
    weight: Property[pylon.Float64, MinValue(0)]


@pylon.type
class Catalog:
    products: MultiLink[Product, Through[ProductTag]]
    optional_tags: MultiLink[Tag] | None


@pylon.interface
class Publishable:
    published_at: Property[pylon.DateTime] | None


def _make_product(**overrides) -> Product:
    defaults = dict(
        name='Widget',
        slug='widget',
        price=decimal.Decimal('9.99'),
        category=Category(name='Gadgets'),
    )
    return Product(**{**defaults, **overrides})


# ---------------------------------------------------------------------------
# Decorators
# ---------------------------------------------------------------------------


class TestAbstractDecorator:
    def test_flags(self):
        cfg = Auditable.__pylon_config__
        assert cfg.abstract is True
        assert cfg.materialized is False

    def test_is_dataclass(self):
        assert dataclasses.is_dataclass(Auditable)

    def test_has_pylon_config(self):
        assert hasattr(Auditable, '__pylon_config__')

    def test_no_parens_form(self):
        @pylon.abstract
        class Bare:
            x: str

        assert Bare.__pylon_config__.abstract is True

    def test_parens_form(self):
        @pylon.abstract()
        class WithParens:
            x: str

        assert WithParens.__pylon_config__.abstract is True

    def test_module_override(self):
        @pylon.abstract(module='core')
        class Base:
            x: str

        assert Base.__pylon_config__.module == 'core'


class TestTypeDecorator:
    def test_flags(self):
        cfg = Product.__pylon_config__
        assert cfg.abstract is False
        assert cfg.materialized is True

    def test_no_parens_form(self):
        @pylon.type
        class Bare:
            x: str

        assert Bare.__pylon_config__.abstract is False

    def test_keyword_overrides(self):
        @pylon.type(module='catalog', name='Item', table='catalog_items')
        class Overridden:
            sku: str

        cfg = Overridden.__pylon_config__
        assert cfg.module == 'catalog'
        assert cfg.name == 'Item'
        assert cfg.table == 'catalog_items'


class TestInterfaceDecorator:
    def test_abstract_and_materialized(self):
        cfg = Publishable.__pylon_config__
        assert cfg.abstract is True
        assert cfg.materialized is True

    def test_is_dataclass(self):
        assert dataclasses.is_dataclass(Publishable)


# ---------------------------------------------------------------------------
# id injection
# ---------------------------------------------------------------------------


class TestIdInjection:
    def test_id_in_dataclass_fields(self):
        assert 'id' in Product.__dataclass_fields__

    def test_id_default_none_in_init(self):
        assert inspect.signature(Product.__init__).parameters['id'].default is None

    def test_id_not_duplicated_in_subtype(self):
        assert list(Product.__dataclass_fields__.keys()).count('id') == 1

    def test_id_is_none_before_save(self):
        assert _make_product().id is None

    def test_id_not_injected_when_parent_provides_it(self):
        # ProductTag inherits from Auditable which already has id via injection.
        # id should still appear exactly once.
        assert list(ProductTag.__dataclass_fields__.keys()).count('id') == 1


# ---------------------------------------------------------------------------
# Property fields
# ---------------------------------------------------------------------------


class TestPropertyField:
    def test_shorthand_str_resolved(self):
        f = Product.__pylon_config__.pointers['name']
        assert f.kind == 'property'
        assert f.scalar_type is pylon.Str
        assert f.nullable is False

    def test_shorthand_float_resolved(self):
        assert Product.__pylon_config__.pointers['score'].scalar_type is pylon.Float64

    def test_explicit_pylon_scalar(self):
        assert Product.__pylon_config__.pointers['price'].scalar_type is pylon.Decimal

    def test_field_description_extracted(self):
        assert Product.__pylon_config__.pointers['price'].description == 'Price excl. tax'

    def test_description_absent_from_constraints(self):
        constraints = Product.__pylon_config__.pointers['price'].constraints
        assert not any(isinstance(c, Description) for c in constraints)

    def test_min_value_in_constraints(self):
        constraints = Product.__pylon_config__.pointers['price'].constraints
        assert any(isinstance(c, MinValue) for c in constraints)

    def test_exclusive_bare_class_in_constraints(self):
        constraints = Product.__pylon_config__.pointers['slug'].constraints
        assert any(c is Exclusive for c in constraints)

    def test_max_len_in_constraints(self):
        constraints = Product.__pylon_config__.pointers['slug'].constraints
        assert any(isinstance(c, MaxLen) and c.length == 120 for c in constraints)

    def test_scalar_default_preserved(self):
        assert Product.__pylon_config__.pointers['score'].default == 0.0

    def test_default_now_python_side_none(self):
        f = Auditable.__pylon_config__.pointers['created_at']
        assert f.default is None
        assert any(isinstance(c, Default) and c.sentinel is Now for c in f.constraints)

    def test_default_now_optional_in_init(self):
        sig = inspect.signature(Auditable.__init__)
        assert sig.parameters['created_at'].default is None


# ---------------------------------------------------------------------------
# Structural tuple types (pylon.Tuple[...])
# ---------------------------------------------------------------------------


class TestTupleField:
    def test_unnamed_elements(self):
        ann = Tuple[pylon.Str, pylon.Bool]
        assert isinstance(ann, TupleAnnotation)
        assert [e.name for e in ann.elements] == [None, None]
        assert [e.type_ for e in ann.elements] == [pylon.Str, pylon.Bool]

    def test_named_elements(self):
        ann = Tuple[('r', pylon.Int16), ('g', pylon.Int16), ('b', pylon.Int16)]
        assert [e.name for e in ann.elements] == ['r', 'g', 'b']
        assert [e.type_ for e in ann.elements] == [pylon.Int16, pylon.Int16, pylon.Int16]

    def test_nested_tuple_element(self):
        ann = Tuple[
            ('origin', Tuple[('x', pylon.Float64), ('y', pylon.Float64)]),
            ('size', pylon.Float64),
        ]
        origin = ann.elements[0].type_
        assert isinstance(origin, TupleAnnotation)
        assert [e.name for e in origin.elements] == ['x', 'y']

    def test_mixed_named_and_unnamed_rejected(self):
        with pytest.raises(TypeError, match=r'all named .* or all unnamed'):
            Tuple[('x', pylon.Float64), pylon.Bool]

    def test_empty_rejected(self):
        with pytest.raises(TypeError, match='at least one element'):
            Tuple[()]

    def test_resolves_to_jsonb_pg_type(self):
        assert _to_pg_type(Tuple[pylon.Str, pylon.Bool]) == 'jsonb'
        assert _to_pg_type(Tuple[('r', pylon.Int16)]) == 'jsonb'

    def test_optional_via_union(self):
        @pylon.type
        class HasTuple:
            pair: Tuple[pylon.Str, pylon.Bool] | None

        f = HasTuple.__pylon_config__.pointers['pair']
        assert f.kind == 'property'
        assert isinstance(f.scalar_type, TupleAnnotation)
        assert f.nullable is True

    def test_property_meta_scalar_type_is_annotation(self):
        @pylon.type
        class HasRequiredTuple:
            rgb: Tuple[('r', pylon.Int16), ('g', pylon.Int16), ('b', pylon.Int16)]

        f = HasRequiredTuple.__pylon_config__.pointers['rgb']
        assert f.kind == 'property'
        assert f.nullable is False
        assert [e.name for e in f.scalar_type.elements] == ['r', 'g', 'b']

    def test_object_type_element_rejected(self):
        @pylon.type
        class Product:
            name: str

        with pytest.raises(SchemaError, match='expected a scalar type, got an object type'):
            _to_pg_type(Tuple[Product, pylon.Str])

    def test_nested_object_type_element_rejected(self):
        @pylon.type
        class Product:
            name: str

        with pytest.raises(SchemaError, match='expected a scalar type, got an object type'):
            _to_pg_type(Tuple[Tuple[Product, pylon.Str], pylon.Int64])


# ---------------------------------------------------------------------------
# One-dimensional array types (pylon.Array[T] and the bare list[T] shorthand)
# ---------------------------------------------------------------------------


class TestArrayField:
    def test_array_of_scalar(self):
        ann = Array[pylon.Str]
        assert isinstance(ann, ArrayAnnotation)
        assert ann.element is pylon.Str

    def test_nested_array_rejected(self):
        with pytest.raises(TypeError, match='one-dimensional'):
            Array[Array[pylon.Str]]

    def test_resolves_to_native_pg_array_not_jsonb(self):
        assert _to_pg_type(Array[pylon.Str]) == 'text[]'
        assert _to_pg_type(Array[pylon.Int64]) == 'int8[]'

    def test_array_of_tuple_resolves_to_jsonb_array(self):
        # An array's element may be anything except another array, including
        # a structural tuple — the array itself still stays a native pg
        # array (jsonb[]), it's just an array of jsonb-backed values.
        assert _to_pg_type(Array[Tuple[pylon.Str, pylon.Bool]]) == 'jsonb[]'

    def test_object_type_element_rejected(self):
        @pylon.type
        class Product:
            name: str

        with pytest.raises(SchemaError, match='expected a scalar type, got an object type'):
            _to_pg_type(Array[Product])

    def test_optional_via_union(self):
        @pylon.type
        class HasArray:
            tags: Array[pylon.Str] | None

        f = HasArray.__pylon_config__.pointers['tags']
        assert f.kind == 'property'
        assert isinstance(f.scalar_type, ArrayAnnotation)
        assert f.nullable is True

    def test_property_meta_scalar_type_is_annotation(self):
        @pylon.type
        class HasRequiredArray:
            tags: Array[pylon.Str]

        f = HasRequiredArray.__pylon_config__.pointers['tags']
        assert f.kind == 'property'
        assert f.nullable is False
        assert f.scalar_type.element is pylon.Str

    # ── bare `list[T]` shorthand — equivalent to Array[T], the same way a
    # bare `str` is equivalent to pylon.Str ──────────────────────────────────

    def test_bare_list_shorthand_resolves_same_as_array(self):
        @pylon.type
        class HasListShorthand:
            tags: list[str]

        f = HasListShorthand.__pylon_config__.pointers['tags']
        assert f.kind == 'property'
        assert isinstance(f.scalar_type, ArrayAnnotation)
        assert f.scalar_type.element is pylon.Str
        assert _to_pg_type(f.scalar_type) == 'text[]'

    def test_bare_list_shorthand_optional(self):
        @pylon.type
        class HasOptionalListShorthand:
            tags: list[str] | None

        f = HasOptionalListShorthand.__pylon_config__.pointers['tags']
        assert f.nullable is True
        assert isinstance(f.scalar_type, ArrayAnnotation)

    def test_bare_nested_list_rejected(self):
        with pytest.raises(TypeError, match='one-dimensional'):

            @pylon.type
            class HasNestedListShorthand:
                tags: list[list[str]]


# ---------------------------------------------------------------------------
# Readonly constraint
# ---------------------------------------------------------------------------


class TestReadonlyConstraint:
    def test_property_is_readonly(self):
        @pylon.type
        class Immut:
            slug: Property[str, Readonly]

        f = Immut.__pylon_config__.pointers['slug']
        assert f.is_readonly is True

    def test_property_not_readonly_by_default(self):
        @pylon.type
        class Normal:
            slug: str

        assert Normal.__pylon_config__.pointers['slug'].is_readonly is False

    def test_readonly_removed_from_constraints(self):
        @pylon.type
        class WithReadonly:
            code: Property[str, Readonly, MaxLen(10)]

        constraints = WithReadonly.__pylon_config__.pointers['code'].constraints
        assert not any(c is Readonly for c in constraints)
        assert any(isinstance(c, MaxLen) for c in constraints)

    def test_link_is_readonly(self):
        @pylon.type
        class HasReadonlyLink:
            owner: Link[Category, Readonly]

        f = HasReadonlyLink.__pylon_config__.pointers['owner']
        assert f.is_readonly is True

    def test_link_not_readonly_by_default(self):
        @pylon.type
        class HasNormalLink:
            ref: Link[Category]

        assert HasNormalLink.__pylon_config__.pointers['ref'].is_readonly is False

    def test_readonly_coexists_with_exclusive_on_property(self):
        @pylon.type
        class Multi:
            code: Property[str, Exclusive, Readonly]

        f = Multi.__pylon_config__.pointers['code']
        assert f.is_readonly is True
        assert any(c is Exclusive for c in f.constraints)


# ---------------------------------------------------------------------------
# Link fields
# ---------------------------------------------------------------------------


class TestLinkField:
    def test_kind(self):
        assert Product.__pylon_config__.pointers['category'].kind == 'link'

    def test_target_type(self):
        assert Product.__pylon_config__.pointers['category'].link_target is Category

    def test_required_link_in_init(self):
        assert inspect.signature(Product.__init__).parameters['category'].default is REQUIRED

    def test_nullable_link_default_none(self):
        assert inspect.signature(Product.__init__).parameters['alt_category'].default is None

    def test_nullable_link_flag(self):
        assert Product.__pylon_config__.pointers['alt_category'].nullable is True

    def test_non_nullable_link_flag(self):
        assert Product.__pylon_config__.pointers['category'].nullable is False

    def test_nullable_link_instance_value(self):
        assert _make_product().alt_category is None


# ---------------------------------------------------------------------------
# MultiLink fields
# ---------------------------------------------------------------------------


class TestMultiLinkField:
    def test_kind(self):
        assert Catalog.__pylon_config__.pointers['products'].kind == 'multilink'

    def test_through_type_stored(self):
        assert Catalog.__pylon_config__.pointers['products'].through is ProductTag

    def test_no_through_when_absent(self):
        assert Catalog.__pylon_config__.pointers['optional_tags'].through is None

    def test_default_empty_list(self):
        assert Catalog().products == []

    def test_nullable_multilink_flag(self):
        assert Catalog.__pylon_config__.pointers['optional_tags'].nullable is True


# ---------------------------------------------------------------------------
# Computed fields
# ---------------------------------------------------------------------------


class TestComputedField:
    def test_kind(self):
        assert Product.__pylon_config__.pointers['full_name'].kind == 'computed'

    def test_expression_stored(self):
        f = Product.__pylon_config__.pointers['full_name']
        assert f.expression == '.first ++ " " ++ .last'

    def test_excluded_from_init(self):
        assert 'full_name' not in inspect.signature(Product.__init__).parameters

    def test_instance_value_is_none(self):
        assert _make_product().full_name is None

    def test_missing_expression_raises(self):
        # Invoke __class_getitem__ directly — annotations are lazy strings under
        # `from __future__ import annotations` so class-body syntax won't fire.
        with pytest.raises(TypeError):
            Computed.__class_getitem__(str)

    def test_non_string_expression_raises(self):
        with pytest.raises(TypeError):
            Computed.__class_getitem__((str, 42))


# ---------------------------------------------------------------------------
# Nullable fields
# ---------------------------------------------------------------------------


class TestNullableFields:
    def test_str_or_none_flag(self):
        assert Product.__pylon_config__.pointers['description'].nullable is True

    def test_implicit_none_default_in_init(self):
        assert inspect.signature(Product.__init__).parameters['description'].default is None

    def test_non_nullable_required_in_init(self):
        assert inspect.signature(Product.__init__).parameters['name'].default is REQUIRED

    def test_nullable_property_annotation_on_interface(self):
        assert Publishable.__pylon_config__.pointers['published_at'].nullable is True


# ---------------------------------------------------------------------------
# Defaults
# ---------------------------------------------------------------------------


class TestDefaults:
    def test_enum_default(self):
        assert _make_product().status == Status.Active

    def test_scalar_default(self):
        assert _make_product().score == 0.0

    def test_mutable_default_uses_factory(self):
        f = Product.__pylon_config__.pointers['tags_list']
        assert f.default is MISSING
        assert f.default_factory is not None

    def test_mutable_default_isolation(self):
        cat = Category(name='c')
        p1 = Product(name='a', slug='a', price=decimal.Decimal('1'), category=cat)
        p2 = Product(name='b', slug='b', price=decimal.Decimal('1'), category=cat)
        p1.tags_list.append('x')
        assert p2.tags_list == []


# ---------------------------------------------------------------------------
# Class-body constraints
# ---------------------------------------------------------------------------


class TestClassBodyConstraints:
    def test_exclusive_composite_stored(self):
        constraints = Product.__pylon_config__.constraints
        assert len(constraints) == 1
        exc = constraints[0]
        assert isinstance(exc, Exclusive)
        assert exc.pointers == ('category', 'slug')

    def test_expression_constraint_stored(self):
        @pylon.type
        class DateRange:
            start: Property[pylon.LocalDate]
            end: Property[pylon.LocalDate]
            Expression('__subject__.start <= __subject__.end')

        assert any(isinstance(c, Expression) and 'start' in c.expr for c in DateRange.__pylon_config__.constraints)

    def test_exclusive_unless_clause(self):
        @pylon.type
        class Slug:
            value: str
            tenant: str
            Exclusive(('tenant', 'value'), unless='.deleted')

        exc = Slug.__pylon_config__.constraints[0]
        assert exc.unless == '.deleted'

    def test_description_does_not_leak_into_constraints(self):
        assert not any(isinstance(c, Description) for c in Product.__pylon_config__.constraints)


# ---------------------------------------------------------------------------
# Class-body indexes
# ---------------------------------------------------------------------------


class TestClassBodyIndexes:
    def test_count(self):
        assert len(Product.__pylon_config__.indexes) == 3

    def test_single_field(self):
        idx = Product.__pylon_config__.indexes[0]
        assert idx.pointer == 'name'
        assert idx.is_expression is False
        assert idx.unless is None

    def test_composite(self):
        idx = Product.__pylon_config__.indexes[1]
        assert idx.pointer == ('slug', 'name')

    def test_expression_index(self):
        idx = Product.__pylon_config__.indexes[2]
        assert idx.is_expression is True

    def test_partial_index_unless(self):
        idx = Product.__pylon_config__.indexes[2]
        assert idx.unless == '.status'


# ---------------------------------------------------------------------------
# Description
# ---------------------------------------------------------------------------


class TestDescription:
    def test_class_body_description(self):
        assert Product.__pylon_config__.description == 'A product available for purchase.'

    def test_class_body_overrides_docstring(self):
        @pylon.type
        class T:
            "A docstring."

            Description('The real description.')
            x: str

        assert T.__pylon_config__.description == 'The real description.'

    def test_docstring_fallback(self):
        assert Auditable.__pylon_config__.description == 'Base type for audited records.'

    def test_no_description_gives_none(self):
        @pylon.type
        class T:
            x: str

        assert T.__pylon_config__.description is None


# ---------------------------------------------------------------------------
# Enums
# ---------------------------------------------------------------------------


class TestEnum:
    def test_value_equals_name(self):
        assert Status.Active.value == 'Active'
        assert Status.Inactive.value == 'Inactive'
        assert Status.Pending.value == 'Pending'

    def test_str_mixin_equality(self):
        assert Status.Active == 'Active'

    def test_is_str_instance(self):
        assert isinstance(Status.Active, str)

    def test_member_count(self):
        assert len(Status) == 3

    def test_as_field_default(self):
        assert _make_product().status is Status.Active


# ---------------------------------------------------------------------------
# Custom scalars
# ---------------------------------------------------------------------------


class TestCustomScalar:
    def test_functional_is_scalar_subclass(self):
        T = pylon.scalar(pylon.Int64, MinValue(0))
        assert issubclass(T, pylon.Scalar)

    def test_functional_base_type(self):
        T = pylon.scalar(pylon.Int64, MinValue(0))
        assert T.__pylon_base__ is pylon.Int64

    def test_decorator_base_type(self):
        @pylon.scalar(pylon.Str)
        class Email(pylon.Scalar):
            pass

        assert Email.__pylon_base__ is pylon.Str

    def test_validate_hook(self):
        @pylon.scalar(pylon.Str)
        class Short(pylon.Scalar):
            @staticmethod
            def validate(v: str) -> None:
                if len(v) > 5:
                    raise ValueError('too long')

        Short.validate('hi')
        with pytest.raises(ValueError):
            Short.validate('toolong')

    def test_from_db_passthrough(self):
        assert pylon.Scalar.from_db('x') == 'x'

    def test_to_db_passthrough(self):
        assert pylon.Scalar.to_db('x') == 'x'


# ---------------------------------------------------------------------------
# Inheritance
# ---------------------------------------------------------------------------


class TestInheritance:
    def test_abstract_fields_in_concrete_init(self):
        params = inspect.signature(Product.__init__).parameters
        assert 'created_at' in params
        assert 'updated_at' in params

    def test_id_not_duplicated(self):
        assert list(Product.__dataclass_fields__.keys()).count('id') == 1

    def test_abstract_defaults_in_concrete(self):
        p = _make_product()
        assert p.created_at is None
        assert p.updated_at is None


# ---------------------------------------------------------------------------
# Naming
# ---------------------------------------------------------------------------


class TestNaming:
    def test_single_word(self):
        assert Category.__pylon_config__.table == 'Category'

    def test_pascal_case_preserved(self):
        @pylon.type(module='account')
        class AccountProfile:
            name: str

        assert AccountProfile.__pylon_config__.table == 'AccountProfile'

    def test_table_override(self):
        @pylon.type(module='shop', table='shop_items')
        class Item:
            sku: str

        assert Item.__pylon_config__.table == 'shop_items'

    def test_name_override(self):
        @pylon.type(module='shop', name='StoreItem')
        class Item:
            sku: str

        assert Item.__pylon_config__.name == 'StoreItem'

    def test_name_defaults_to_class_name(self):
        assert Product.__pylon_config__.name == 'Product'


# ---------------------------------------------------------------------------
# PylonConfig
# ---------------------------------------------------------------------------


class TestPylonConfig:
    def test_fields_dict_populated(self):
        assert len(Product.__pylon_config__.pointers) > 0

    def test_field_names_match_dataclass(self):
        cfg_keys = set(Product.__pylon_config__.pointers.keys())
        dc_keys = set(Product.__dataclass_fields__.keys())
        assert cfg_keys.issubset(dc_keys)

    def test_field_meta_type(self):
        for f in Product.__pylon_config__.pointers.values():
            assert isinstance(f, pylon.PointerMeta)

    def test_pylon_config_on_class_not_instance(self):
        # __pylon_config__ must be a class attribute, not stored per-instance.
        p = _make_product()
        assert '__pylon_config__' not in p.__dict__
        assert type(p).__pylon_config__ is Product.__pylon_config__


# ---------------------------------------------------------------------------
# lazy()
# ---------------------------------------------------------------------------


class TestLazy:
    def test_module_path(self):
        from pylon.schema import lazy

        assert lazy('.order').module_path == '.order'

    def test_repr(self):
        from pylon.schema import lazy

        assert repr(lazy('.product')) == "pylon.lazy('.product')"


# ---------------------------------------------------------------------------
# Scalar maps
# ---------------------------------------------------------------------------


class TestScalarMaps:
    @pytest.mark.parametrize(
        'py_type,pylon_type',
        [
            (str, pylon.Str),
            (int, pylon.Int64),
            (float, pylon.Float64),
            (bool, pylon.Bool),
        ],
    )
    def test_shorthand_map(self, py_type, pylon_type):
        assert SHORTHAND_MAP[py_type] is pylon_type

    @pytest.mark.parametrize(
        'pylon_type,pg_type',
        [
            (pylon.Str, 'text'),
            (pylon.Int16, 'int2'),
            (pylon.Int32, 'int4'),
            (pylon.Int64, 'int8'),
            (pylon.Float32, 'float4'),
            (pylon.Float64, 'float8'),
            (pylon.Bool, 'boolean'),
            (pylon.DateTime, 'timestamptz'),
            (pylon.LocalDateTime, 'timestamp'),
            (pylon.LocalDate, 'date'),
            (pylon.LocalTime, 'time'),
            (pylon.UUID, 'uuid'),
            (pylon.JSON, 'jsonb'),
            (pylon.Bytes, 'bytea'),
        ],
    )
    def test_pg_type_map(self, pylon_type, pg_type):
        assert PG_TYPE_MAP[pylon_type] == pg_type


# ---------------------------------------------------------------------------
# Triggers
# ---------------------------------------------------------------------------


class TestTrigger:
    def test_trigger_stored_in_config(self):
        @pylon.type
        class T:
            name: str
            Trigger(on=On.Insert, timing=Timing.After, handler='insert Log { note := __new__.name }')

        assert len(T.__pylon_config__.triggers) == 1

    def test_trigger_attributes(self):
        @pylon.type
        class T:
            name: str
            Trigger(on=On.Delete, timing=Timing.Before, handler='insert Log { note := __old__.name }')

        t = T.__pylon_config__.triggers[0]
        assert t.on == On.Delete
        assert t.timing == Timing.Before
        assert t.handler == 'insert Log { note := __old__.name }'

    def test_trigger_combined_events(self):
        @pylon.type
        class T:
            name: str
            Trigger(on=On.Insert | On.Update, timing=Timing.After, handler='insert Log { note := __new__.name }')

        t = T.__pylon_config__.triggers[0]
        assert On.Insert in t.on
        assert On.Update in t.on
        assert On.Delete not in t.on

    def test_multiple_triggers(self):
        @pylon.type
        class T:
            name: str
            Trigger(on=On.Insert, timing=Timing.After, handler='insert Log { note := __new__.name }')
            Trigger(on=On.Delete, timing=Timing.Before, handler='insert Log { note := __old__.name }')

        assert len(T.__pylon_config__.triggers) == 2

    def test_trigger_not_in_constraints_or_indexes(self):
        @pylon.type
        class T:
            name: str
            Trigger(on=On.Insert, timing=Timing.After, handler='insert Log { note := __new__.name }')

        assert len(T.__pylon_config__.constraints) == 0
        assert len(T.__pylon_config__.indexes) == 0

    def test_trigger_alongside_index_and_constraint(self):
        @pylon.type
        class T:
            name: str
            slug: str
            Index('name')
            Exclusive(('name', 'slug'))
            Trigger(on=On.Update, timing=Timing.After, handler='insert Log { note := __new__.name }')

        cfg = T.__pylon_config__
        assert len(cfg.indexes) == 1
        assert len(cfg.constraints) == 1
        assert len(cfg.triggers) == 1

    def test_timing_insteadof(self):
        @pylon.type
        class T:
            name: str
            Trigger(on=On.Insert, timing=Timing.InsteadOf, handler='insert Log { note := __new__.name }')

        assert T.__pylon_config__.triggers[0].timing == Timing.InsteadOf

    def test_on_flags_all_three(self):
        combined = On.Insert | On.Update | On.Delete
        assert On.Insert in combined
        assert On.Update in combined
        assert On.Delete in combined

    def test_repr(self):
        t = Trigger.__new__(Trigger)
        t.on = On.Insert
        t.timing = Timing.After
        t.handler = '.expr'
        assert 'Trigger' in repr(t)


# ---------------------------------------------------------------------------
# Mutation rewrites
# ---------------------------------------------------------------------------


class TestMutationRewrite:
    def test_rewrite_stored_in_field(self):
        @pylon.type
        class T:
            name: Property[str, Rewrite(On.Insert, '.name ++ " (created)"')]

        f = T.__pylon_config__.pointers['name']
        assert len(f.rewrites) == 1

    def test_rewrite_attributes(self):
        @pylon.type
        class T:
            name: Property[str, Rewrite(On.Update, '.name ++ " (updated)"')]

        r = T.__pylon_config__.pointers['name'].rewrites[0]
        assert r.on == On.Update
        assert r.handler == '.name ++ " (updated)"'

    def test_rewrite_not_in_constraints(self):
        @pylon.type
        class T:
            name: Property[str, MaxLen(50), Rewrite(On.Insert, '.name')]

        f = T.__pylon_config__.pointers['name']
        assert not any(isinstance(c, Rewrite) for c in f.constraints)
        assert any(isinstance(c, MaxLen) for c in f.constraints)

    def test_multiple_rewrites_on_same_field(self):
        @pylon.type
        class T:
            name: Property[
                str,
                Rewrite(On.Insert, '.name ++ " (created)"'),
                Rewrite(On.Update, '.name ++ " (updated)"'),
            ]

        f = T.__pylon_config__.pointers['name']
        assert len(f.rewrites) == 2
        ons = {r.on for r in f.rewrites}
        assert On.Insert in ons
        assert On.Update in ons

    def test_delete_rewrite_on_field(self):
        @pylon.type
        class T:
            group: Property[
                str,
                Rewrite(On.Delete, 'update T set { deleted_at := datetime_current() }'),
            ]

        r = T.__pylon_config__.pointers['group'].rewrites[0]
        assert r.on == On.Delete

    def test_rewrites_empty_by_default(self):
        f = Product.__pylon_config__.pointers['name']
        assert f.rewrites == []

    def test_rewrite_on_link(self):
        @pylon.type
        class T:
            ref: Link[Category, Rewrite(On.Update, '.expr')]

        f = T.__pylon_config__.pointers['ref']
        assert len(f.rewrites) == 1
        assert f.rewrites[0].on == On.Update

    def test_rewrite_does_not_affect_constraints_count(self):
        @pylon.type
        class T:
            name: Property[str, MaxLen(10), Rewrite(On.Insert, '.name')]

        f = T.__pylon_config__.pointers['name']
        assert len(f.constraints) == 1
        assert isinstance(f.constraints[0], MaxLen)

    def test_repr(self):
        r = Rewrite(On.Insert, '.expr')
        assert 'Rewrite' in repr(r)
        assert 'Insert' in repr(r)


# ---------------------------------------------------------------------------
# Globals
# ---------------------------------------------------------------------------


MISSING = dataclasses.MISSING


def _make_globals_module(**annotations) -> _types.ModuleType:
    m = _types.ModuleType('test_schema_globals')
    m.__annotations__ = annotations
    return m


class TestGlobal:
    def test_required_annotation(self):
        ann = pylon.Global[pylon.UUID]
        assert isinstance(ann, GlobalAnnotation)
        assert ann.required is True
        assert ann.scalar_type is pylon.UUID

    def test_optional_annotation(self):
        ann = pylon.Global[pylon.UUID | None]
        assert isinstance(ann, GlobalAnnotation)
        assert ann.required is False
        assert ann.scalar_type is pylon.UUID

    def test_str_type(self):
        ann = pylon.Global[pylon.Str]
        assert ann.scalar_type is pylon.Str
        assert ann.required is True

    def test_repr_contains_type(self):
        ann = pylon.Global[pylon.UUID]
        assert 'GlobalAnnotation' in repr(ann)
        assert 'required=True' in repr(ann)

    def test_collect_required(self):
        m = _make_globals_module(current_user_id=pylon.Global[pylon.UUID])
        result = collect_module_globals(m)
        assert len(result) == 1
        d = result[0]
        assert d.name == 'current_user_id'
        assert d.required is True
        assert d.scalar_type is pylon.UUID
        assert d.default is MISSING

    def test_collect_optional(self):
        m = _make_globals_module(current_user_id=pylon.Global[pylon.UUID | None])
        result = collect_module_globals(m)
        assert result[0].required is False

    def test_collect_with_default(self):
        default_val = uuid.UUID('12345678-1234-5678-1234-567812345678')
        m = _make_globals_module(current_tenant_id=pylon.Global[pylon.UUID | None])
        m.current_tenant_id = default_val
        result = collect_module_globals(m)
        assert len(result) == 1
        assert result[0].default is default_val

    def test_collect_multiple(self):
        m = _make_globals_module(
            current_user_id=pylon.Global[pylon.UUID],
            current_tenant_id=pylon.Global[pylon.UUID | None],
        )
        result = collect_module_globals(m)
        assert len(result) == 2
        names = {d.name for d in result}
        assert names == {'current_user_id', 'current_tenant_id'}

    def test_private_annotations_skipped(self):
        m = _make_globals_module(
            _private=pylon.Global[pylon.Str],
            public=pylon.Global[pylon.Str],
        )
        result = collect_module_globals(m)
        assert len(result) == 1
        assert result[0].name == 'public'

    def test_non_global_annotations_ignored(self):
        m = _make_globals_module(
            some_var=str,
            current_user=pylon.Global[pylon.UUID],
        )
        result = collect_module_globals(m)
        assert len(result) == 1
        assert result[0].name == 'current_user'

    def test_module_name_inferred_from_dotted_path(self):
        m = _types.ModuleType('pylon_app.schema.user')
        m.__annotations__ = {'x': pylon.Global[pylon.Str]}
        result = collect_module_globals(m)
        assert result[0].module == 'user'

    def test_pylon_module_override(self):
        m = _make_globals_module(x=pylon.Global[pylon.Str])
        m.__pylon_module__ = 'auth'
        result = collect_module_globals(m)
        assert result[0].module == 'auth'

    def test_module_name_default_module_fallback(self):
        m = _make_globals_module(x=pylon.Global[pylon.Str])
        m.__name__ = 'default'
        result = collect_module_globals(m)
        assert result[0].module == 'default'

    def test_empty_module_returns_empty_list(self):
        m = _make_globals_module()
        assert collect_module_globals(m) == []

    def test_global_descriptor_repr(self):
        d = GlobalDescriptor(name='x', module='user', scalar_type=pylon.UUID, required=True)
        r = repr(d)
        assert 'GlobalDescriptor' in r
        assert "'x'" in r
        assert 'required=True' in r

    def test_global_descriptor_repr_with_default(self):
        d = GlobalDescriptor(name='x', module='user', scalar_type=pylon.UUID, required=False, default='abc')
        assert "default='abc'" in repr(d)


# ---------------------------------------------------------------------------
# Channels
# ---------------------------------------------------------------------------


@pylon.type(module='shop', name='ChannelUser')
class ChannelUser:
    name: str


def _make_channels_module(**values) -> _types.ModuleType:
    m = _types.ModuleType('test_schema_channels')
    for name, value in values.items():
        setattr(m, name, value)
    return m


class TestChannel:
    def test_to_snake_case(self):
        assert _to_snake_case('UserUpdates') == 'user_updates'
        assert _to_snake_case('SearchReady') == 'search_ready'
        assert _to_snake_case('HTTPResponse') == 'http_response'
        assert _to_snake_case('already_snake') == 'already_snake'

    def test_wire_name_default_derivation(self):
        d = ChannelDescriptor(name='UserUpdates', module='shop', payload_type=str)
        assert wire_name_for_channel(d) == 'shop__user_updates'

    def test_wire_name_override_used_verbatim(self):
        d = ChannelDescriptor(name='Pings', module='shop', payload_type=str, wire_name_override='custom_ping')
        assert wire_name_for_channel(d) == 'custom_ping'

    def test_collect_bound_channel_value(self):
        m = _make_channels_module(UserUpdates=Channel(str))
        result = collect_module_channels(m)
        assert len(result) == 1
        d = result[0]
        assert d.name == 'UserUpdates'
        assert d.payload_type is str
        assert d.wire_name_override is None
        assert d.description is None

    def test_collect_with_name_and_description_override(self):
        m = _make_channels_module(Pings=Channel(str, name='custom_ping', description='heartbeat'))
        result = collect_module_channels(m)
        assert result[0].wire_name_override == 'custom_ping'
        assert result[0].description == 'heartbeat'

    def test_private_values_skipped(self):
        m = _make_channels_module(_Private=Channel(str), Public=Channel(str))
        result = collect_module_channels(m)
        assert len(result) == 1
        assert result[0].name == 'Public'

    def test_non_channel_values_ignored(self):
        m = _make_channels_module(some_var='not a channel', Real=Channel(str))
        result = collect_module_channels(m)
        assert len(result) == 1
        assert result[0].name == 'Real'

    def test_collect_multiple(self):
        m = _make_channels_module(A=Channel(str), B=Channel(int))
        result = collect_module_channels(m)
        assert {d.name for d in result} == {'A', 'B'}

    def test_module_name_inferred_from_dotted_path(self):
        m = _types.ModuleType('pylon_app.schema.notifications')
        m.X = Channel(str)
        result = collect_module_channels(m)
        assert result[0].module == 'notifications'

    def test_pylon_module_override(self):
        m = _make_channels_module(X=Channel(str))
        m.__pylon_module__ = 'auth'
        result = collect_module_channels(m)
        assert result[0].module == 'auth'

    def test_empty_module_returns_empty_list(self):
        m = _make_channels_module()
        assert collect_module_channels(m) == []

    def test_channel_descriptor_repr(self):
        d = ChannelDescriptor(name='X', module='shop', payload_type=str)
        assert 'ChannelDescriptor' in repr(d)
        assert "'X'" in repr(d)

    def test_build_descriptor_scalar_payload(self):
        d = ChannelDescriptor(name='Pings', module='shop', payload_type=str)
        from pylon import _core

        desc = _build_channel_descriptor(d, _core)
        assert desc.name == 'Pings'
        assert desc.wire_name == 'shop__pings'

    def test_build_descriptor_type_payload(self):
        d = ChannelDescriptor(name='UserUpdates', module='shop', payload_type=ChannelUser)
        from pylon import _core

        desc = _build_channel_descriptor(d, _core)
        assert desc.wire_name == 'shop__user_updates'

    def test_build_descriptor_object_payload(self):
        payload = PylonObject(doc_id=uuid.UUID, score=float)
        d = ChannelDescriptor(name='SearchReady', module='shop', payload_type=payload)
        from pylon import _core

        desc = _build_channel_descriptor(d, _core)
        assert desc.wire_name == 'shop__search_ready'

    def test_build_descriptor_object_payload_rejects_object_type_field(self):
        payload = PylonObject(user=ChannelUser)
        d = ChannelDescriptor(name='Bad', module='shop', payload_type=payload)
        from pylon import _core

        with pytest.raises(SchemaError):
            _build_channel_descriptor(d, _core)

    def test_validate_channels_rejects_reserved_prefix(self):
        d = ChannelDescriptor(name='X', module='shop', payload_type=str, wire_name_override='pylon_cache_invalidate')
        with pytest.raises(SchemaError, match='reserved'):
            _validate_channels([d])

    def test_validate_channels_rejects_cross_module_duplicate(self):
        a = ChannelDescriptor(name='A', module='m1', payload_type=str, wire_name_override='dup')
        b = ChannelDescriptor(name='B', module='m2', payload_type=str, wire_name_override='dup')
        with pytest.raises(SchemaError, match='Duplicate channel wire name'):
            _validate_channels([a, b])

    def test_validate_channels_allows_same_name_different_module(self):
        a = ChannelDescriptor(name='Updates', module='m1', payload_type=str)
        b = ChannelDescriptor(name='Updates', module='m2', payload_type=str)
        _validate_channels([a, b])  # no error — different wire names

    def test_validate_channels_empty_list_ok(self):
        _validate_channels([])

    def test_reserved_prefix_matches_pylons_own_internal_channels(self):
        assert RESERVED_WIRE_NAME_PREFIX == 'pylon_'


class TestChannelListenDecoding:
    """`Client.listen()`'s runtime lookup/decode helpers — pure unit
    coverage complementing the live end-to-end tests in
    tests/test_client_live.py (a real trigger firing notify(), a real
    listener receiving it)."""

    def _core_channel(self, **kwargs):
        from pylon._core import ChannelDescriptor as CoreChannelDescriptor

        return CoreChannelDescriptor(**kwargs)

    def test_decode_scalar_text_uuid(self):
        from pylon.schema._channels import _decode_scalar_text

        u = uuid.UUID('3fa85f64-5717-4562-b3fc-2c963f66afa6')
        assert _decode_scalar_text(str(u), 'uuid') == u

    def test_decode_scalar_text_integers(self):
        from pylon.schema._channels import _decode_scalar_text

        assert _decode_scalar_text('42', 'int2') == 42
        assert _decode_scalar_text('42', 'int4') == 42
        assert _decode_scalar_text('42', 'int8') == 42

    def test_decode_scalar_text_floats(self):
        from pylon.schema._channels import _decode_scalar_text

        assert _decode_scalar_text('0.5', 'float4') == 0.5
        assert _decode_scalar_text('0.5', 'float8') == 0.5

    def test_decode_scalar_text_numeric_is_decimal(self):
        import decimal

        from pylon.schema._channels import _decode_scalar_text

        assert _decode_scalar_text('123.456', 'numeric') == decimal.Decimal('123.456')

    def test_decode_scalar_text_boolean(self):
        from pylon.schema._channels import _decode_scalar_text

        assert _decode_scalar_text('true', 'boolean') is True
        assert _decode_scalar_text('false', 'boolean') is False

    def test_decode_scalar_text_datetimes(self):
        import datetime

        from pylon.schema._channels import _decode_scalar_text

        assert _decode_scalar_text('2026-08-05 18:47:59.698038+00', 'timestamptz') == datetime.datetime.fromisoformat(
            '2026-08-05 18:47:59.698038+00:00'
        )
        assert _decode_scalar_text('2026-08-05', 'date') == datetime.date(2026, 8, 5)
        assert _decode_scalar_text('18:47:59', 'time') == datetime.time(18, 47, 59)

    def test_decode_scalar_text_text_passthrough(self):
        from pylon.schema._channels import _decode_scalar_text

        assert _decode_scalar_text('hello', 'text') == 'hello'

    def test_decode_scalar_text_unsupported_type_falls_back_to_raw_string(self):
        from pylon.schema._channels import _decode_scalar_text

        assert _decode_scalar_text('1 day 02:00:00', 'interval') == '1 day 02:00:00'

    def test_decode_json_value_passes_through_none_and_non_strings(self):
        from pylon.schema._channels import _decode_json_value

        assert _decode_json_value(None, 'uuid') is None
        assert _decode_json_value(0.5, 'float8') == 0.5
        assert _decode_json_value(True, 'boolean') is True

    def test_decode_json_value_parses_string_encoded_types(self):
        from pylon.schema._channels import _decode_json_value

        u = uuid.UUID('3fa85f64-5717-4562-b3fc-2c963f66afa6')
        assert _decode_json_value(str(u), 'uuid') == u

    def test_decode_channel_payload_scalar(self):
        from pylon.schema._channels import decode_channel_payload

        ch = self._core_channel(
            name='Pings', module='m', wire_name='m__pings', payload_kind='scalar', payload_scalar_pg_type='text'
        )
        assert decode_channel_payload(ch, 'hello') == 'hello'

    def test_decode_channel_payload_type_kind_is_a_uuid(self):
        from pylon.schema._channels import decode_channel_payload

        ch = self._core_channel(
            name='X', module='m', wire_name='m__x', payload_kind='type', payload_type_ref='m::Widget'
        )
        u = uuid.UUID('3fa85f64-5717-4562-b3fc-2c963f66afa6')
        assert decode_channel_payload(ch, str(u)) == u

    def test_decode_channel_payload_object(self):
        from pylon.datatypes import Object as PylonObject
        from pylon.schema._channels import decode_channel_payload

        ch = self._core_channel(
            name='SearchReady',
            module='m',
            wire_name='m__search_ready',
            payload_kind='object',
            payload_object_fields=[('doc_id', 'uuid'), ('score', 'float8')],
        )
        u = uuid.UUID('3fa85f64-5717-4562-b3fc-2c963f66afa6')
        payload = decode_channel_payload(ch, json.dumps({'doc_id': str(u), 'score': 0.5}))
        assert isinstance(payload, PylonObject)
        assert payload.doc_id == u
        assert payload.score == 0.5

    def test_decode_channel_payload_raises_query_error_on_malformed_uuid(self):
        from pylon.exceptions import QueryError
        from pylon.schema._channels import decode_channel_payload

        ch = self._core_channel(
            name='X', module='m', wire_name='m__x', payload_kind='type', payload_type_ref='m::Widget'
        )
        with pytest.raises(QueryError, match="doesn't match its declared shape"):
            decode_channel_payload(ch, 'not-a-uuid')

    def test_decode_channel_payload_raises_query_error_on_malformed_json(self):
        from pylon.exceptions import QueryError
        from pylon.schema._channels import decode_channel_payload

        ch = self._core_channel(
            name='SearchReady',
            module='m',
            wire_name='m__search_ready',
            payload_kind='object',
            payload_object_fields=[('doc_id', 'uuid')],
        )
        with pytest.raises(QueryError, match="doesn't match its declared shape"):
            decode_channel_payload(ch, 'not json at all')

    def test_resolve_channel_by_bare_name(self):
        from pylon._core import SchemaDescriptor as CoreSchemaDescriptor

        from pylon.schema._channels import resolve_channel

        ch = self._core_channel(
            name='Pings', module='shop', wire_name='shop__pings', payload_kind='scalar', payload_scalar_pg_type='text'
        )
        schema = CoreSchemaDescriptor(channels=[ch])
        assert resolve_channel(schema, 'Pings').wire_name == 'shop__pings'

    def test_resolve_channel_by_qualified_name(self):
        from pylon._core import SchemaDescriptor as CoreSchemaDescriptor

        from pylon.schema._channels import resolve_channel

        ch = self._core_channel(
            name='Pings', module='shop', wire_name='shop__pings', payload_kind='scalar', payload_scalar_pg_type='text'
        )
        schema = CoreSchemaDescriptor(channels=[ch])
        assert resolve_channel(schema, 'shop::Pings').wire_name == 'shop__pings'

    def test_resolve_channel_raises_on_unknown_name(self):
        from pylon._core import SchemaDescriptor as CoreSchemaDescriptor

        from pylon.exceptions import QueryError
        from pylon.schema._channels import resolve_channel

        schema = CoreSchemaDescriptor(channels=[])
        with pytest.raises(QueryError, match='not a known Channel'):
            resolve_channel(schema, 'NoSuchChannel')
