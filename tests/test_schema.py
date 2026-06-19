from __future__ import annotations

import dataclasses
import decimal
import inspect

import pytest

import pylon.schema as pylon
from pylon.schema import (
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
    Rewrite,
    Timing,
    Trigger,
    through,
)
from pylon.schema._scalars import PG_TYPE_MAP, SHORTHAND_MAP

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


@pylon.enum("Active", "Inactive", "Pending")
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
    Description("A product available for purchase.")
    name: str
    slug: Property[str, Exclusive, MaxLen(120)]
    status: Status = Status.Active
    description: str | None
    tags_list: list[str] = []
    score: float = 0.0
    price: Property[pylon.Decimal, MinValue(0), Description("Price excl. tax")]
    category: Link[Category]
    alt_category: Link[Category] | None
    full_name: Computed[str, '.first ++ " " ++ .last']

    Exclusive(("category", "slug"))
    Index("name")
    Index(("slug", "name"))
    Index("str_lower(.name)", unless=".status")


@pylon.type
class ProductTag(Auditable):
    source: Link[Product]
    target: Link[Tag]
    weight: Property[pylon.Float64, MinValue(0)]


@pylon.type
class Catalog:
    products: MultiLink[Product, through(ProductTag)]
    optional_tags: MultiLink[Tag] | None


@pylon.interface
class Publishable:
    published_at: Property[pylon.DateTime] | None


def _make_product(**overrides) -> Product:
    defaults = dict(
        name="Widget",
        slug="widget",
        price=decimal.Decimal("9.99"),
        category=Category(name="Gadgets"),
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
        assert hasattr(Auditable, "__pylon_config__")

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
        @pylon.abstract(module="core")
        class Base:
            x: str

        assert Base.__pylon_config__.module == "core"


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
        @pylon.type(module="catalog", name="Item", table="catalog_items")
        class Overridden:
            sku: str

        cfg = Overridden.__pylon_config__
        assert cfg.module == "catalog"
        assert cfg.name == "Item"
        assert cfg.table == "catalog_items"


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
        assert "id" in Product.__dataclass_fields__

    def test_id_default_none_in_init(self):
        assert inspect.signature(Product.__init__).parameters["id"].default is None

    def test_id_not_duplicated_in_subtype(self):
        assert list(Product.__dataclass_fields__.keys()).count("id") == 1

    def test_id_is_none_before_save(self):
        assert _make_product().id is None

    def test_id_not_injected_when_parent_provides_it(self):
        # ProductTag inherits from Auditable which already has id via injection.
        # id should still appear exactly once.
        assert list(ProductTag.__dataclass_fields__.keys()).count("id") == 1


# ---------------------------------------------------------------------------
# Property fields
# ---------------------------------------------------------------------------


class TestPropertyField:
    def test_shorthand_str_resolved(self):
        f = Product.__pylon_config__.fields["name"]
        assert f.kind == "property"
        assert f.scalar_type is pylon.Str
        assert f.nullable is False

    def test_shorthand_float_resolved(self):
        assert Product.__pylon_config__.fields["score"].scalar_type is pylon.Float64

    def test_explicit_pylon_scalar(self):
        assert Product.__pylon_config__.fields["price"].scalar_type is pylon.Decimal

    def test_field_description_extracted(self):
        assert Product.__pylon_config__.fields["price"].description == "Price excl. tax"

    def test_description_absent_from_constraints(self):
        constraints = Product.__pylon_config__.fields["price"].constraints
        assert not any(isinstance(c, Description) for c in constraints)

    def test_min_value_in_constraints(self):
        constraints = Product.__pylon_config__.fields["price"].constraints
        assert any(isinstance(c, MinValue) for c in constraints)

    def test_exclusive_bare_class_in_constraints(self):
        constraints = Product.__pylon_config__.fields["slug"].constraints
        assert any(c is Exclusive for c in constraints)

    def test_max_len_in_constraints(self):
        constraints = Product.__pylon_config__.fields["slug"].constraints
        assert any(isinstance(c, MaxLen) and c.length == 120 for c in constraints)

    def test_scalar_default_preserved(self):
        assert Product.__pylon_config__.fields["score"].default == 0.0

    def test_default_now_python_side_none(self):
        f = Auditable.__pylon_config__.fields["created_at"]
        assert f.default is None
        assert any(isinstance(c, Default) and c.sentinel is Now for c in f.constraints)

    def test_default_now_optional_in_init(self):
        sig = inspect.signature(Auditable.__init__)
        assert sig.parameters["created_at"].default is None


# ---------------------------------------------------------------------------
# Link fields
# ---------------------------------------------------------------------------


class TestLinkField:
    def test_kind(self):
        assert Product.__pylon_config__.fields["category"].kind == "link"

    def test_target_type(self):
        assert Product.__pylon_config__.fields["category"].link_target is Category

    def test_required_link_in_init(self):
        assert (
            inspect.signature(Product.__init__).parameters["category"].default
            is REQUIRED
        )

    def test_nullable_link_default_none(self):
        assert (
            inspect.signature(Product.__init__).parameters["alt_category"].default
            is None
        )

    def test_nullable_link_flag(self):
        assert Product.__pylon_config__.fields["alt_category"].nullable is True

    def test_non_nullable_link_flag(self):
        assert Product.__pylon_config__.fields["category"].nullable is False

    def test_nullable_link_instance_value(self):
        assert _make_product().alt_category is None


# ---------------------------------------------------------------------------
# MultiLink fields
# ---------------------------------------------------------------------------


class TestMultiLinkField:
    def test_kind(self):
        assert Catalog.__pylon_config__.fields["products"].kind == "multilink"

    def test_through_type_stored(self):
        assert Catalog.__pylon_config__.fields["products"].through is ProductTag

    def test_no_through_when_absent(self):
        assert Catalog.__pylon_config__.fields["optional_tags"].through is None

    def test_default_empty_list(self):
        assert Catalog().products == []

    def test_nullable_multilink_flag(self):
        assert Catalog.__pylon_config__.fields["optional_tags"].nullable is True


# ---------------------------------------------------------------------------
# Computed fields
# ---------------------------------------------------------------------------


class TestComputedField:
    def test_kind(self):
        assert Product.__pylon_config__.fields["full_name"].kind == "computed"

    def test_expression_stored(self):
        f = Product.__pylon_config__.fields["full_name"]
        assert f.expression == '.first ++ " " ++ .last'

    def test_excluded_from_init(self):
        assert "full_name" not in inspect.signature(Product.__init__).parameters

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
        assert Product.__pylon_config__.fields["description"].nullable is True

    def test_implicit_none_default_in_init(self):
        assert (
            inspect.signature(Product.__init__).parameters["description"].default
            is None
        )

    def test_non_nullable_required_in_init(self):
        assert (
            inspect.signature(Product.__init__).parameters["name"].default is REQUIRED
        )

    def test_nullable_property_annotation_on_interface(self):
        assert Publishable.__pylon_config__.fields["published_at"].nullable is True


# ---------------------------------------------------------------------------
# Defaults
# ---------------------------------------------------------------------------


class TestDefaults:
    def test_enum_default(self):
        assert _make_product().status == Status.Active

    def test_scalar_default(self):
        assert _make_product().score == 0.0

    def test_mutable_default_uses_factory(self):
        f = Product.__pylon_config__.fields["tags_list"]
        assert f.default is MISSING
        assert f.default_factory is not None

    def test_mutable_default_isolation(self):
        cat = Category(name="c")
        p1 = Product(name="a", slug="a", price=decimal.Decimal("1"), category=cat)
        p2 = Product(name="b", slug="b", price=decimal.Decimal("1"), category=cat)
        p1.tags_list.append("x")
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
        assert exc.fields == ("category", "slug")

    def test_expression_constraint_stored(self):
        @pylon.type
        class DateRange:
            start: Property[pylon.LocalDate]
            end: Property[pylon.LocalDate]
            Expression("__subject__.start <= __subject__.end")

        assert any(
            isinstance(c, Expression) and "start" in c.expr
            for c in DateRange.__pylon_config__.constraints
        )

    def test_exclusive_unless_clause(self):
        @pylon.type
        class Slug:
            value: str
            tenant: str
            Exclusive(("tenant", "value"), unless=".deleted")

        exc = Slug.__pylon_config__.constraints[0]
        assert exc.unless == ".deleted"

    def test_description_does_not_leak_into_constraints(self):
        assert not any(
            isinstance(c, Description) for c in Product.__pylon_config__.constraints
        )


# ---------------------------------------------------------------------------
# Class-body indexes
# ---------------------------------------------------------------------------


class TestClassBodyIndexes:
    def test_count(self):
        assert len(Product.__pylon_config__.indexes) == 3

    def test_single_field(self):
        idx = Product.__pylon_config__.indexes[0]
        assert idx.field == "name"
        assert idx.is_expression is False
        assert idx.unless is None

    def test_composite(self):
        idx = Product.__pylon_config__.indexes[1]
        assert idx.field == ("slug", "name")

    def test_expression_index(self):
        idx = Product.__pylon_config__.indexes[2]
        assert idx.is_expression is True

    def test_partial_index_unless(self):
        idx = Product.__pylon_config__.indexes[2]
        assert idx.unless == ".status"


# ---------------------------------------------------------------------------
# Description
# ---------------------------------------------------------------------------


class TestDescription:
    def test_class_body_description(self):
        assert (
            Product.__pylon_config__.description == "A product available for purchase."
        )

    def test_class_body_overrides_docstring(self):
        @pylon.type
        class T:
            "A docstring."

            Description("The real description.")
            x: str

        assert T.__pylon_config__.description == "The real description."

    def test_docstring_fallback(self):
        assert (
            Auditable.__pylon_config__.description == "Base type for audited records."
        )

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
        assert Status.Active.value == "Active"
        assert Status.Inactive.value == "Inactive"
        assert Status.Pending.value == "Pending"

    def test_str_mixin_equality(self):
        assert Status.Active == "Active"

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
                    raise ValueError("too long")

        Short.validate("hi")
        with pytest.raises(ValueError):
            Short.validate("toolong")

    def test_from_db_passthrough(self):
        assert pylon.Scalar.from_db("x") == "x"

    def test_to_db_passthrough(self):
        assert pylon.Scalar.to_db("x") == "x"


# ---------------------------------------------------------------------------
# Inheritance
# ---------------------------------------------------------------------------


class TestInheritance:
    def test_abstract_fields_in_concrete_init(self):
        params = inspect.signature(Product.__init__).parameters
        assert "created_at" in params
        assert "updated_at" in params

    def test_id_not_duplicated(self):
        assert list(Product.__dataclass_fields__.keys()).count("id") == 1

    def test_abstract_defaults_in_concrete(self):
        p = _make_product()
        assert p.created_at is None
        assert p.updated_at is None


# ---------------------------------------------------------------------------
# Naming
# ---------------------------------------------------------------------------


class TestNaming:
    def test_single_word(self):
        assert Category.__pylon_config__.table.endswith("_category")

    def test_pascal_to_snake(self):
        @pylon.type(module="account")
        class AccountProfile:
            name: str

        assert AccountProfile.__pylon_config__.table == "account_account_profile"

    def test_table_override(self):
        @pylon.type(module="shop", table="shop_items")
        class Item:
            sku: str

        assert Item.__pylon_config__.table == "shop_items"

    def test_name_override(self):
        @pylon.type(module="shop", name="StoreItem")
        class Item:
            sku: str

        assert Item.__pylon_config__.name == "StoreItem"

    def test_name_defaults_to_class_name(self):
        assert Product.__pylon_config__.name == "Product"


# ---------------------------------------------------------------------------
# PylonConfig
# ---------------------------------------------------------------------------


class TestPylonConfig:
    def test_fields_dict_populated(self):
        assert len(Product.__pylon_config__.fields) > 0

    def test_field_names_match_dataclass(self):
        cfg_keys = set(Product.__pylon_config__.fields.keys())
        dc_keys = set(Product.__dataclass_fields__.keys())
        assert cfg_keys.issubset(dc_keys)

    def test_field_meta_type(self):
        for f in Product.__pylon_config__.fields.values():
            assert isinstance(f, pylon.FieldMeta)

    def test_pylon_config_on_class_not_instance(self):
        # __pylon_config__ must be a class attribute, not stored per-instance.
        p = _make_product()
        assert "__pylon_config__" not in p.__dict__
        assert type(p).__pylon_config__ is Product.__pylon_config__


# ---------------------------------------------------------------------------
# lazy()
# ---------------------------------------------------------------------------


class TestLazy:
    def test_module_path(self):
        from pylon.schema import lazy

        assert lazy(".order").module_path == ".order"

    def test_repr(self):
        from pylon.schema import lazy

        assert repr(lazy(".product")) == "pylon.lazy('.product')"


# ---------------------------------------------------------------------------
# Scalar maps
# ---------------------------------------------------------------------------


class TestScalarMaps:
    @pytest.mark.parametrize(
        "py_type,pylon_type",
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
        "pylon_type,pg_type",
        [
            (pylon.Str, "text"),
            (pylon.Int16, "int2"),
            (pylon.Int32, "int4"),
            (pylon.Int64, "int8"),
            (pylon.Float32, "float4"),
            (pylon.Float64, "float8"),
            (pylon.Bool, "boolean"),
            (pylon.DateTime, "timestamptz"),
            (pylon.LocalDateTime, "timestamp"),
            (pylon.LocalDate, "date"),
            (pylon.LocalTime, "time"),
            (pylon.UUID, "uuid"),
            (pylon.JSON, "jsonb"),
            (pylon.Bytes, "bytea"),
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
            Trigger(on=On.Insert, timing=Timing.After, handler=".do_something")

        assert len(T.__pylon_config__.triggers) == 1

    def test_trigger_attributes(self):
        @pylon.type
        class T:
            name: str
            Trigger(on=On.Delete, timing=Timing.Before, handler=".cleanup")

        t = T.__pylon_config__.triggers[0]
        assert t.on == On.Delete
        assert t.timing == Timing.Before
        assert t.handler == ".cleanup"

    def test_trigger_combined_events(self):
        @pylon.type
        class T:
            name: str
            Trigger(on=On.Insert | On.Update, timing=Timing.After, handler=".expr")

        t = T.__pylon_config__.triggers[0]
        assert On.Insert in t.on
        assert On.Update in t.on
        assert On.Delete not in t.on

    def test_multiple_triggers(self):
        @pylon.type
        class T:
            name: str
            Trigger(on=On.Insert, timing=Timing.After, handler=".on_insert")
            Trigger(on=On.Delete, timing=Timing.Before, handler=".on_delete")

        assert len(T.__pylon_config__.triggers) == 2

    def test_trigger_not_in_constraints_or_indexes(self):
        @pylon.type
        class T:
            name: str
            Trigger(on=On.Insert, timing=Timing.After, handler=".expr")

        assert len(T.__pylon_config__.constraints) == 0
        assert len(T.__pylon_config__.indexes) == 0

    def test_trigger_alongside_index_and_constraint(self):
        @pylon.type
        class T:
            name: str
            slug: str
            Index("name")
            Exclusive(("name", "slug"))
            Trigger(on=On.Update, timing=Timing.After, handler=".expr")

        cfg = T.__pylon_config__
        assert len(cfg.indexes) == 1
        assert len(cfg.constraints) == 1
        assert len(cfg.triggers) == 1

    def test_timing_insteadof(self):
        @pylon.type
        class T:
            name: str
            Trigger(on=On.Insert, timing=Timing.InsteadOf, handler=".expr")

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
        t.handler = ".expr"
        assert "Trigger" in repr(t)


# ---------------------------------------------------------------------------
# Mutation rewrites
# ---------------------------------------------------------------------------


class TestMutationRewrite:
    def test_rewrite_stored_in_field(self):
        @pylon.type
        class T:
            name: Property[str, Rewrite(On.Insert, '.name ++ " (created)"')]

        f = T.__pylon_config__.fields["name"]
        assert len(f.rewrites) == 1

    def test_rewrite_attributes(self):
        @pylon.type
        class T:
            name: Property[str, Rewrite(On.Update, '.name ++ " (updated)"')]

        r = T.__pylon_config__.fields["name"].rewrites[0]
        assert r.on == On.Update
        assert r.handler == '.name ++ " (updated)"'

    def test_rewrite_not_in_constraints(self):
        @pylon.type
        class T:
            name: Property[str, MaxLen(50), Rewrite(On.Insert, ".name")]

        f = T.__pylon_config__.fields["name"]
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

        f = T.__pylon_config__.fields["name"]
        assert len(f.rewrites) == 2
        ons = {r.on for r in f.rewrites}
        assert On.Insert in ons
        assert On.Update in ons

    def test_delete_rewrite_on_field(self):
        @pylon.type
        class T:
            group: Property[
                str,
                Rewrite(On.Delete, "update T set { deleted_at := datetime_current() }"),
            ]

        r = T.__pylon_config__.fields["group"].rewrites[0]
        assert r.on == On.Delete

    def test_rewrites_empty_by_default(self):
        f = Product.__pylon_config__.fields["name"]
        assert f.rewrites == []

    def test_rewrite_on_link(self):
        @pylon.type
        class T:
            ref: Link[Category, Rewrite(On.Update, ".expr")]

        f = T.__pylon_config__.fields["ref"]
        assert len(f.rewrites) == 1
        assert f.rewrites[0].on == On.Update

    def test_rewrite_does_not_affect_constraints_count(self):
        @pylon.type
        class T:
            name: Property[str, MaxLen(10), Rewrite(On.Insert, ".name")]

        f = T.__pylon_config__.fields["name"]
        assert len(f.constraints) == 1
        assert isinstance(f.constraints[0], MaxLen)

    def test_repr(self):
        r = Rewrite(On.Insert, ".expr")
        assert "Rewrite" in repr(r)
        assert "Insert" in repr(r)
