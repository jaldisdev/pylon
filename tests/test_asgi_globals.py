"""Unit tests for pylon.server.asgi's _global_type_text — the /api/globals
typeName resolver. Same style as test_asgi_schema.py: exercises the module's
pure function directly against real Pylon annotation objects rather than
spinning up the ASGI app."""

import pylon.schema as pylon
from pylon.server.asgi import _global_type_text


class TestGlobalTypeTextScalar:
    def test_builtin_scalar_resolves_to_qualified_name(self):
        assert _global_type_text(pylon.Str, set(), set()) == "std::str"
        assert _global_type_text(pylon.UUID, set(), set()) == "std::uuid"

    def test_unresolvable_custom_scalar_returns_none(self):
        @pylon.scalar(pylon.Str)
        class Slug(pylon.Scalar):
            pass

        assert _global_type_text(Slug, set(), set()) is None


class TestGlobalTypeTextEnum:
    def test_enum_resolves_to_qualified_name(self):
        @pylon.enum("Active", "Inactive")
        class Status(pylon.Enum):
            pass

        assert _global_type_text(Status, {Status}, set()) == "test_asgi_globals::Status"


class TestGlobalTypeTextNamedTuple:
    def test_nominal_named_tuple_resolves_to_qualified_name(self):
        @pylon.named_tuple
        class Point(pylon.NamedTuple):
            x: pylon.Float64
            y: pylon.Float64

        assert _global_type_text(Point, set(), {Point}) == "test_asgi_globals::Point"


class TestGlobalTypeTextStructuralTuple:
    def test_unnamed_elements(self):
        ann = pylon.Tuple[pylon.Str, pylon.Bool]
        assert _global_type_text(ann, set(), set()) == "tuple<std::str, std::bool>"

    def test_named_elements(self):
        ann = pylon.Tuple[("x", pylon.Float64), ("y", pylon.Float64)]
        assert _global_type_text(ann, set(), set()) == "tuple<x: std::float64, y: std::float64>"

    def test_nested_tuple_element(self):
        ann = pylon.Tuple[
            ("origin", pylon.Tuple[("x", pylon.Float64), ("y", pylon.Float64)]),
            ("size", pylon.Float64),
        ]
        assert _global_type_text(ann, set(), set()) == (
            "tuple<origin: tuple<x: std::float64, y: std::float64>, size: std::float64>"
        )

    def test_enum_element(self):
        @pylon.enum("Active", "Inactive")
        class Status2(pylon.Enum):
            pass

        ann = pylon.Tuple[("status", Status2)]
        assert _global_type_text(ann, {Status2}, set()) == "tuple<status: test_asgi_globals::Status2>"

    def test_element_unresolvable_bails_out_to_none(self):
        @pylon.scalar(pylon.Str)
        class Slug2(pylon.Scalar):
            pass

        ann = pylon.Tuple[("s", Slug2)]
        assert _global_type_text(ann, set(), set()) is None


class TestGlobalTypeTextArray:
    def test_pylon_array(self):
        assert _global_type_text(pylon.Array[pylon.Str], set(), set()) == "array<std::str>"

    def test_bare_list_shorthand(self):
        assert _global_type_text(list[str], set(), set()) == "array<std::str>"

    def test_array_of_tuple(self):
        ann = pylon.Array[pylon.Tuple[("x", pylon.Float64), ("y", pylon.Float64)]]
        assert _global_type_text(ann, set(), set()) == "array<tuple<x: std::float64, y: std::float64>>"
