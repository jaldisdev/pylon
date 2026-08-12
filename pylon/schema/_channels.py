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
import re
from typing import Any

RESERVED_WIRE_NAME_PREFIX = 'pylon_'
"""Every Postgres channel Pylon's own internals ever LISTEN/NOTIFY on
(`pylon_index_queue`, `pylon_signal_queue`, `pylon_cache_invalidate` — see
`crates/pylon-core/src/stdlib/ddl.rs`) starts with this. A user `Channel`
landing on one of those exact names would silently share wire traffic with
whichever internal consumer is already listening on it, so the whole prefix
is reserved rather than just the three names in use today."""


class Channel:
    """A PostgreSQL pub/sub channel (`NOTIFY`/`LISTEN`), declared as a bound
    module-level value — not an annotation, since (unlike `Global`/`Alias`)
    it needs real instance state (an optional `name=` override).

    The payload can be:

    - A registered `@pylon.type`/`@pylon.interface` type — `notify()` sends
      the object, `listen()` decodes into that type.
    - A plain scalar (`str`, `uuid.UUID`, a registered custom scalar, ...).
    - An ad hoc named-field shape via `pylon.Object(...)`, passing each
      field's *type* as the keyword value rather than a data value — e.g.
      `pylon.Object(doc_id=uuid.UUID, score=float)`. Every field must be a
      scalar (same restriction as `Tuple`/`NamedTuple`).

    Usage::

        UserUpdates = pylon.Channel(User)
        SearchReady = pylon.Channel(pylon.Object(doc_id=uuid.UUID, score=float), name="search_ready")

    The Postgres channel identifier (the actual `NOTIFY`/`LISTEN` argument)
    is derived from the module + this variable's own name unless overridden
    by `name=` — see `_wire_name_for_channel`. Postgres channels have no
    schema namespacing at all (a flat, database-wide identifier space), so
    this is the one schema construct whose module is folded directly into
    its wire-visible name rather than kept as separate namespacing.
    """

    def __init__(self, payload_type: Any, *, name: str | None = None, description: str | None = None) -> None:
        self.payload_type = payload_type
        self.name_override = name
        self.description = description


@dataclasses.dataclass
class ChannelDescriptor:
    """Collected metadata for a single module-level Channel."""

    name: str
    module: str
    payload_type: Any
    description: str | None = None
    wire_name_override: str | None = None

    def __repr__(self) -> str:
        return f'ChannelDescriptor({self.name!r}, module={self.module!r}, payload_type={self.payload_type!r})'


def _to_snake_case(name: str) -> str:
    s1 = re.sub(r'(.)([A-Z][a-z]+)', r'\1_\2', name)
    return re.sub(r'([a-z0-9])([A-Z])', r'\1_\2', s1).lower()


def wire_name_for_channel(c: ChannelDescriptor) -> str:
    """The actual Postgres NOTIFY/LISTEN channel identifier for *c*.

    `name=` is used verbatim (full control, no casing applied). Otherwise
    `{module}__{snake_case(variable_name)}` — the module has to be folded in
    here (unlike every other named schema construct) since Postgres channels
    aren't schema-namespaced at all.
    """
    if c.wire_name_override is not None:
        return c.wire_name_override
    return f'{c.module}__{_to_snake_case(c.name)}'


def _infer_module_name(module: Any) -> str:
    override = getattr(module, '__pylon_module__', None)
    if isinstance(override, str):
        return override
    module_path = getattr(module, '__name__', 'default')
    return module_path.rpartition('.')[-1] or module_path


def collect_module_channels(module: Any) -> list[ChannelDescriptor]:
    """Scan a module's bound values (not annotations) for `Channel` instances.

    Unlike `Global`/`Alias` (annotation-based, found via `typing.get_type_hints`),
    a `Channel` is a real instantiated value assigned at module scope, so
    discovery scans `vars(module)` instead.
    """
    module_name = _infer_module_name(module)
    result: list[ChannelDescriptor] = []
    for name, value in vars(module).items():
        if name.startswith('_'):
            continue
        if not isinstance(value, Channel):
            continue
        result.append(
            ChannelDescriptor(
                name=name,
                module=module_name,
                payload_type=value.payload_type,
                description=value.description,
                wire_name_override=value.name_override,
            )
        )
    return result


# ── Runtime lookup + payload decode (used by Client.listen()) ─────────────────
#
# Everything below operates on the pyo3 `_core.ChannelDescriptor` /
# `_core.SchemaDescriptor` — the *migrated*, JSON-round-tripped runtime
# descriptors (see `SchemaDescriptor.channels`'s own doc comment) — not the
# build-time dataclasses above, which are discarded once `finalize()`/
# `migration create` have built the schema. A `Channel`'s own Python
# `payload_type` object (a live class or `pylon.Object` instance) never
# survives that round trip, which is exactly why decoding here works purely
# off the descriptor's `payload_kind`/`payload_*` fields (plain strings),
# not off the original Python type.


def resolve_channel(schema: Any, name: str) -> Any:
    """Find a declared Channel by bare or `module::name` reference.

    Mirrors `crates/pylon-core/src/ir/compiler.rs`'s `resolve_channel` (used
    by `notify()`), so the same reference a schema author writes in a PyQL
    `notify(...)` call also works as `Client.listen(...)`'s argument.
    """
    from pylon.exceptions import QueryError

    for channel in schema.channels:
        if channel.name == name or f'{channel.module}::{channel.name}' == name:
            return channel
    raise QueryError(f'listen(): {name!r} is not a known Channel')


def decode_channel_payload(channel: Any, raw_payload: str) -> Any:
    """Decode a raw NOTIFY payload string per *channel*'s declared shape.

    - `payload_kind == "type"`: the payload is the changed row's `id`,
      exactly what `notify()` sends for a Type-shaped channel — decodes to
      a bare `uuid.UUID`, not a fetched object (see `docs/schema/channels.md`).
    - `payload_kind == "scalar"`: decodes text -> the declared PostgreSQL
      type's natural Python value.
    - `payload_kind == "object"`: JSON-decodes the payload, then decodes
      each declared field's value by its own scalar type, and returns a
      `pylon.Object(**fields)` — the same class query results use for
      free-form shapes.

    Raises `pylon.exceptions.QueryError` (the same class every other
    runtime decode failure in this codebase surfaces as — see
    `pgcon_err` in `crates/pylon-py/src/pgcon.rs`) if the payload doesn't
    actually match the declared shape, chaining the original parse error.
    Left to propagate by `Client.listen()` — a malformed payload ends that
    listen loop rather than being silently skipped.
    """
    from pylon.exceptions import QueryError

    try:
        if channel.payload_kind == 'type':
            return _decode_scalar_text(raw_payload, 'uuid')
        if channel.payload_kind == 'scalar':
            return _decode_scalar_text(raw_payload, channel.payload_scalar_pg_type)
        # "object"
        import json

        from pylon.datatypes import Object as _PylonObject

        decoded = json.loads(raw_payload)
        fields = {name: _decode_json_value(decoded[name], pg_type) for name, pg_type in channel.payload_object_fields}
        return _PylonObject(**fields)
    except QueryError:
        raise
    except Exception as exc:
        raise QueryError(
            f"listen(): payload on channel {channel.wire_name!r} doesn't match its declared shape: {exc}"
        ) from exc


def _decode_scalar_text(text: str, pg_type: str) -> Any:
    """Decode NOTIFY's raw text payload as PostgreSQL's own `<pg_type>::text`
    cast would have rendered it (see `notify()`'s SQL emission — the
    payload is always literally cast to `text` before being sent).

    Covers every base type `PG_TYPE_MAP` (`_scalars.py`) maps a built-in
    scalar to, except `interval`/`bytea` — their text encodings are
    non-trivial to parse correctly and uncommon as a pub/sub payload; both
    currently pass through as the raw string rather than risk a wrong
    decode.
    """
    import datetime
    import decimal
    import uuid as _uuid

    if pg_type == 'uuid':
        return _uuid.UUID(text)
    if pg_type in ('int2', 'int4', 'int8'):
        return int(text)
    if pg_type in ('float4', 'float8'):
        return float(text)
    if pg_type == 'numeric':
        return decimal.Decimal(text)
    if pg_type == 'boolean':
        return text in ('true', 't')
    if pg_type == 'timestamptz' or pg_type == 'timestamp':
        return datetime.datetime.fromisoformat(text)
    if pg_type == 'date':
        return datetime.date.fromisoformat(text)
    if pg_type == 'time':
        return datetime.time.fromisoformat(text)
    # text, jsonb (sent as its own text representation, not re-parsed),
    # interval, bytea, and anything else: raw string.
    return text


def _decode_json_value(value: Any, pg_type: str) -> Any:
    """Like `_decode_scalar_text`, but for a value already parsed out of an
    Object channel's JSON payload — `json.loads` already turned a JSON
    number/bool/null into the right Python primitive, so only the types
    `to_jsonb()` renders as a JSON *string* (uuid, numeric, date/time,
    interval, bytea) need any further parsing here.
    """
    if value is None:
        return None
    if not isinstance(value, str):
        return value
    return _decode_scalar_text(value, pg_type)
