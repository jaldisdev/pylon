"""Hand-rolled ASGI app for `pylon serve` — no web framework, just this callable
under uvicorn. It owns its own routing, JSON body/response handling, and ASGI
lifespan management (connecting/closing the shared `Client`).

Mounts, per the observability spec's deployment design:
    /api/...  -> data endpoints (schema browser, query console, ...)
    /         -> the built React SPA, only when `[ui].enabled` (Phase 1 ships
                 no build output yet, so this simply 404s until it does)
`/metrics` is Prometheus/OTel work, explicitly deferred past Phase 1.
"""

from __future__ import annotations

import dataclasses
import datetime
import decimal
import json
import mimetypes
import time
import uuid
from pathlib import Path
from typing import Any, Awaitable, Callable

from pylon.client import Client
from pylon.config import Config
from pylon.exceptions import PylonError
from pylon.schema._decorators import _get_own_annotations, _infer_module, _unwrap_optional
from pylon.schema._pointers import ComputedAnnotation, LinkAnnotation, MultiLinkAnnotation, PropertyAnnotation
from pylon.schema._meta import PointerMeta
from pylon.schema._registry import named_tuples_snapshot, snapshot as schema_snapshot
from pylon.schema._walker import _effective_pointers

Scope = dict[str, Any]
Receive = Callable[[], Awaitable[dict[str, Any]]]
Send = Callable[[dict[str, Any]], Awaitable[None]]

# Directory a production build of pylon-ui is copied into. Not shipped yet in
# Phase 1 — static serving simply 404s until it exists.
STATIC_DIR = Path(__file__).parent / "static"


def create_app(config: Config) -> Callable[[Scope, Receive, Send], Awaitable[None]]:
    """Build the raw ASGI application for `pylon serve`."""
    # Populated by the lifespan handler on startup; every http request reads
    # the same connected Client back out of this closure.
    connection: dict[str, Client] = {}

    async def app(scope: Scope, receive: Receive, send: Send) -> None:
        if scope["type"] == "lifespan":
            await _handle_lifespan(config, connection, receive, send)
            return

        path, method = scope["path"], scope["method"]
        if path == "/api/schema" and method == "GET":
            await _handle_get_schema(send)
        elif path == "/api/globals" and method == "GET":
            await _handle_get_globals(send)
        elif path == "/api/connections" and method == "GET":
            await _handle_get_connections(config, send)
        elif path == "/api/models" and method == "GET":
            await _handle_get_models(config, send)
        elif path == "/api/stats" and method == "GET":
            await _handle_get_stats(connection["client"], send)
        elif path == "/api/query" and method == "POST":
            await _handle_run_query(connection["client"], receive, send)
        elif path == "/api/ai/chat" and method == "POST":
            await _handle_ai_chat(config, connection["client"], receive, send)
        elif config.ui.enabled:
            await _serve_static(path, send)
        else:
            await _send_json(send, 404, {"error": "not found"})

    return app


async def _handle_lifespan(config: Config, connection: dict[str, Client], receive: Receive, send: Send) -> None:
    while True:
        message = await receive()
        if message["type"] == "lifespan.startup":
            try:
                # Installs the process-level SchemaDescriptor singleton (from
                # [project].schema-dir) that PyQL compilation needs — same call
                # every other Pylon entry point (repl, worker, ...) makes on startup.
                import pylon

                pylon.finalize()

                client = Client(config)
                await client.ensure_connected()
                connection["client"] = client
                await send({"type": "lifespan.startup.complete"})
            except Exception as exc:  # noqa: BLE001 — report to the ASGI server, don't hide it
                await send({"type": "lifespan.startup.failed", "message": str(exc)})
                return
        elif message["type"] == "lifespan.shutdown":
            try:
                client = connection.get("client")
                if client is not None:
                    await client.aclose()
                await send({"type": "lifespan.shutdown.complete"})
            except Exception as exc:  # noqa: BLE001
                await send({"type": "lifespan.shutdown.failed", "message": str(exc)})
            return


async def _read_json_body(receive: Receive) -> Any:
    body = b""
    more_body = True
    while more_body:
        message = await receive()
        body += message.get("body", b"")
        more_body = message.get("more_body", False)
    return json.loads(body) if body else {}


async def _send_json(send: Send, status: int, payload: Any) -> None:
    body = json.dumps(payload).encode()
    await send(
        {
            "type": "http.response.start",
            "status": status,
            "headers": [(b"content-type", b"application/json")],
        }
    )
    await send({"type": "http.response.body", "body": body})


# ---------------------------------------------------------------------------
# /api/schema
# ---------------------------------------------------------------------------
#
# Sourced entirely from the registered PyQL schema classes (pylon.finalize()'s
# in-memory registry) rather than pg_catalog — no DB round trip needed, and it
# resolves things pg_catalog structurally can't: a Link's Postgres column is
# named `company_id`, not `company`; a MultiLink has no column on its owning
# type at all (it's a separate junction table). Determining which pointers are
# links/multi-links (and their target type), vs. plain properties, requires
# reading the PyQL-level Property/Link/MultiLink/Computed annotations directly
# — confirmed empirically against the real demo schema, not guessed:
# `python3 -c "..."` against pylon-demo showed e.g. bare scalar properties with
# no Property[...] wrapper at all (`name: pylon.Str`), and Link/MultiLink
# target types that are sometimes the actual class, sometimes a forward-
# reference *string* (`Link["Company"]` written before Company is defined) —
# both are handled below.

# Canonical PyQL-style type names for every scalar marker class
# (pylon.Str, pylon.UUID, ..., plus stdlib uuid.UUID used for the injected
# `id` property) — shown below the pointer name in the Data Explorer's column
# headers, and used by the frontend to decide which types get a `<tag>`
# prefix on values (uuid/datetime/etc — plain str/int/bool/json don't need
# one, their JS type already says enough).
_TYPE_NAME_BY_CLASS = {
    "Str": "std::str",
    "Int16": "std::int16",
    "Int32": "std::int32",
    "Int64": "std::int64",
    "Float32": "std::float32",
    "Float64": "std::float64",
    "Decimal": "std::decimal",
    "Bool": "std::bool",
    "DateTime": "std::datetime",
    "LocalDateTime": "cal::local_datetime",
    "LocalDate": "cal::local_date",
    "LocalTime": "cal::local_time",
    "Duration": "std::duration",
    "UUID": "std::uuid",
    "JSON": "std::json",
    "Bytes": "std::bytes",
    "Sequence": "std::int64",  # sequences are backed by int64
}


def _type_qualname(cls: type) -> str:
    return f"{_infer_module(cls)}::{cls.__name__}"


def _merged_annotations(cls: type) -> dict[str, Any]:
    """Pointer annotations across the whole MRO (base classes first), mirroring
    how the schema DSL merges inherited pointers — e.g. Account's `email` shows
    up on Individual/Organization, but only via their base class's __dict__,
    not their own (confirmed: bare cls.__annotations__ misses it)."""
    merged: dict[str, Any] = {}
    for base in reversed(cls.__mro__):
        merged.update(_get_own_annotations(base))
    return merged


def _resolve_target(target: Any, owning_cls: type) -> str:
    """Link/MultiLink target_type is either the real class or a forward-ref
    string (e.g. Link["Company"]) — a string is assumed to name a type in the
    referencing pointer's own module, the same convention PyQL/PyQL use for
    unqualified same-module references."""
    if isinstance(target, str):
        return f"{_infer_module(owning_cls)}::{target}"
    return _type_qualname(target)


def _scalar_type_name(scalar_type: Any) -> str | None:
    name = scalar_type.__name__ if isinstance(scalar_type, type) else None
    return _TYPE_NAME_BY_CLASS.get(name) if name else None


def _pointer_editability(meta: PointerMeta) -> dict[str, Any]:
    """readonly/required/hasDefault/through — sourced from Pylon's own
    PointerMeta (cls.__pylon_config__.pointers), which the raw type annotation
    alone can't tell us. Only meaningful for property/link (readonly/required/
    hasDefault) and multilink (through, the junction type) — computed pointers
    are never editable regardless, so nothing is added for them."""
    if meta.kind in ("property", "link"):
        return {
            "readonly": meta.is_readonly,
            "required": not meta.nullable,
            "hasDefault": meta.default is not dataclasses.MISSING or meta.default_factory is not dataclasses.MISSING,
        }
    if meta.kind == "multilink" and meta.through is not None:
        # Resolved to a qualified "module::Name" string by pylon.finalize()'s
        # walker (_walker.py's _resolve_links) before this handler ever runs.
        return {"through": meta.through}
    return {}


def _classify_pointer(
    annotation: Any,
    owning_cls: type,
    enum_classes: set[type],
    meta: PointerMeta | None,
    named_tuple_classes: set[type],
) -> dict[str, Any]:
    """Returns the {kind, target?, typeName?, readonly?, required?, hasDefault?,
    through?} fragment for one property/link/multiLink/computed pointer."""
    _, inner = _unwrap_optional(annotation)

    if isinstance(inner, LinkAnnotation):
        result = {"kind": "link", "target": _resolve_target(inner.target_type, owning_cls)}
    elif isinstance(inner, MultiLinkAnnotation):
        result = {"kind": "multiLink", "target": _resolve_target(inner.target_type, owning_cls)}
    elif isinstance(inner, ComputedAnnotation):
        # return_type is usually a plain scalar class; Computed[MultiLink[...], ...]
        # (a computed backlink) is a real but rarer shape we don't resolve here.
        type_name = _scalar_type_name(inner.return_type)
        result = {"kind": "computed", "typeName": type_name} if type_name else {"kind": "computed"}
    else:
        # PropertyAnnotation wraps constrained/defaulted properties; a bare
        # scalar type hint (e.g. `name: pylon.Str`, or the injected
        # `id: uuid.UUID | None`) is an unconstrained property — both are
        # plain scalar properties.
        scalar_type = inner.scalar_type if isinstance(inner, PropertyAnnotation) else inner
        if isinstance(scalar_type, type) and scalar_type in enum_classes:
            result = {"kind": "enum", "target": _type_qualname(scalar_type)}
        elif isinstance(scalar_type, type) and scalar_type in named_tuple_classes:
            result = {"kind": "namedTuple", "target": _type_qualname(scalar_type)}
        else:
            type_name = _scalar_type_name(scalar_type)
            result = {"kind": "property", "typeName": type_name} if type_name else {"kind": "property"}

    if meta is not None:
        result.update(_pointer_editability(meta))
    return result


def _vector_index_pointer_names(vi: Any) -> list[str]:
    """Pointer names (not "Type.pointer" refs) a VectorIndex was declared with —
    i.e. exactly what got embedded for it."""
    return [vp.ref.split(".", 1)[1] for vp in vi._vector_pointers]


def _classify_named_tuple_member(
    annotation: Any, enum_classes: set[type], named_tuple_classes: set[type]
) -> dict[str, Any]:
    """Returns the {kind, target?, typeName?, required} fragment for one
    named-tuple member. A member is always a plain dataclass field — never a
    Link/MultiLink/Computed, since those annotations only exist on
    @pylon.type-decorated schema types, not value types."""
    nullable, inner = _unwrap_optional(annotation)
    if isinstance(inner, type) and inner in named_tuple_classes:
        result: dict[str, Any] = {"kind": "namedTuple", "target": _type_qualname(inner)}
    elif isinstance(inner, type) and inner in enum_classes:
        result = {"kind": "enum", "target": _type_qualname(inner)}
    else:
        type_name = _scalar_type_name(inner)
        result = {"kind": "scalar", "typeName": type_name} if type_name else {"kind": "scalar"}
    result["required"] = not nullable
    return result


def _build_named_tuple_entry(cls: type, enum_classes: set[type], named_tuple_classes: set[type]) -> dict[str, Any]:
    return {
        "module": _infer_module(cls),
        "name": cls.__name__,
        "members": [
            {"name": name, **_classify_named_tuple_member(annotation, enum_classes, named_tuple_classes)}
            for name, annotation in _merged_annotations(cls).items()
        ],
    }


def _build_type_entry(cls: type, enum_classes: set[type], named_tuple_classes: set[type]) -> dict[str, Any]:
    pointer_metas = _effective_pointers(cls)  # computed once per type, not per pointer
    return {
        "module": _infer_module(cls),
        "name": cls.__name__,
        # Real Pylon inheritance info (@pylon.abstract/@pylon.interface +
        # concrete subtypes) — lets the Data Explorer offer a subtype
        # picker when inserting into an abstract/interface type.
        "abstract": cls.__pylon_config__.abstract,
        "bases": [_type_qualname(base) for base in cls.__bases__ if hasattr(base, "__pylon_config__")],
        "pointers": [
            {
                "name": name,
                **_classify_pointer(annotation, cls, enum_classes, pointer_metas.get(name), named_tuple_classes),
            }
            for name, annotation in _merged_annotations(cls).items()
        ],
        # Powers the AI tab's Type select (only types with >=1 entry here
        # are offered) and Index select (offered only when there's more
        # than one). indexName is None for a bare/default VectorIndex.
        "vectorIndexes": [
            {"indexName": vi.index_name, "model": vi.model, "pointers": _vector_index_pointer_names(vi)}
            for vi in getattr(cls.__pylon_config__, "vector_indexes", [])
        ],
    }


async def _handle_get_schema(send: Send) -> None:
    registered_types, registered_enums, _ = schema_snapshot()
    enum_classes = set(registered_enums)
    registered_named_tuples = named_tuples_snapshot()
    named_tuple_classes = set(registered_named_tuples)

    types = [_build_type_entry(cls, enum_classes, named_tuple_classes) for cls in registered_types]
    enums = [
        {"module": _infer_module(cls), "name": cls.__name__, "members": [member.name for member in cls]}
        for cls in registered_enums
    ]
    named_tuples = [
        _build_named_tuple_entry(cls, enum_classes, named_tuple_classes) for cls in registered_named_tuples
    ]

    await _send_json(send, 200, {"types": types, "enums": enums, "namedTuples": named_tuples})


# ---------------------------------------------------------------------------
# /api/globals
# ---------------------------------------------------------------------------
#
# Powers the top bar's globals pill bar + configuration modal. Only *settable*
# session globals are listed — computed globals (Global[T, "select ..."]) are
# derived at query time, never user-set, so they're filtered out here rather
# than the frontend having to know to skip them.


async def _handle_get_globals(send: Send) -> None:
    from pylon.query import _get_schema

    schema = _get_schema()
    globals_ = [
        {
            "module": g["module"],
            "name": g["name"],
            "typeName": _TYPE_NAME_BY_CLASS.get(g["scalar_type"]),
            "required": g["required"],
        }
        for g in schema.globals()
        if not g["computed"]
    ]
    await _send_json(send, 200, {"globals": globals_})


# ---------------------------------------------------------------------------
# /api/connections
# ---------------------------------------------------------------------------
#
# Powers the top bar's connection-switcher dropdown and the frontend's :branch
# URL validation (an unrecognized segment renders 404 instead of a tab).
# config.connections is keyed "default" for the base [database] block plus
# one entry per [database.<name>] sub-table; "default" is surfaced to the
# frontend as "main", reusing the branch name the UI already hardcodes.


async def _handle_get_connections(config: Config, send: Send) -> None:
    others = sorted(name for name in config.connections if name != "default")
    connections = ["main", *others]
    await _send_json(
        send,
        200,
        {
            "project": config.project.name if config.project else None,
            "connections": connections,
        },
    )


# ---------------------------------------------------------------------------
# /api/models
# ---------------------------------------------------------------------------
#
# Powers the AI tab's Model select — only "chat"-purpose models (see
# ModelConfig.purpose in pylon/config.py). Embedding models are never listed
# here; they're selected implicitly via a type's VectorIndex, not by the user.


async def _handle_get_models(config: Config, send: Send) -> None:
    models = [
        {"name": name, "model": cfg.model, "apiStyle": cfg.api_style}
        for name, cfg in config.models_registry.items()
        if cfg.purpose == "chat"
    ]
    await _send_json(send, 200, {"models": models})


# ---------------------------------------------------------------------------
# /api/stats
# ---------------------------------------------------------------------------
#
# Powers the Dashboard tab's two headline numbers. "objects" is a live-tuple
# estimate straight from Postgres's own autovacuum-maintained statistics
# (pg_stat_user_tables.n_live_tup) rather than a real `count(*)` over every
# type — an exact count would mean one query per table (or a big UNION ALL),
# which doesn't scale and isn't what the upstream engine's own dashboard does either; this is
# an estimate, same tradeoff. "_pylon" is Pylon's own internal schema
# (migrations bookkeeping etc — never user data), excluded the same way
# pg_catalog/information_schema are. "types" is every registered schema type
# (concrete, abstract, interface, junction — schema_snapshot()'s first
# element already includes all four, confirmed via _decorators.py's shared
# _build_type() registering every one of them) plus registered custom scalars
# — no DB round trip needed, this is purely the in-memory registry.


async def _handle_get_stats(client: Client, send: Send) -> None:
    registered_types, _, registered_scalars = schema_snapshot()

    async with client.raw_connection() as conn:
        estimated_objects = await conn.fetchval(
            """
            SELECT SUM(n_live_tup)::bigint AS estimated_total_objects
            FROM pg_stat_user_tables
            WHERE schemaname NOT IN ('pg_catalog', 'information_schema', '_pylon')
            """
        )

    await _send_json(
        send,
        200,
        {
            "objects": estimated_objects or 0,
            "types": len(registered_types) + len(registered_scalars),
        },
    )


# ---------------------------------------------------------------------------
# /api/query
# ---------------------------------------------------------------------------


def _to_jsonable(value: Any) -> Any:
    """Recursively convert a hydrated PyQL result into JSON-safe values.

    Shape queries (e.g. `SELECT Person { name }`) hydrate into instances of
    the *full* Person dataclass but only set the requested fields — unrequested
    fields exist in dataclasses.fields() but aren't set on the instance,
    so this reads __dict__ directly rather than the class's full field list.
    """
    if dataclasses.is_dataclass(value) and not isinstance(value, type):
        attrs = getattr(value, "__dict__", None)
        if attrs is None:  # slotted dataclass — fall back to only-set fields
            attrs = {f.name: getattr(value, f.name) for f in dataclasses.fields(value) if hasattr(value, f.name)}
        # __pylon_type__ passes through as-is here (unlike the CLI's plain-text
        # formatter, which strips it after using it as a label) — the frontend's
        # JsonTree does that same label-vs-fields split itself, since it also
        # needs __pylon_type__ to look up real field types from /api/schema.
        return {k: _to_jsonable(v) for k, v in attrs.items()}
    if isinstance(value, (list, tuple)):
        return [_to_jsonable(v) for v in value]
    if isinstance(value, dict):
        return {k: _to_jsonable(v) for k, v in value.items()}
    if isinstance(value, uuid.UUID):
        return str(value)
    if isinstance(value, (datetime.datetime, datetime.date, datetime.time)):
        return value.isoformat()
    if isinstance(value, decimal.Decimal):
        return float(value)
    return value


async def _handle_run_query(client: Client, receive: Receive, send: Send) -> None:
    body = await _read_json_body(receive)
    pyql = body.get("pyql", "")
    params = body.get("params") or {}
    # Session globals configured via the top bar's globals modal — keyed by
    # "module::name", threaded through client.with_globals() for this query
    # only (the client itself stays global/stateless across requests).
    globals_ = body.get("globals") or {}

    start = time.perf_counter()
    try:
        target = client.with_globals(globals_) if globals_ else client
        objects = await target.query(pyql, **params)
    except PylonError as exc:
        await _send_json(send, 400, {"error": str(exc)})
        return
    duration_ms = (time.perf_counter() - start) * 1000

    await _send_json(send, 200, {"objects": [_to_jsonable(o) for o in objects], "duration_ms": duration_ms})


# ---------------------------------------------------------------------------
# /api/ai/chat
# ---------------------------------------------------------------------------
#
# The AI tab's RAG loop: runs vector::search for context, templates the (for
# now, hardcoded-default — see pylon-ui's AI tab plan) system/user prompt
# around it, and calls the selected chat-purpose model. No prompt-template
# registry exists in Pylon yet, so this is the one place that default prompt
# text lives; a real registry (like the upstream engine's `builtin::rag-default`) is future
# work, not something to fake here.

_DEFAULT_PROMPT_SYSTEM = """You are an expert Q&A system.
Always answer questions based on the provided context information. Never use prior knowledge.
Follow these additional rules:
1. Never directly reference the given context in your answer.
2. Never include phrases like 'Based on the context, ...' or any similar phrases in your responses.
3. When the context does not provide information about the question, answer with 'No information available.'.
Context information is below:
{context}
Given the context information above and not prior knowledge, answer the user query."""

_DEFAULT_PROMPT_USER = "Query: {query}\nAnswer:"


def _make_chat_provider(model_cfg):
    from pylon.vector.models import AnthropicProvider, OpenAIProvider

    if model_cfg.api_style == "anthropic":
        return AnthropicProvider(api_url=model_cfg.api_url, model=model_cfg.model, api_key=model_cfg.secret)
    return OpenAIProvider(api_url=model_cfg.api_url, model=model_cfg.model, api_key=model_cfg.secret)


def _resolve_vector_index_pointers(pylon_type: str, index_name: str | None) -> list[str]:
    """Looks up the VectorIndex matching *index_name* on *pylon_type* and
    returns its pointer names — so /api/ai/chat's context is built from what
    the similarity search actually matched on, not an arbitrary object dump
    of whatever pointers happen to be requested for display."""
    module, _, name = pylon_type.partition("::")
    registered_types, _, _ = schema_snapshot()
    for cls in registered_types:
        if _infer_module(cls) != module or cls.__name__ != name:
            continue
        for vi in getattr(cls.__pylon_config__, "vector_indexes", []):
            if vi.index_name == index_name:
                return _vector_index_pointer_names(vi)
    return []


async def _handle_ai_chat(config: Config, client: Client, receive: Receive, send: Send) -> None:
    body = await _read_json_body(receive)
    model_name = body.get("modelName", "")
    pylon_type = body.get("pylonType", "")
    index_name = body.get("indexName")
    # Optional PyQL expression narrowing which objects vector::search's first
    # argument scopes over, e.g. "select Type filter .property = value" —
    # embedded as-is inside vector::search(({context_query}), ...). Empty/
    # unset falls back to searching every object of pylon_type (status quo).
    context_query = body.get("contextQuery") or None
    message = body.get("message", "")
    history = body.get("history") or []

    model_cfg = config.models_registry.get(model_name)
    if model_cfg is None or model_cfg.purpose != "chat":
        await _send_json(send, 400, {"error": f"'{model_name}' is not a configured chat model"})
        return

    index_pointers = _resolve_vector_index_pointers(pylon_type, index_name)
    if not index_pointers:
        await _send_json(
            send, 400, {"error": f"no VectorIndex found on '{pylon_type}' matching index_name={index_name!r}"}
        )
        return

    shape = ", ".join(index_pointers)
    index_clause = ", index_name := <str>$indexName" if index_name else ""
    search_target = f"({context_query})" if context_query else pylon_type
    # The chat message itself is both the vector::search query text *and*
    # the LLM's question (see _DEFAULT_PROMPT_USER below) — no separate
    # search-text input anymore.
    pyql = (
        f"select vector::search({search_target}, query := <str>$queryText{index_clause}) "
        f"{{ object {{ {shape} }}, distance }} order by .distance limit 5"
    )
    params: dict[str, object] = {"queryText": message}
    if index_name:
        params["indexName"] = index_name

    try:
        objects = await client.query(pyql, **params)
    except PylonError as exc:
        await _send_json(send, 400, {"error": str(exc)})
        return

    results = [_to_jsonable(o) for o in objects]
    # One line per result, just the indexed pointers concatenated — the same
    # text that was embedded, not a "key: value" dump of the whole object.
    context = "\n".join(
        "- " + ". ".join(str(result["object"].get(p, "")) for p in index_pointers) for result in results
    )

    messages = [
        {"role": "system", "content": _DEFAULT_PROMPT_SYSTEM.format(context=context)},
        *history,
        {"role": "user", "content": _DEFAULT_PROMPT_USER.format(query=message)},
    ]

    provider = _make_chat_provider(model_cfg)
    try:
        reply = await provider.chat(messages)
    except Exception as exc:  # noqa: BLE001 — surface provider/network errors to the UI
        await _send_json(send, 502, {"error": f"chat model request failed: {exc}"})
        return

    await _send_json(send, 200, {"reply": reply, "results": results})


# ---------------------------------------------------------------------------
# Static SPA (Phase 1: no build output shipped yet, so this just 404s)
# ---------------------------------------------------------------------------


async def _serve_static(path: str, send: Send) -> None:
    root = STATIC_DIR.resolve()
    if not root.is_dir():
        await _send_json(send, 404, {"error": "not found"})
        return

    candidate = (root / path.lstrip("/")).resolve()
    if not candidate.is_relative_to(root) or not candidate.is_file():
        candidate = root / "index.html"  # SPA client-side routing fallback

    if not candidate.is_file():
        await _send_json(send, 404, {"error": "not found"})
        return

    content_type = mimetypes.guess_type(str(candidate))[0] or "application/octet-stream"
    await send(
        {
            "type": "http.response.start",
            "status": 200,
            "headers": [(b"content-type", content_type.encode())],
        }
    )
    await send({"type": "http.response.body", "body": candidate.read_bytes()})
