"""Unit tests for pylon.server.asgi's schema-introspection helpers — the
/api/schema response builders. These call the module's pure functions
directly rather than spinning up the ASGI app, matching how the rest of the
test suite exercises the schema layer (tests/test_schema.py).

Deliberately does NOT use `from __future__ import annotations`: asgi.py reads
raw class annotations at introspection time (not via the dataclass-building
machinery in _decorators.py), so test classes here must behave like real
user schema files — which never use PEP 563 string annotations either (see
pylon-demo/dbschema/*.py)."""

import dataclasses

import pylon.schema as pylon
from pylon.schema._meta import PointerMeta
from pylon.server.asgi import (
    _build_named_tuple_entry,
    _build_type_entry,
    _classify_named_tuple_member,
    _classify_pointer,
    _classify_tuple_elements,
    _pointer_editability,
)


class TestClassifyStructuralTuplePointer:
    def test_unnamed_elements(self):
        ann = pylon.Tuple[pylon.Str, pylon.Bool]
        result = _classify_pointer(ann, object, set(), None, set())
        assert result["kind"] == "namedTuple"
        assert "target" not in result
        assert result["members"] == [
            {"name": None, "kind": "scalar", "typeName": "std::str"},
            {"name": None, "kind": "scalar", "typeName": "std::bool"},
        ]

    def test_named_elements(self):
        ann = pylon.Tuple[("r", pylon.Int16), ("g", pylon.Int16)]
        result = _classify_pointer(ann, object, set(), None, set())
        assert result["kind"] == "namedTuple"
        assert result["members"] == [
            {"name": "r", "kind": "scalar", "typeName": "std::int16"},
            {"name": "g", "kind": "scalar", "typeName": "std::int16"},
        ]

    def test_nested_tuple_element(self):
        ann = pylon.Tuple[
            ("origin", pylon.Tuple[("x", pylon.Float64), ("y", pylon.Float64)]),
            ("size", pylon.Float64),
        ]
        result = _classify_pointer(ann, object, set(), None, set())
        origin_member = result["members"][0]
        assert origin_member["name"] == "origin"
        assert origin_member["kind"] == "namedTuple"
        assert "target" not in origin_member
        assert origin_member["members"] == [
            {"name": "x", "kind": "scalar", "typeName": "std::float64"},
            {"name": "y", "kind": "scalar", "typeName": "std::float64"},
        ]
        assert result["members"][1] == {"name": "size", "kind": "scalar", "typeName": "std::float64"}

    def test_enum_element(self):
        @pylon.enum("Active", "Inactive")
        class Status(pylon.Enum):
            pass

        ann = pylon.Tuple[("status", Status)]
        result = _classify_pointer(ann, object, {Status}, None, set())
        assert result["members"] == [
            {"name": "status", "kind": "enum", "target": "test_asgi_schema::Status"}
        ]

    def test_nominal_named_tuple_element(self):
        @pylon.named_tuple
        class Point(pylon.NamedTuple):
            x: pylon.Float64
            y: pylon.Float64

        ann = pylon.Tuple[("origin", Point)]
        result = _classify_pointer(ann, object, set(), None, {Point})
        assert result["members"] == [
            {"name": "origin", "kind": "namedTuple", "target": "test_asgi_schema::Point"}
        ]

    def test_optional_tuple_pointer_still_classifies_via_unwrap(self):
        ann = pylon.Tuple[pylon.Str] | None
        result = _classify_pointer(ann, object, set(), None, set())
        assert result["kind"] == "namedTuple"
        assert result["members"] == [{"name": None, "kind": "scalar", "typeName": "std::str"}]


class TestClassifyTupleElementsHelper:
    def test_returns_flat_list_for_unnamed(self):
        ann = pylon.Tuple[pylon.Bool, pylon.Bool]
        members = _classify_tuple_elements(ann, set(), set())
        assert members == [
            {"name": None, "kind": "scalar", "typeName": "std::bool"},
            {"name": None, "kind": "scalar", "typeName": "std::bool"},
        ]


class TestNominalNamedTupleWithStructuralMember:
    def test_named_tuple_member_can_be_a_structural_tuple(self):
        @pylon.named_tuple
        class Shape(pylon.NamedTuple):
            origin: pylon.Tuple[("x", pylon.Float64), ("y", pylon.Float64)]

        annotation = Shape.__annotations__["origin"]
        result = _classify_named_tuple_member(annotation, set(), set())
        assert result["kind"] == "namedTuple"
        assert "target" not in result
        assert result["members"] == [
            {"name": "x", "kind": "scalar", "typeName": "std::float64"},
            {"name": "y", "kind": "scalar", "typeName": "std::float64"},
        ]
        assert result["required"] is True

    def test_build_named_tuple_entry_end_to_end(self):
        @pylon.named_tuple
        class Shape2(pylon.NamedTuple):
            origin: pylon.Tuple[("x", pylon.Float64), ("y", pylon.Float64)]

        entry = _build_named_tuple_entry(Shape2, set(), set())
        assert entry["name"] == "Shape2"
        assert entry["members"][0]["name"] == "origin"
        assert entry["members"][0]["kind"] == "namedTuple"
        assert entry["members"][0]["members"][1] == {
            "name": "y",
            "kind": "scalar",
            "typeName": "std::float64",
        }


class TestBuildTypeEntryWithStructuralTuple:
    def test_pointer_shows_up_as_named_tuple_with_members(self):
        @pylon.type(module="geo", name="Shape3")
        class Shape3:
            label: str
            rgb: pylon.Tuple[("r", pylon.Int16), ("g", pylon.Int16), ("b", pylon.Int16)]

        entry = _build_type_entry(Shape3, set(), set())
        rgb = next(p for p in entry["pointers"] if p["name"] == "rgb")
        assert rgb["kind"] == "namedTuple"
        assert "target" not in rgb
        assert [m["name"] for m in rgb["members"]] == ["r", "g", "b"]
        assert rgb["required"] is True


class TestClassifyArrayPointer:
    def test_scalar_element(self):
        ann = pylon.Array[pylon.Str]
        result = _classify_pointer(ann, object, set(), None, set())
        assert result["kind"] == "array"
        assert result["element"] == {"name": None, "kind": "scalar", "typeName": "std::str"}

    def test_enum_element(self):
        @pylon.enum("Active", "Inactive")
        class Status(pylon.Enum):
            pass

        ann = pylon.Array[Status]
        result = _classify_pointer(ann, object, {Status}, None, set())
        assert result["element"] == {
            "name": None,
            "kind": "enum",
            "target": "test_asgi_schema::Status",
        }

    def test_tuple_element(self):
        ann = pylon.Array[pylon.Tuple[("x", pylon.Float64), ("y", pylon.Float64)]]
        result = _classify_pointer(ann, object, set(), None, set())
        assert result["element"]["kind"] == "namedTuple"
        assert result["element"]["members"] == [
            {"name": "x", "kind": "scalar", "typeName": "std::float64"},
            {"name": "y", "kind": "scalar", "typeName": "std::float64"},
        ]

    def test_bare_list_shorthand_classifies_the_same_as_array(self):
        ann = list[str]
        result = _classify_pointer(ann, object, set(), None, set())
        assert result["kind"] == "array"
        assert result["element"] == {"name": None, "kind": "scalar", "typeName": "std::str"}

    def test_optional_array_pointer_still_classifies_via_unwrap(self):
        ann = pylon.Array[pylon.Str] | None
        result = _classify_pointer(ann, object, set(), None, set())
        assert result["kind"] == "array"
        assert result["element"] == {"name": None, "kind": "scalar", "typeName": "std::str"}


class TestBuildTypeEntryWithArray:
    def test_pointer_shows_up_as_array_with_element(self):
        @pylon.type(module="geo", name="Tagged")
        class Tagged:
            label: str
            tags: pylon.Array[pylon.Str]

        entry = _build_type_entry(Tagged, set(), set())
        tags = next(p for p in entry["pointers"] if p["name"] == "tags")
        assert tags["kind"] == "array"
        assert tags["element"] == {"name": None, "kind": "scalar", "typeName": "std::str"}
        assert tags["required"] is True


class TestPointerEditabilityForJunctionBackedLink:
    """A junction-backed single link (`meta.through` set on a `link`-kind
    pointer) must report `through` *in addition to* its ordinary
    readonly/required/hasDefault fields — unlike a multilink, which only
    ever reports `through` alone."""

    def _link_meta(self, *, through: str | None, nullable: bool = True) -> PointerMeta:
        return PointerMeta(
            name="spouse", kind="link", scalar_type=None, nullable=nullable,
            constraints=[], default=dataclasses.MISSING, default_factory=dataclasses.MISSING,
            through=through,
        )

    def test_junction_backed_link_reports_through_and_editability(self):
        result = _pointer_editability(self._link_meta(through="default::Marriage"))
        assert result == {
            "readonly": False,
            "required": False,
            "hasDefault": False,
            "through": "default::Marriage",
        }

    def test_plain_link_has_no_through(self):
        result = _pointer_editability(self._link_meta(through=None, nullable=False))
        assert "through" not in result
        assert result == {"readonly": False, "required": True, "hasDefault": False}
