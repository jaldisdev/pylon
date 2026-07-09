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
from pylon.schema._fields import ComputedAnnotation, LinkAnnotation, MultiLinkAnnotation, PropertyAnnotation
from pylon.schema._registry import snapshot as schema_snapshot

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
        elif path == "/api/query" and method == "POST":
            await _handle_run_query(connection["client"], receive, send)
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
# type at all (it's a separate junction table). Determining which fields are
# links/multi-links (and their target type), vs. plain properties, requires
# reading the PyQL-level Property/Link/MultiLink/Computed annotations directly
# — confirmed empirically against the real demo schema, not guessed:
# `python3 -c "..."` against pylon-demo showed e.g. bare scalar fields with no
# Property[...] wrapper at all (`name: pylon.Str`), and Link/MultiLink target
# types that are sometimes the actual class, sometimes a forward-reference
# *string* (`Link["Company"]` written before Company is defined) — both are
# handled below.

# Scalar marker classes (pylon.UUID, pylon.DateTime, ..., plus stdlib
# uuid.UUID used for the injected `id` field) that get a `<tag>` prefix in the
# frontend's JsonTree, matching the upstream inspector's inspector. Plain str/int/float/bool/
# json scalars need no tag — their JS type already says enough.
_SCALAR_TAG_BY_NAME = {
    "UUID": "uuid",
    "DateTime": "datetime",
    "LocalDateTime": "local_datetime",
    "LocalDate": "date",
    "LocalTime": "time",
    "Duration": "duration",
}


def _type_qualname(cls: type) -> str:
    return f"{_infer_module(cls)}::{cls.__name__}"


def _merged_annotations(cls: type) -> dict[str, Any]:
    """Field annotations across the whole MRO (base classes first), mirroring
    how the schema DSL merges inherited fields — e.g. Account's `email` shows
    up on Individual/Organization, but only via their base class's __dict__,
    not their own (confirmed: bare cls.__annotations__ misses it)."""
    merged: dict[str, Any] = {}
    for base in reversed(cls.__mro__):
        merged.update(_get_own_annotations(base))
    return merged


def _resolve_target(target: Any, owning_cls: type) -> str:
    """Link/MultiLink target_type is either the real class or a forward-ref
    string (e.g. Link["Company"]) — a string is assumed to name a type in the
    referencing field's own module, the same convention PyQL/PyQL use for
    unqualified same-module references."""
    if isinstance(target, str):
        return f"{_infer_module(owning_cls)}::{target}"
    return _type_qualname(target)


def _classify_field(annotation: Any, owning_cls: type, enum_classes: set[type]) -> dict[str, Any]:
    """Returns the {kind, target?, scalarTag?} fragment for one field."""
    _, inner = _unwrap_optional(annotation)

    if isinstance(inner, LinkAnnotation):
        return {"kind": "link", "target": _resolve_target(inner.target_type, owning_cls)}
    if isinstance(inner, MultiLinkAnnotation):
        return {"kind": "multiLink", "target": _resolve_target(inner.target_type, owning_cls)}
    if isinstance(inner, ComputedAnnotation):
        return {"kind": "computed"}

    # PropertyAnnotation wraps constrained/defaulted properties; a bare scalar
    # type hint (e.g. `name: pylon.Str`, or the injected `id: uuid.UUID | None`)
    # is an unconstrained property — both are plain scalar fields.
    scalar_type = inner.scalar_type if isinstance(inner, PropertyAnnotation) else inner

    if isinstance(scalar_type, type) and scalar_type in enum_classes:
        return {"kind": "enum", "target": _type_qualname(scalar_type)}

    scalar_name = scalar_type.__name__ if isinstance(scalar_type, type) else None
    scalar_tag = _SCALAR_TAG_BY_NAME.get(scalar_name) if scalar_name else None
    return {"kind": "property", "scalarTag": scalar_tag} if scalar_tag else {"kind": "property"}


async def _handle_get_schema(send: Send) -> None:
    registered_types, registered_enums, _ = schema_snapshot()
    enum_classes = set(registered_enums)

    types = [
        {
            "module": _infer_module(cls),
            "name": cls.__name__,
            "fields": [
                {"name": name, **_classify_field(annotation, cls, enum_classes)}
                for name, annotation in _merged_annotations(cls).items()
            ],
        }
        for cls in registered_types
    ]
    enums = [
        {"module": _infer_module(cls), "name": cls.__name__, "members": [member.name for member in cls]}
        for cls in registered_enums
    ]

    await _send_json(send, 200, {"types": types, "enums": enums})


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

    start = time.perf_counter()
    try:
        rows = await client.query(pyql, **params)
    except PylonError as exc:
        await _send_json(send, 400, {"error": str(exc)})
        return
    duration_ms = (time.perf_counter() - start) * 1000

    await _send_json(send, 200, {"rows": [_to_jsonable(r) for r in rows], "duration_ms": duration_ms})


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
