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

from typing import TYPE_CHECKING, Any

if TYPE_CHECKING:
    from pylon._core import CompiledQuery, SchemaDescriptor

_singleton: SchemaDescriptor | None = None


def compile(
    query: str,
    *,
    schema: SchemaDescriptor | None = None,
    allow_user_specified_id: bool = False,
) -> CompiledQuery:
    """Compile a PyQL string to SQL. Raises PyQLError (or a subclass) on failure.

    If schema is omitted, falls back to the process-level singleton SchemaDescriptor.
    Synchronous — compilation is CPU-bound; async lives at the DB execution layer.

    ``allow_user_specified_id`` is a session config option — see
    ``pylon.config_options`` for the full registry exposed to clients
    (``Client.with_config()``) and the frontend.
    """
    from pylon._core import compile as _core_compile

    return _core_compile(
        query,
        schema if schema is not None else _get_schema(),
        allow_user_specified_id=allow_user_specified_id,
    )


def deserialize(
    records: list,
    query: CompiledQuery,
    registry: dict[str, type],
) -> list:
    """Decode rows into Python objects using the shape embedded in query.

    Each record must have a ``result`` column containing the anonymous PostgreSQL
    record tuple produced by the compiled SQL.  The shape descriptor in ``query``
    drives the decoding; ``registry`` maps short type names to dataclass types.
    """
    shape = query.shape
    return [_decode(record['result'], shape, registry) for record in records]


def _decode_json_member(value: Any, node: dict, registry: dict[str, type]) -> Any:
    """Decode one member's own value within a jsonb-backed tuple — the
    recursive counterpart of `_decode`'s composite-ROW-position walk, but for
    a jsonb dict/list's own keys/positions instead."""
    kind = node['kind']
    if kind == 'enum':
        if value is None:
            return None
        enum_type = node['enum_type']
        short = enum_type.split('::')[-1]
        cls = registry.get(enum_type) or registry.get(short)
        return cls(value) if cls is not None else value
    if kind == 'tuple':
        return _decode_json_tuple(value, node, registry)
    return value  # "scalar" — jsonb's own native JSON type is already correct


def _decode_json_tuple(value: Any, node: dict, registry: dict[str, type]) -> Any:
    """Decode a jsonb tuple/named-tuple value using its statically-known
    member shape (`node["members"]`) — a real Python tuple for positional
    members, the registered dataclass for a nominal type, or a
    pylon.datatypes.NamedTupleValue for an unregistered structural named
    tuple. Falls back to the raw jsonb value when no member shape was
    available at compile time."""
    if value is None:
        return None

    members = node.get('members')
    type_name = node.get('type_name')
    if members is None:
        if type_name and isinstance(value, dict):
            cls = registry.get(type_name)
            if cls is not None:
                return cls(**value)
        return value

    positional = all(m['key'] is None for m in members)
    if positional:
        return tuple(
            _decode_json_member(value[i] if isinstance(value, list) else None, m, registry)
            for i, m in enumerate(members)
        )

    kwargs = {
        m['key']: _decode_json_member(value.get(m['key']) if isinstance(value, dict) else None, m, registry)
        for m in members
    }
    if type_name:
        cls = registry.get(type_name)
        if cls is not None:
            return cls(**kwargs)
    from pylon.datatypes import NamedTupleValue

    return NamedTupleValue(**kwargs)


def _install_link_sets(obj: Any, cls: type, kwargs: dict) -> None:
    """Give every multi-link on a freshly decoded instance a `LinkSet`.

    A multi-link that *was* requested in the shape gets a hydrated one
    wrapping the decoded members. One that wasn't gets an unhydrated
    placeholder rather than being left absent: reading it should say "this
    wasn't fetched" instead of raising a bare `AttributeError`, and `+=`/`-=`
    should still work on it, since PyQL applies those server-side without
    needing the current members.

    Multi-links are excluded from `__pylon_saved__` — they are tracked by the
    LinkSet's own op log, not by diffing (see `pylon.datatypes.LinkSet`).
    """
    from pylon.datatypes import LinkSet

    cfg = getattr(cls, '__pylon_config__', None)
    if cfg is None:
        return

    saved = obj.__dict__.get('__pylon_saved__')
    for name, meta in cfg.pointers.items():
        if meta.kind != 'multilink':
            continue
        if name in kwargs:
            obj.__dict__[name] = LinkSet(kwargs[name] or (), pointer=meta)
        else:
            obj.__dict__[name] = LinkSet(unhydrated=True, pointer=meta)
        if saved is not None:
            saved.pop(name, None)


def _decode(value: Any, node: dict, registry: dict[str, type]) -> Any:
    kind = node['kind']

    if kind == 'scalar':
        return value[node['position']]

    if kind in ('raw_scalar', 'json_scalar'):
        return value

    if kind == 'object':
        pos = node['position']
        # Root object sits at the top level; nested objects are at a tuple position.
        obj_tuple = value if pos == 0 else value[pos]
        if obj_tuple is None:
            return None
        pointers = node['pointers']
        # pointers[0] is always the auto-injected __type__ discriminator (position 0); skip it.
        # Explicit __type__ requested by the user appears at position > 0 and is included.
        kwargs = {
            p['name']: _decode(obj_tuple, p, registry)
            for p in pointers
            if not (p['name'] == '__type__' and p['position'] == 0)
        }
        # A free object literal (`select { a := 1 }`) has no schema type at
        # all — node["type_name"] is None and obj_tuple has no injected
        # __type__ discriminator, so obj_tuple[0] there is just the first
        # user field's raw value, not a type name. Only schema-backed
        # objects (type_name is always set for those) carry that
        # discriminator, so only look for it in that case.
        static_type_name = node.get('type_name')
        if static_type_name:
            # Use the actual per-row __type__ value (obj_tuple[0]) for class
            # lookup. For concrete types it equals the static type_name; for
            # polymorphic (interface) queries it gives the real concrete type.
            actual_type = obj_tuple[0] if obj_tuple else None
            type_name = actual_type or static_type_name
            short = type_name.split('::')[-1]
            cls = registry.get(short)
            if cls is not None:
                obj = object.__new__(cls)
                obj.__dict__.update(kwargs)
                obj.__dict__['__pylon_type__'] = type_name
                # Shadow copy of the hydrated field values — lets
                # Client.save() diff current vs. persisted state instead of
                # intercepting every __setattr__ (see pylon.modelquery).
                obj.__dict__['__pylon_saved__'] = dict(kwargs)
                _install_link_sets(obj, cls, kwargs)
                return obj
        return kwargs

    if kind == 'named_tuple':
        pos = node.get('position', 0)
        # Root-level named tuples arrive as the raw decoded jsonb value (dict
        # for named members, list for positional/unnamed ones); nested ones
        # sit at a positional index inside the parent composite row.
        raw = value if (value is None or isinstance(value, (dict, list))) else value[pos]
        return _decode_json_tuple(raw, node, registry)

    if kind == 'enum':
        raw = value[node['position']]
        if raw is None:
            return None
        enum_type = node['enum_type']
        short = enum_type.split('::')[-1]
        cls = registry.get(enum_type) or registry.get(short)
        if cls is not None:
            return cls(raw)
        return raw

    if kind == 'array':
        from pylon.datatypes import PylonSet

        arr = value[node['position']] or []
        element = node['element']
        # Array elements are anonymous records; decode each one as a root object.
        return PylonSet(_decode(item, {**element, 'position': 0}, registry) for item in arr)

    if kind == 'tuple':
        return tuple(_decode(value, e, registry) for e in node['elements'])

    if kind == 'group':
        key_nodes = node['key_nodes']
        key_obj = {kn['name']: _decode(value, kn, registry) for kn in key_nodes}
        grouping = list(value[node['grouping_position']] or [])
        elements = [
            _decode(item, {**node['element'], 'position': 0}, registry)
            for item in (value[node['elements_position']] or [])
        ]
        return {'key': key_obj, 'grouping': grouping, 'elements': elements}

    if kind == 'vector_search':
        # Outer tuple: (NULL, object_record, distance_float)
        obj_tuple = value[node['object_position']]
        distance = value[node['distance_position']]
        obj = _decode(obj_tuple, {**node['object_node'], 'position': 0}, registry)
        return {'object': obj, 'distance': distance}

    if kind == 'fts_search':
        # Outer tuple: (NULL, object_record, score_float)
        obj_tuple = value[node['object_position']]
        score = value[node['rank_position']]
        obj = _decode(obj_tuple, {**node['object_node'], 'position': 0}, registry)
        return {'object': obj, 'score': score}

    raise ValueError(f'unknown shape node kind: {kind!r}')


def _pg_schema_to_pylon_module(pg_schema: str) -> str:
    """Reverse of `_walker.py`'s `_pg_schema()` (module -> pg schema name):
    only the "default" module is ever renamed (to Postgres "public"), so
    that's the only translation to undo — every other schema name is
    already a real Pylon module name."""
    return 'default' if pg_schema == 'public' else pg_schema


def _pylon_qualify_enum_type(enum_type: str) -> str:
    """`ShapeNode::Enum.enum_type` carries the Postgres-schema-qualified form
    (e.g. "public::Gender") for the decode registry lookup in `_decode()`,
    which falls back to a short-name match — but the frontend's /api/schema-
    driven enum lookup needs the real Pylon-qualified name ("default::Gender")."""
    if '::' not in enum_type:
        return enum_type
    pg_schema, name = enum_type.split('::', 1)
    return f'{_pg_schema_to_pylon_module(pg_schema)}::{name}'


def _member_shape_tag(m: dict) -> Any:
    """Value-tree tag for one JsonMember node (see `_decode_json_member`) —
    the recursive counterpart of `shape_value_tags` for a tuple's own
    members, which use a slightly different dict shape (`kind`/`enum_type`/
    `members` directly, no `position`)."""
    kind = m['kind']
    if kind == 'enum':
        return {'kind': 'enum', 'enumType': m['enum_type']}
    if kind == 'tuple':
        return {
            'kind': 'namedTuple',
            'typeName': m.get('type_name'),
            'members': [{'key': mm['key'], 'shape': _member_shape_tag(mm)} for mm in m['members']],
        }
    return None


def shape_value_tags(node: dict) -> Any:
    """Convert a compiled query's position-based shape descriptor into a
    value-tree-aligned "tag tree" — mirrors the structure of the already-
    decoded JSON value (`_to_jsonable`'s output), with no positions, so the
    frontend can walk it alongside the response body to render type tags
    (`<uuid>`, enum labels, the `(x := 1, y := 2)` tuple literal syntax)
    for values that aren't a known schema pointer — e.g. a bare top-level
    cast, or a tuple nested inside a free object — the same way it already
    does for object properties via /api/schema."""
    kind = node['kind']
    if kind == 'enum':
        # Unlike JsonMember's own enum_type (already Pylon-module-qualified —
        # see _member_shape_tag), ShapeNode::Enum's enum_type is built from
        # the Postgres schema name, so "default" needs un-translating back
        # from "public" for the frontend's /api/schema-driven enum lookup.
        return {'kind': 'enum', 'enumType': _pylon_qualify_enum_type(node['enum_type'])}
    if kind == 'named_tuple':
        # A nested free object (`test := { foo := 'bar' }`, e.g. inside a
        # computed shape element) compiles to the exact same IR/shape node
        # as a real named-tuple literal (`test := (foo := 'bar')`) — same
        # jsonb encoding either way — but the two need different frontend
        # display (an expandable `Object {foo: 'bar'}` vs. a non-
        # expandable `(foo := 'bar')` tuple literal). is_free_object (see
        # ShapeNode::NamedTuple in query/mod.rs) carries that distinction
        # through from the original curly-brace-vs-paren PyQL syntax.
        if node.get('is_free_object'):
            return {'kind': 'object', 'typeName': None, 'pointers': {}}
        members = node.get('members')
        return {
            'kind': 'namedTuple',
            'typeName': node.get('type_name'),
            'members': None if members is None else [{'key': m['key'], 'shape': _member_shape_tag(m)} for m in members],
        }
    if kind == 'tuple':
        return {
            'kind': 'namedTuple',
            'typeName': None,
            'members': [{'key': None, 'shape': shape_value_tags(e)} for e in node['elements']],
        }
    if kind == 'object':
        return {
            'kind': 'object',
            'typeName': node.get('type_name'),
            'pointers': {p['name']: shape_value_tags(p) for p in node['pointers'] if p['name'] != '__type__'},
        }
    if kind == 'array':
        return {'kind': 'array', 'element': shape_value_tags(node['element'])}
    return None


def _get_schema() -> SchemaDescriptor:
    if _singleton is None:
        raise RuntimeError(
            'No SchemaDescriptor singleton has been installed. '
            'Either pass schema= explicitly or call pylon.query._set_schema() '
            'from your framework startup hook (AppConfig.ready() / ASGI lifespan).'
        )
    return _singleton


def _set_schema(schema: SchemaDescriptor) -> None:
    """Install the process-level SchemaDescriptor singleton.

    Called by framework startup hooks to eagerly set the singleton before
    any queries are compiled.
    """
    from pylon._core import clear_query_cache

    global _singleton
    _singleton = schema
    clear_query_cache()
