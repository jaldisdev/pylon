import dataclasses
from typing import Any

_object_class_cache: dict[tuple[str, ...], type] = {}
_named_tuple_value_cache: dict[tuple[str, ...], type] = {}


class PylonSet(list):
    """A Pylon set value — displayed as {a, b, c} instead of [a, b, c].

    Used for multi-link pointers and any other set-valued computed results.
    """


class Object:
    """Result container for free-form PyQL queries.

    Instances are proper dataclasses so dataclasses.asdict() and similar
    tools work on them. Field names come from the keyword arguments.

    Usage::

        result = Object(name='hello', count=42)
        result.name                     # 'hello'
        dataclasses.asdict(result)      # {'name': 'hello', 'count': 42}
        isinstance(result, Object)      # True
    """

    def __new__(cls, **kwargs: Any) -> "Object":
        if cls is Object:
            field_names = tuple(kwargs.keys())
            if field_names not in _object_class_cache:
                _object_class_cache[field_names] = dataclasses.make_dataclass(
                    "Object",
                    [(name, Any) for name in field_names],
                    bases=(Object,),
                )
            return object.__new__(_object_class_cache[field_names])
        return object.__new__(cls)


class NamedTupleValue(tuple):
    """A named-tuple *value* — mirrors the upstream NamedTuple, not Object.

    A named tuple is a tuple: its members are ordered and positionally
    indexable/iterable/comparable like any tuple, and each member is also
    reachable by name. That's a different shape than Object (a keyword-only
    dataclass for free-form query results) and the distinction matters — a
    tuple<x: ..., y: ...> value should behave like the tuple it is.

    Usage::

        p = NamedTupleValue(x=1, y=2)
        p[0]                      # 1
        p.x                       # 1
        tuple(p)                  # (1, 2)
        repr(p)                   # '(x := 1, y := 2)'
    """

    _fields: tuple[str, ...] = ()

    def __new__(cls, **kwargs: Any) -> "NamedTupleValue":
        if cls is NamedTupleValue:
            field_names = tuple(kwargs.keys())
            if field_names not in _named_tuple_value_cache:
                _named_tuple_value_cache[field_names] = type(
                    "NamedTupleValue",
                    (NamedTupleValue,),
                    {"_fields": field_names},
                )
            cls = _named_tuple_value_cache[field_names]
            return tuple.__new__(cls, kwargs.values())
        return tuple.__new__(cls, (kwargs[name] for name in cls._fields))

    def __getattr__(self, name: str) -> Any:
        try:
            return self[self._fields.index(name)]
        except ValueError:
            raise AttributeError(name) from None

    def __repr__(self) -> str:
        members = ", ".join(f"{name} := {value!r}" for name, value in zip(self._fields, self))
        return f"({members})"
