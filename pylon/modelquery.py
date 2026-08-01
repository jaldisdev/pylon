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

"""Model-based query/mutation API — build and run PyQL queries directly
against `@pylon.type` classes instead of hand-written query text::

    users = await client.query(Person)
    bob_like = await client.query(Person.filter(lambda u: std.ilike(u.name, '%bob%')))
    await client.execute(Person.filter(id=bob.id).delete())

Everything here renders to plain PyQL text + a params dict, then hands off
to the *existing* compile/cache/execute pipeline unchanged (see
``pylon.client._compile_and_bind``/``_compile_and_resolve``) — there is no
separate execution path, no new caching, no Rust changes.
"""

from __future__ import annotations

from typing import Any

from pylon.exceptions import InterfaceError

# ── Expression tree ──────────────────────────────────────────────────────────
#
# Built by operator-overloading a proxy object (`_FieldPath`) instead of
# disassembling lambda bytecode. `and`/`or`/`not`/`if` can't be overloaded
# in Python, so boolean combination uses `&`/`|`/`~`; `__bool__` raises a
# clear error on misuse rather than silently building a wrong
# (always-truthy) expression.


class _Node:
    """Base for every expression-tree node. Comparison operators live here
    (not just on `_FieldPath`) so comparing *any* sub-expression works —
    e.g. `std.foo(u.name) == 'x'` compares a `_FuncCall` result, not a bare
    field path. Only `_FieldPath` additionally supports attribute access."""

    __slots__ = ()

    def _cmp(self, op: str, other: Any) -> "_Compare":
        return _Compare(self, op, _as_node(other))

    def __eq__(self, other: Any) -> "_Compare":  # type: ignore[override]
        return self._cmp("=", other)

    def __ne__(self, other: Any) -> "_Compare":  # type: ignore[override]
        return self._cmp("!=", other)

    def __lt__(self, other: Any) -> "_Compare":
        return self._cmp("<", other)

    def __le__(self, other: Any) -> "_Compare":
        return self._cmp("<=", other)

    def __gt__(self, other: Any) -> "_Compare":
        return self._cmp(">", other)

    def __ge__(self, other: Any) -> "_Compare":
        return self._cmp(">=", other)

    def __hash__(self) -> int:
        # __eq__ is overloaded to build an expression rather than compare,
        # so the default hash (disabled by defining __eq__) needs restoring
        # explicitly — identity hashing is fine, these are throwaway proxies.
        return object.__hash__(self)

    def __and__(self, other: Any) -> "_BoolOp":
        if not isinstance(other, _Node):
            return NotImplemented
        return _BoolOp("and", self, other)

    def __or__(self, other: Any) -> "_BoolOp":
        if not isinstance(other, _Node):
            return NotImplemented
        return _BoolOp("or", self, other)

    def __invert__(self) -> "_Not":
        return _Not(self)

    def __bool__(self) -> bool:
        raise TypeError(
            "cannot use a Pylon filter expression in a boolean context "
            "(if/and/or/not) — use & / | / ~ instead"
        )


class _Literal(_Node):
    __slots__ = ("value",)

    def __init__(self, value: Any) -> None:
        self.value = value


class _FieldPath(_Node):
    """A `.a.b.c`-style relative path — also the proxy object passed into a
    `.filter(lambda u: ...)` callable (the root path, with no segments)."""

    __slots__ = ("segments",)

    def __init__(self, segments: list[str]) -> None:
        self.segments = segments

    def __getattr__(self, name: str) -> "_FieldPath":
        if name.startswith("_"):
            raise AttributeError(name)
        return _FieldPath([*self.segments, name])


class _Compare(_Node):
    __slots__ = ("left", "op", "right")

    def __init__(self, left: _Node, op: str, right: _Node) -> None:
        self.left = left
        self.op = op
        self.right = right


class _BoolOp(_Node):
    __slots__ = ("op", "left", "right")

    def __init__(self, op: str, left: _Node, right: _Node) -> None:
        self.op = op
        self.left = left
        self.right = right


class _Not(_Node):
    __slots__ = ("operand",)

    def __init__(self, operand: _Node) -> None:
        self.operand = operand


class _FuncCall(_Node):
    __slots__ = ("module", "name", "args")

    def __init__(self, module: str | None, name: str, args: list[_Node]) -> None:
        self.module = module
        self.name = name
        self.args = args


def _as_node(value: Any) -> _Node:
    return value if isinstance(value, _Node) else _Literal(value)


# ── stdlib function-call namespace ───────────────────────────────────────────
#
# `std.ilike(u.name, '...')`, `math.sqrt(u.x)`, `cal.foo(...)` — attribute
# access builds a generic function-call node for ANY PyQL stdlib function,
# no hand-maintained per-function allowlist needed. A small alias table
# covers the handful of PyQL operators that are keyword/infix-only in the
# grammar (e.g. `ilike`/`like`/`in` — there's no bare `std::ilike(...)`
# callable, only the `x ilike y` infix form) so they still work when called
# `std.ilike(...)`-style — this is purely a surface-syntax choice on the
# renderer's part, not a claim that PyQL exposes them as real functions.
_INFIX_ALIASES = {
    "ilike": "ilike",
    "like": "like",
    "not_ilike": "not ilike",
    "not_like": "not like",
    "in_": "in",
    "not_in": "not in",
}


class _FuncNamespace:
    __slots__ = ("_module",)

    def __init__(self, module: str) -> None:
        self._module = module

    def __getattr__(self, name: str):
        if name.startswith("_"):
            raise AttributeError(name)
        infix = _INFIX_ALIASES.get(name)

        def _call(*args: Any) -> _Node:
            nodes = [_as_node(a) for a in args]
            if infix is not None:
                if len(nodes) != 2:
                    raise TypeError(
                        f"{self._module}.{name}(...) renders as the infix '{infix}' "
                        f"operator and takes exactly 2 arguments, got {len(nodes)}"
                    )
                return _Compare(nodes[0], infix, nodes[1])
            return _FuncCall(self._module, name, nodes)

        return _call


std = _FuncNamespace("std")
math = _FuncNamespace("math")
cal = _FuncNamespace("cal")


# ── Rendering: expression tree → PyQL text + params ──────────────────────────
#
# Every literal leaf becomes a `$__mq_pN` parameter — never inlined as raw
# text — so values flow through the exact same type-coercion/binding path
# as hand-written PyQL parameters (see `_compile_and_bind` in client.py).


def render_expr(node: _Node) -> tuple[str, dict[str, Any]]:
    params: dict[str, Any] = {}
    counter = [0]
    text = _render(node, params, counter)
    return text, params


def _render(node: _Node, params: dict[str, Any], counter: list[int]) -> str:
    if isinstance(node, _FieldPath):
        if not node.segments:
            raise InterfaceError("filter expression references the whole object, not a field")
        return "." + ".".join(node.segments)
    if isinstance(node, _Literal):
        name = f"__mq_p{counter[0]}"
        counter[0] += 1
        params[name] = node.value
        return f"${name}"
    if isinstance(node, _Compare):
        return f"{_render(node.left, params, counter)} {node.op} {_render(node.right, params, counter)}"
    if isinstance(node, _BoolOp):
        # Unconditionally parenthesize both sides — this is generated text,
        # never shown to a user, so there's no cost to over-parenthesizing
        # and it sidesteps any precedence bugs entirely.
        return f"({_render(node.left, params, counter)}) {node.op} ({_render(node.right, params, counter)})"
    if isinstance(node, _Not):
        return f"not ({_render(node.operand, params, counter)})"
    if isinstance(node, _FuncCall):
        args_text = ", ".join(_render(a, params, counter) for a in node.args)
        prefix = f"{node.module}::" if node.module else ""
        return f"{prefix}{node.name}({args_text})"
    raise TypeError(f"unsupported filter expression node: {node!r}")


# ── ModelSet: a chainable, unexecuted query against one type ────────────────


class ModelSet:
    """A model class plus an optional filter expression — returned by
    `Model.filter(...)`, consumed by `Client.query`/`Client.execute`."""

    __slots__ = ("model", "_expr", "_delete")

    def __init__(self, model: type, expr: _Node | None = None, delete: bool = False) -> None:
        self.model = model
        self._expr = expr
        self._delete = delete

    def filter(self, *args: Any, **kwargs: Any) -> "ModelSet":
        if self._delete:
            raise InterfaceError("cannot call .filter() on an already-built delete query")
        expr = self._expr
        for fn in args:
            if not callable(fn):
                raise TypeError(f".filter() positional arguments must be callables, got {type(fn).__name__}")
            result = fn(_FieldPath([]))
            if not isinstance(result, _Node):
                raise TypeError(
                    ".filter() lambda must return a filter expression built from comparisons/"
                    f"&/|/~/std.*(...), got {type(result).__name__}"
                )
            expr = result if expr is None else _BoolOp("and", expr, result)
        for field_name, value in kwargs.items():
            cond = _Compare(_FieldPath([field_name]), "=", _as_node(value))
            expr = cond if expr is None else _BoolOp("and", expr, cond)
        return ModelSet(self.model, expr, self._delete)

    def delete(self) -> "ModelSet":
        if self._expr is None:
            raise InterfaceError(
                "refusing to build an unfiltered delete — call .filter(...) first "
                "(this guards against accidentally deleting an entire table)"
            )
        return ModelSet(self.model, self._expr, delete=True)


def filter_classmethod(cls: type, *args: Any, **kwargs: Any) -> ModelSet:
    """The function installed as `Model.filter` (see `_inject_query_methods`
    in `pylon.schema._decorators`)."""
    return ModelSet(cls).filter(*args, **kwargs)


# ── PyQL text generation for SELECT / DELETE ─────────────────────────────────


def _qualified_type_name(model: type) -> str:
    cfg = model.__pylon_config__
    return f"{cfg.module}::{cfg.name}"


def _default_shape(model: type) -> str:
    cfg = model.__pylon_config__
    fields = [name for name, meta in cfg.pointers.items() if meta.kind == "property"]
    return ", ".join(fields)


def render_select(ms: ModelSet) -> tuple[str, dict[str, Any]]:
    type_name = _qualified_type_name(ms.model)
    shape = _default_shape(ms.model)
    if ms._expr is not None:
        filter_text, params = render_expr(ms._expr)
        return f"select {type_name} {{ {shape} }} filter {filter_text}", params
    return f"select {type_name} {{ {shape} }}", {}


def render_delete(ms: ModelSet) -> tuple[str, dict[str, Any]]:
    type_name = _qualified_type_name(ms.model)
    assert ms._expr is not None, "ModelSet.delete() already guards against a missing filter"
    filter_text, params = render_expr(ms._expr)
    return f"delete {type_name} filter {filter_text}", params


def render(obj: Any) -> tuple[str, dict[str, Any]] | None:
    """Converts a bare `@pylon.type` class or a `ModelSet` into PyQL text +
    params, or returns `None` if `obj` isn't something this module knows how
    to render (the caller should fall back to its own "not a str" error)."""
    if isinstance(obj, type) and hasattr(obj, "__pylon_config__"):
        return render_select(ModelSet(obj))
    if isinstance(obj, ModelSet):
        return render_delete(obj) if obj._delete else render_select(obj)
    return None


# ── save(): INSERT/UPDATE PyQL generation for a single instance ─────────────
#
# New-vs-existing is decided by the presence of `__pylon_saved__` in
# `obj.__dict__` — stashed by `pylon.query._decode()` on every hydrated
# instance, absent on a freshly `__init__`'d one (even if the caller
# pre-assigned `id` under `allow_user_specified_id`).

_UNSET = object()


def prepare_save(obj: Any) -> tuple[str, dict[str, Any]] | None:
    """Renders one INSERT (new instance) or UPDATE (hydrated instance,
    diffed against its `__pylon_saved__` shadow) for `obj`, or returns
    `None` if a hydrated instance has no changed fields (a no-op save)."""
    cfg = type(obj).__pylon_config__
    type_name = f"{cfg.module}::{cfg.name}"
    saved = obj.__dict__.get("__pylon_saved__")
    params: dict[str, Any] = {}
    counter = [0]

    def _param(value: Any) -> str:
        name = f"__mq_s{counter[0]}"
        counter[0] += 1
        params[name] = value
        return f"${name}"

    if saved is None:
        assignments = []
        for name, meta in cfg.pointers.items():
            if meta.kind != "property" or name not in obj.__dict__:
                continue
            value = obj.__dict__[name]
            # A field left at its Python-side default (None) is
            # indistinguishable from "never touched" — omit it so any
            # server-side default (id generation, Default(...), sequences)
            # still applies. An explicitly-set None on a genuinely nullable
            # field is unrepresentable here, but has the same net effect:
            # the column is simply omitted and stays NULL either way.
            if value is None:
                continue
            assignments.append(f"{name} := {_param(value)}")
        if not assignments:
            raise InterfaceError(f"cannot save a new {type_name} instance with no fields set")
        return f"insert {type_name} {{ {', '.join(assignments)} }}", params

    assignments = []
    for name, meta in cfg.pointers.items():
        if meta.kind != "property" or meta.is_readonly or name not in obj.__dict__:
            continue
        current = obj.__dict__[name]
        if current == saved.get(name, _UNSET):
            continue
        assignments.append(f"{name} := {_param(current)}")
    if not assignments:
        return None
    pid = obj.__dict__.get("id")
    if pid is None:
        raise InterfaceError(f"cannot update {type_name}: instance has no id")
    id_param = _param(pid)
    return f"update {type_name} filter .id = {id_param} set {{ {', '.join(assignments)} }}", params
