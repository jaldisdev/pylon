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

import contextlib
import dataclasses
from typing import Any

_object_class_cache: dict[tuple[str, ...], type] = {}
_named_tuple_value_cache: dict[tuple[str, ...], type] = {}


class PylonSet(list):
    """A Pylon set value — displayed as {a, b, c} instead of [a, b, c].

    Used for multi-link pointers and any other set-valued computed results.
    """


def _junction_pointers(through: Any) -> set[str] | None:
    """Property names declared by a junction class, or None if it can't be
    resolved (`through` is the class before the schema is walked and its
    `module::Name` afterwards)."""
    cls = through
    if isinstance(through, str):
        from pylon.schema._registry import snapshot

        types, _enums, _scalars = snapshot()
        cls = next(
            (
                t
                for t in types
                if getattr(t, '__pylon_config__', None) is not None
                and f'{t.__pylon_config__.module}::{t.__pylon_config__.name}' == through
            ),
            None,
        )
    cfg = getattr(cls, '__pylon_config__', None)
    if cfg is None:
        return None
    return {name for name, meta in cfg.pointers.items() if meta.kind == 'property' and name != 'id'}


class Unfetched:
    """The class-level default of a field a query's shape may not have selected.

    A non-data descriptor: when hydration wrote the field into the instance's
    `__dict__` the instance wins and this never runs, so a selected field costs
    nothing to read and a selected-but-null one still reads as `None`. It runs
    only when the field is absent — which, for a hydrated row, means the shape
    did not ask for it.

    Raising there matters more than it sounds: a field that reads as `None`
    whether it was null or never fetched is indistinguishable from a legitimate
    value, which is how an assessment answer once got written with `[None]` for
    its options. The same reasoning as `LinkSet`'s unhydrated mode, for the
    pointers that aren't multi-links.

    `__get__` off the class returns `fallback` rather than raising, because that
    is what `dataclasses` reads to build `__init__` — so constructing an
    instance still gives the field its declared default.

    `name` is passed in because `__set_name__` only fires for an attribute set
    while the class body is executing; these are installed with `setattr` after
    the fact (see `_prepare_dataclass`), so it never runs for them.

    Only rows a query produced raise. A constructed instance was never given a
    shape to leave anything out of, and its `init=False` pointers — computeds —
    have no value yet by construction rather than by omission, so those keep
    reading as their declared default. Hydration is what marks a row, by
    writing `__pylon_type__` into its `__dict__`.
    """

    __slots__ = ('_fallback', '_name')

    def __init__(self, fallback: Any = None, *, name: str = '<unknown>') -> None:
        self._fallback = fallback
        self._name = name

    def __set_name__(self, owner: type, name: str) -> None:
        self._name = name

    def __get__(self, instance: Any, owner: type | None = None) -> Any:
        if instance is None or '__pylon_type__' not in instance.__dict__:
            return self._fallback
        raise AttributeError(
            f'{type(instance).__name__!r} object has no attribute {self._name!r} — it was not '
            'selected by the query that produced it. Request it in the query shape to read it.'
        )

    def __repr__(self) -> str:
        return f'<Unfetched {self._name}>'


class LinkSet(PylonSet):
    """A multi-link's value on a model instance, which also records the
    mutations applied to it so `client.save()` can replay them.

    `+=` and `-=` can't be recovered from a before/after diff: appending a
    member that is already linked is a no-op server-side, and removing one
    that isn't linked is too, so neither shows up as a state change. The ops
    are therefore recorded as they happen rather than reconstructed.

    **Unhydrated mode.** Links aren't fetched by default — adding them to
    every query's shape would make `client.query(Person)` fan out across
    tables for data most callers don't want. But PyQL's `+=`/`-=` run
    server-side, so appending needs no knowledge of the current members.
    A multi-link that wasn't fetched is therefore an *unhydrated* LinkSet:
    it accepts mutations and refuses reads, rather than presenting an empty
    list that isn't really empty.
    """

    __slots__ = ('_ops', '_owner', '_pointer', '_unhydrated')

    def __init__(
        self,
        items: Any = (),
        *,
        unhydrated: bool = False,
        pointer: Any = None,
        owner: str | None = None,
    ) -> None:
        super().__init__(() if unhydrated else items)
        self._unhydrated = unhydrated
        #: The owning `PointerMeta`, when known — lets `add()` validate link
        #: property names against the junction as they're written. Absent for
        #: a LinkSet constructed directly.
        self._pointer = pointer
        #: The name of the class the pointer belongs to, so a refused read can
        #: say which one it was. Hydration passes it; a directly constructed
        #: LinkSet has no owner to name.
        self._owner = owner
        #: Recorded mutations, in order:
        #: ('add' | 'remove' | 'set', [values], {link_prop: value}).
        #: The property dict is empty for everything except `add()`.
        self._ops: list[tuple[str, list[Any], dict[str, Any]]] = []

    # ── Reads ────────────────────────────────────────────────────────────────

    def _pointer_label(self) -> str:
        name = getattr(self._pointer, 'name', None)
        if name is None:
            return 'a multi-link'
        return f'{self._owner}.{name}' if self._owner else name

    def _check_readable(self, action: str) -> None:
        if self._unhydrated:
            raise AttributeError(
                f'cannot {action} {self._pointer_label()} — it was not fetched, and multi-links '
                'are not loaded by default. Request it in the query shape to read it, or use '
                '+= / -= to modify it without loading it.'
            )

    def __iter__(self):
        self._check_readable('iterate')
        return super().__iter__()

    def __len__(self) -> int:
        self._check_readable('take the length of')
        return super().__len__()

    def __getitem__(self, index):
        self._check_readable('index')
        return super().__getitem__(index)

    def __contains__(self, item) -> bool:
        self._check_readable('test membership on')
        return super().__contains__(item)

    def __eq__(self, other) -> bool:
        self._check_readable('compare')
        return super().__eq__(other)

    def __ne__(self, other) -> bool:
        self._check_readable('compare')
        return super().__ne__(other)

    __hash__ = None  # type: ignore[assignment]

    def __repr__(self) -> str:
        if self._unhydrated:
            return '<LinkSet not fetched>'
        return super().__repr__()

    @property
    def is_hydrated(self) -> bool:
        return not self._unhydrated

    # ── Mutations ────────────────────────────────────────────────────────────

    @staticmethod
    def _as_items(other: Any) -> list[Any]:
        # A single model instance is the common case (`bob.friends += alice`),
        # so accept it alongside an iterable of them. A str is iterable but
        # never a valid link target, so reject it rather than silently
        # splitting it into characters.
        if isinstance(other, (str, bytes)) or not hasattr(other, '__iter__'):
            return [other]
        return list(other)

    def __iadd__(self, other: Any) -> 'LinkSet':  # type: ignore[override]
        items = self._as_items(other)
        self._ops.append(('add', items, {}))
        if not self._unhydrated:
            super().extend(items)
        # Returning self keeps `x.links += [y]` a no-op at the attribute
        # level: Python reassigns the same object it already held.
        return self

    def __isub__(self, other: Any) -> 'LinkSet':
        items = self._as_items(other)
        self._ops.append(('remove', items, {}))
        if not self._unhydrated:
            for item in items:
                # Removing something not currently linked is a no-op
                # server-side; mirror that rather than raising.
                with contextlib.suppress(ValueError):
                    super().remove(item)
        return self

    def add(self, target: Any, **link_props: Any) -> None:
        """Link `target`, setting link properties on the junction.

        `+=` covers the ordinary case; this is for a `Through[...]` link
        whose junction carries properties::

            product.tags += [plain]                    # no properties
            product.tags.add(featured, weight=1.0)     # with properties

        One target per call, because PyQL attaches `@prop` values to the
        *whole* selection of a `+=` clause — two targets with different
        values are two clauses, so a call maps to exactly one. Calls that
        share the same values are merged back into one clause when rendered.

        There is no `remove()` counterpart: the compiler rejects link
        properties when unlinking ("link properties (`@prop := value`) cannot
        be assigned when removing a link"), so `-=` remains the only spelling.
        """
        if link_props:
            self._check_link_props(link_props)
        self._ops.append(('add', [target], dict(link_props)))
        if not self._unhydrated:
            super().append(target)

    def _check_link_props(self, link_props: dict[str, Any]) -> None:
        """Reject a property the junction doesn't declare, while the call
        that named it is still the thing in front of you."""
        from pylon.exceptions import InterfaceError

        pointer = self._pointer
        if pointer is None or getattr(pointer, 'through', None) is None:
            if pointer is not None:
                raise InterfaceError(
                    f'{pointer.name} has no junction, so it has no link properties to set — '
                    f'use `+=` instead of add(..., {next(iter(link_props))}=...)'
                )
            return

        junction = _junction_pointers(pointer.through)
        if junction is None:
            return
        unknown = [name for name in link_props if name not in junction]
        if unknown:
            import difflib

            hint = ''
            close = difflib.get_close_matches(unknown[0], junction, n=1, cutoff=0.7)
            if close:
                hint = f' — did you mean {close[0]}?'
            known = ', '.join(sorted(junction)) or 'none'
            raise InterfaceError(
                f'unknown link {"properties" if len(unknown) > 1 else "property"} '
                f'{", ".join(sorted(unknown))} on {pointer.name}{hint} (declared: {known})'
            )

    # ── Op log ───────────────────────────────────────────────────────────────

    @property
    def ops(self) -> list[tuple[str, list[Any], dict[str, Any]]]:
        return self._ops

    def clear_ops(self) -> None:
        """Called after a successful save so mutations aren't replayed."""
        self._ops = []


@dataclasses.dataclass(frozen=True)
class Range:
    """A PostgreSQL range value (`range<T>`) — `lower`/`upper` are `None`
    for an unbounded side, not for an empty range (see `empty`).

    Usage::

        r = Range(lower=1, upper=10, inc_lower=True, inc_upper=False)
        r.lower, r.upper          # 1, 10
    """

    lower: Any = None
    upper: Any = None
    inc_lower: bool = True
    inc_upper: bool = False
    empty: bool = False

    def __repr__(self) -> str:
        if self.empty:
            return 'Range(empty=True)'
        lo = '' if self.lower is None else repr(self.lower)
        hi = '' if self.upper is None else repr(self.upper)
        open_b = '[' if self.inc_lower else '('
        close_b = ']' if self.inc_upper else ')'
        return f'Range({open_b}{lo}, {hi}{close_b})'


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

    def __new__(cls, **kwargs: Any) -> 'Object':
        if cls is Object:
            field_names = tuple(kwargs.keys())
            if field_names not in _object_class_cache:
                _object_class_cache[field_names] = dataclasses.make_dataclass(
                    'Object',
                    [(name, Any) for name in field_names],
                    bases=(Object,),
                )
            return object.__new__(_object_class_cache[field_names])
        return object.__new__(cls)


class NamedTupleValue(tuple):
    """A named-tuple *value*, not Object.

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

    def __new__(cls, **kwargs: Any) -> 'NamedTupleValue':
        if cls is NamedTupleValue:
            field_names = tuple(kwargs.keys())
            if field_names not in _named_tuple_value_cache:
                _named_tuple_value_cache[field_names] = type(
                    'NamedTupleValue',
                    (NamedTupleValue,),
                    {'_fields': field_names},
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
        members = ', '.join(f'{name} := {value!r}' for name, value in zip(self._fields, self, strict=False))
        return f'({members})'
