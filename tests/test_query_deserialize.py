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

"""Unit tests for pylon.query.deserialize and _decode."""

from __future__ import annotations

from dataclasses import dataclass
from unittest.mock import MagicMock

import pytest

from pylon.query import _decode, _decode_json_tuple, deserialize, shape_value_tags


# ── Helpers ──────────────────────────────────────────────────────────────────


@dataclass
class Person:
    name: str
    age: int | None = None


@dataclass
class Company:
    name: str


REGISTRY = {"Person": Person, "Company": Company}


def _scalar(name: str, pos: int) -> dict:
    return {"kind": "scalar", "name": name, "position": pos}


def _object(
    name: str,
    type_name: str,
    pointers: list,
    *,
    position: int = 0,
    cardinality: str = "many",
) -> dict:
    type_pointer = {"kind": "scalar", "name": "__type__", "position": 0}
    return {
        "kind": "object",
        "name": name,
        "type_name": type_name,
        "position": position,
        "cardinality": cardinality,
        "pointers": [type_pointer] + pointers,
    }


def _array(name: str, position: int, element: dict) -> dict:
    return {"kind": "array", "name": name, "position": position, "element": element}


# ── _decode scalar ────────────────────────────────────────────────────────────


class TestDecodeScalar:
    def test_reads_by_position(self):
        node = _scalar("age", 2)
        value = ("default::Person", "Alice", 30)
        assert _decode(value, node, {}) == 30

    def test_position_zero_is_type_disc(self):
        node = _scalar("__type__", 0)
        value = ("default::Person", "Alice")
        assert _decode(value, node, {}) == "default::Person"


# ── _decode object ────────────────────────────────────────────────────────────


class TestDecodeObject:
    def test_root_object_populates_dataclass(self):
        shape = _object(
            "",
            "default::Person",
            [_scalar("name", 1), _scalar("age", 2)],
        )
        # Root object: value IS the tuple (position 0 means "I am the root")
        value = ("default::Person", "Alice", 30)
        result = _decode(value, shape, REGISTRY)
        assert result == Person(name="Alice", age=30)

    def test_unknown_type_returns_dict(self):
        shape = _object("", "default::Unknown", [_scalar("name", 1)])
        value = ("default::Unknown", "foo")
        result = _decode(value, shape, {})
        assert result == {"name": "foo"}

    def test_nested_object_reads_from_position(self):
        company_shape = _object(
            "company",
            "default::Company",
            [_scalar("name", 1)],
            position=2,
            cardinality="optional",
        )
        # Outer tuple: (type, name, company_tuple)
        company_tuple = ("default::Company", "Acme")
        outer = ("default::Person", "Alice", company_tuple)
        result = _decode(outer, company_shape, REGISTRY)
        assert result == Company(name="Acme")

    def test_none_nested_object_returns_none(self):
        company_shape = _object(
            "company", "default::Company", [_scalar("name", 1)],
            position=2, cardinality="optional",
        )
        outer = ("default::Person", "Alice", None)
        assert _decode(outer, company_shape, REGISTRY) is None

    def test_free_object_int_field_does_not_crash(self):
        # Regression: a free object literal (`select { test := 1 }`) has no
        # schema type at all — type_name is None and there's no injected
        # __type__ discriminator, so obj_tuple[0] is just the first field's
        # raw value, not a type name. This used to be blindly treated as one,
        # crashing with "'int' object has no attribute 'split'" whenever that
        # first field held a non-zero int.
        shape = {
            "kind": "object",
            "name": "",
            "type_name": None,
            "position": 0,
            "cardinality": "many",
            "pointers": [_scalar("test", 0)],
        }
        value = (1,)
        assert _decode(value, shape, {}) == {"test": 1}


# ── _decode array ─────────────────────────────────────────────────────────────


class TestDecodeArray:
    def _posts_shape(self):
        return _array(
            "posts",
            position=2,
            element=_object("", "default::Post", [_scalar("title", 1)]),
        )

    def test_decodes_list_of_objects(self):
        @dataclass
        class Post:
            title: str

        registry = {"Post": Post}
        shape = self._posts_shape()
        post_tuple = ("default::Post", "Hello world")
        outer = ("default::Person", "Alice", [post_tuple])
        result = _decode(outer, shape, registry)
        assert result == [Post(title="Hello world")]

    def test_empty_array(self):
        shape = self._posts_shape()
        outer = ("default::Person", "Alice", [])
        assert _decode(outer, shape, {}) == []

    def test_null_array_treated_as_empty(self):
        shape = self._posts_shape()
        outer = ("default::Person", "Alice", None)
        assert _decode(outer, shape, {}) == []


# ── _decode named_tuple / _decode_json_tuple ───────────────────────────────────


def _member(key: str | None, kind: str = "scalar", **extra) -> dict:
    return {"key": key, "kind": kind, **extra}


def _named_tuple(
    name: str, position: int, members: list | None, type_name: str | None = None
) -> dict:
    return {
        "kind": "named_tuple",
        "name": name,
        "position": position,
        "type_name": type_name,
        "members": members,
    }


class TestDecodeJsonTuple:
    def test_positional_members_decode_to_real_tuple(self):
        node = _named_tuple("", 0, [_member(None), _member(None)])
        result = _decode(["1", 3], node, {})
        assert result == ("1", 3)
        assert isinstance(result, tuple)

    def test_named_members_no_registered_class_build_named_tuple_value(self):
        from pylon.datatypes import NamedTupleValue

        node = _named_tuple("", 0, [_member("street"), _member("zip")])
        result = _decode({"street": "123 Main St", "zip": "94107"}, node, {})
        assert isinstance(result, NamedTupleValue)
        assert isinstance(result, tuple)
        assert result.street == "123 Main St"
        assert result.zip == "94107"
        assert result == ("123 Main St", "94107")

    def test_named_members_with_registered_class_hydrates_dataclass(self):
        @dataclass
        class Point:
            x: float
            y: float

        node = _named_tuple("", 0, [_member("x"), _member("y")], type_name="Point")
        result = _decode({"x": 1.0, "y": 2.0}, node, {"Point": Point})
        assert result == Point(x=1.0, y=2.0)

    def test_nested_tuple_member_decodes_recursively(self):
        node = _named_tuple(
            "",
            0,
            [
                _member(
                    "origin",
                    kind="tuple",
                    members=[_member("x"), _member("y")],
                ),
                _member("size"),
            ],
        )
        value = {"origin": {"x": 1.0, "y": 2.0}, "size": 3.0}
        result = _decode(value, node, {})
        from pylon.datatypes import NamedTupleValue

        assert isinstance(result, NamedTupleValue)
        assert result.origin == NamedTupleValue(x=1.0, y=2.0)
        assert result.size == 3.0

    def test_enum_member_hydrates_registered_enum(self):
        from enum import Enum

        class Status(Enum):
            Active = "Active"

        node = _named_tuple(
            "", 0, [_member("status", kind="enum", enum_type="default::Status")]
        )
        result = _decode({"status": "Active"}, node, {"default::Status": Status})
        assert result.status is Status.Active

    def test_none_value_returns_none(self):
        node = _named_tuple("", 0, [_member(None)])
        assert _decode(None, node, {}) is None

    def test_no_members_falls_back_to_raw_dict(self):
        # Pre-existing behavior for shapes without statically-known member
        # info (e.g. a bare uncast named-tuple literal) — the raw jsonb dict
        # passes through unchanged, or hydrates a registered class from it
        # directly if one's registered.
        node = _named_tuple("", 0, None, type_name="Point")

        @dataclass
        class Point:
            x: float
            y: float

        result = _decode({"x": 1.0, "y": 2.0}, node, {"Point": Point})
        assert result == Point(x=1.0, y=2.0)

    def test_no_members_no_registered_class_returns_raw_dict(self):
        node = _named_tuple("", 0, None)
        assert _decode({"a": 1}, node, {}) == {"a": 1}

    def test_nested_position_reads_from_parent_tuple(self):
        # Nested named tuple sits at a positional index inside the parent
        # composite ROW (matches _decode's object/array pattern).
        node = _named_tuple("location", 2, [_member("x"), _member("y")])
        outer = ("default::Person", "Alice", {"x": 1.0, "y": 2.0})
        result = _decode(outer, node, {})
        from pylon.datatypes import NamedTupleValue

        assert result == NamedTupleValue(x=1.0, y=2.0)

    def test_decode_json_tuple_helper_directly(self):
        node = _named_tuple("", 0, [_member(None), _member(None)])
        assert _decode_json_tuple(["1", 3], node, {}) == ("1", 3)


# ── deserialize (top-level) ────────────────────────────────────────────────────


class TestDeserialize:
    def _make_query(self, shape: dict) -> MagicMock:
        q = MagicMock()
        q.shape = shape
        return q

    def test_empty_records(self):
        shape = _object("", "default::Person", [_scalar("name", 1)])
        query = self._make_query(shape)
        assert deserialize([], query, REGISTRY) == []

    def test_single_record(self):
        shape = _object("", "default::Person", [_scalar("name", 1), _scalar("age", 2)])
        query = self._make_query(shape)
        record = {"result": ("default::Person", "Alice", 30)}
        result = deserialize([record], query, REGISTRY)
        assert result == [Person(name="Alice", age=30)]

    def test_multiple_records(self):
        shape = _object("", "default::Person", [_scalar("name", 1)])
        query = self._make_query(shape)
        records = [
            {"result": ("default::Person", "Alice")},
            {"result": ("default::Person", "Bob")},
        ]
        result = deserialize(records, query, REGISTRY)
        assert result == [Person(name="Alice"), Person(name="Bob")]


# ── shape_value_tags ────────────────────────────────────────────────────────


class TestShapeValueTags:
    def test_scalar_and_raw_scalar_have_no_tag(self):
        assert shape_value_tags(_scalar("name", 1)) is None
        assert shape_value_tags({"kind": "raw_scalar"}) is None

    def test_bare_positional_tuple_reports_positional_members(self):
        node = {
            "kind": "tuple",
            "position": 0,
            "elements": [_scalar("", 0), {"kind": "enum", "name": "", "position": 1, "enum_type": "default::Status"}],
        }
        tags = shape_value_tags(node)
        assert tags == {
            "kind": "namedTuple",
            "typeName": None,
            "members": [
                {"key": None, "shape": None},
                {"key": None, "shape": {"kind": "enum", "enumType": "default::Status"}},
            ],
        }

    def test_named_tuple_with_members_reports_named_members(self):
        node = _named_tuple(
            "address",
            2,
            [_member("street"), _member("zip")],
        )
        tags = shape_value_tags(node)
        assert tags == {
            "kind": "namedTuple",
            "typeName": None,
            "members": [
                {"key": "street", "shape": None},
                {"key": "zip", "shape": None},
            ],
        }

    def test_named_tuple_without_members_reports_none_members(self):
        node = _named_tuple("location", 2, None, type_name="default::Point")
        assert shape_value_tags(node) == {"kind": "namedTuple", "typeName": "default::Point", "members": None}

    def test_nested_tuple_member_recurses(self):
        node = _named_tuple(
            "shape",
            2,
            [
                _member("origin", kind="tuple", members=[_member("x"), _member("y")]),
                _member("size"),
            ],
        )
        tags = shape_value_tags(node)
        assert tags["members"][0] == {
            "key": "origin",
            "shape": {
                "kind": "namedTuple",
                "typeName": None,
                "members": [{"key": "x", "shape": None}, {"key": "y", "shape": None}],
            },
        }

    def test_object_pointers_recurse_and_skip_type_discriminator(self):
        shape = _object(
            "",
            "default::Person",
            [
                _scalar("name", 1),
                {"kind": "enum", "name": "gender", "position": 2, "enum_type": "public::Gender"},
                _named_tuple("address", 3, [_member("street"), _member("zip")]),
            ],
        )
        tags = shape_value_tags(shape)
        assert tags["kind"] == "object"
        assert tags["typeName"] == "default::Person"
        assert "__type__" not in tags["pointers"]
        assert tags["pointers"]["name"] is None
        # Un-translated from Postgres "public" back to the real Pylon module.
        assert tags["pointers"]["gender"] == {"kind": "enum", "enumType": "default::Gender"}
        assert tags["pointers"]["address"]["kind"] == "namedTuple"

    def test_enum_type_default_module_unqualified_from_public_schema(self):
        node = {"kind": "enum", "name": "gender", "position": 1, "enum_type": "public::Gender"}
        assert shape_value_tags(node) == {"kind": "enum", "enumType": "default::Gender"}

    def test_enum_type_non_default_module_passes_through(self):
        node = {"kind": "enum", "name": "status", "position": 1, "enum_type": "shop::Status"}
        assert shape_value_tags(node) == {"kind": "enum", "enumType": "shop::Status"}

    def test_array_recurses_into_element(self):
        node = _array("posts", 2, {"kind": "enum", "name": "", "position": 0, "enum_type": "public::Status"})
        assert shape_value_tags(node) == {"kind": "array", "element": {"kind": "enum", "enumType": "default::Status"}}
