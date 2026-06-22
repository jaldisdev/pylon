"""Unit tests for pylon.query.deserialize and _decode."""

from __future__ import annotations

from dataclasses import dataclass
from unittest.mock import MagicMock

import pytest

from pylon.query import _decode, deserialize


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
    fields: list,
    *,
    position: int = 0,
    cardinality: str = "many",
) -> dict:
    type_field = {"kind": "scalar", "name": "__type__", "position": 0}
    return {
        "kind": "object",
        "name": name,
        "type_name": type_name,
        "position": position,
        "cardinality": cardinality,
        "fields": [type_field] + fields,
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
