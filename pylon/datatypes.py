import dataclasses
from typing import Any

_object_class_cache: dict[tuple[str, ...], type] = {}


class PylonSet(list):
    """A Pylon set value — displayed as {a, b, c} instead of [a, b, c].

    Used for multi-link fields and any other set-valued computed results.
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
