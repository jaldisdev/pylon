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

    def _cmp(self, op: str, other: Any) -> _Compare:
        return _Compare(self, op, _as_node(other))

    def __eq__(self, other: Any) -> _Compare:  # type: ignore[override]
        return self._cmp('=', other)

    def __ne__(self, other: Any) -> _Compare:  # type: ignore[override]
        return self._cmp('!=', other)

    def __lt__(self, other: Any) -> _Compare:
        return self._cmp('<', other)

    def __le__(self, other: Any) -> _Compare:
        return self._cmp('<=', other)

    def __gt__(self, other: Any) -> _Compare:
        return self._cmp('>', other)

    def __ge__(self, other: Any) -> _Compare:
        return self._cmp('>=', other)

    def __hash__(self) -> int:
        # __eq__ is overloaded to build an expression rather than compare,
        # so the default hash (disabled by defining __eq__) needs restoring
        # explicitly — identity hashing is fine, these are throwaway proxies.
        return object.__hash__(self)

    def __and__(self, other: Any) -> _BoolOp:
        if not isinstance(other, _Node):
            return NotImplemented
        return _BoolOp('and', self, other)

    def __or__(self, other: Any) -> _BoolOp:
        if not isinstance(other, _Node):
            return NotImplemented
        return _BoolOp('or', self, other)

    def __invert__(self) -> _Not:
        return _Not(self)

    def __bool__(self) -> bool:
        raise TypeError(
            'cannot use a Pylon filter expression in a boolean context (if/and/or/not) — use & / | / ~ instead'
        )


class _Literal(_Node):
    __slots__ = ('value',)

    def __init__(self, value: Any) -> None:
        self.value = value


class _FieldPath(_Node):
    """A `.a.b.c`-style relative path — also the proxy object passed into a
    `.filter(lambda u: ...)` callable (the root path, with no segments)."""

    __slots__ = ('segments',)

    def __init__(self, segments: list[str]) -> None:
        self.segments = segments

    def __getattr__(self, name: str) -> _FieldPath:
        if name.startswith('_'):
            raise AttributeError(name)
        return _FieldPath([*self.segments, name])


class _Compare(_Node):
    __slots__ = ('left', 'op', 'right')

    def __init__(self, left: _Node, op: str, right: _Node) -> None:
        self.left = left
        self.op = op
        self.right = right


class _BoolOp(_Node):
    __slots__ = ('left', 'op', 'right')

    def __init__(self, op: str, left: _Node, right: _Node) -> None:
        self.op = op
        self.left = left
        self.right = right


class _Not(_Node):
    __slots__ = ('operand',)

    def __init__(self, operand: _Node) -> None:
        self.operand = operand


class _FuncCall(_Node):
    __slots__ = ('args', 'module', 'name')

    def __init__(self, module: str | None, name: str, args: list[_Node]) -> None:
        self.module = module
        self.name = name
        self.args = args


class _ConstantCall(_FuncCall):
    """A zero-argument constant such as `math.pi`, exposed as a value.

    Callable as well, returning itself, so both `math.pi` and `math.pi()`
    work. Without that, choosing one spelling silently breaks the other —
    and which reads more naturally genuinely varies by name (`math.pi` vs.
    `sys.get_version`), so accepting both beats guessing.
    """

    __slots__ = ()

    def __call__(self) -> _ConstantCall:
        return self


class _SubQuery(_Node):
    """A `ModelSet` used as a value inside another expression.

    Always rendered as a `with` binding referenced by name, never inlined:
    PyQL rejects a bare sub-statement in expression position ("sub-statement
    used as expression is only valid as the subject of a SELECT result"),
    but a with-bound one referenced by name is fine. Hoisting is therefore
    mandatory here, unlike anywhere else.
    """

    __slots__ = ('model_set',)

    def __init__(self, model_set: Any) -> None:
        self.model_set = model_set


def _as_node(value: Any) -> _Node:
    """Wrap a Python value as an expression node.

    Anything that isn't already a node has to become a bound parameter, so
    this is the one place that decides what may legally be a *value*.
    Objects that clearly mean "a query construct" rather than a value are
    rejected or converted, because silently binding them as parameters
    produced text that compiled and then did the wrong thing — a model
    instance became a parameter holding the instance, and a `ModelSet`
    became one holding the set.
    """
    if isinstance(value, _Node):
        return value

    if isinstance(value, ModelSet):
        return _SubQuery(value)

    # A model *instance* stands for the row it identifies, so compare by id
    # (`filter .company = <uuid>$p`, which the compiler accepts for a link).
    if hasattr(type(value), '__pylon_config__'):
        target_id = value.__dict__.get('id')
        if target_id is None:
            cfg = type(value).__pylon_config__
            raise InterfaceError(
                f'cannot use an unsaved {cfg.name} instance in a query expression — it has no id '
                'yet, so there is nothing to match against. Save it first.'
            )
        return _Literal(target_id)

    # A model *class* is a whole set, not a value.
    if isinstance(value, type) and hasattr(value, '__pylon_config__'):
        raise InterfaceError(
            f'cannot use the type {value.__name__} itself as a value — did you mean {value.__name__}.filter(...)?'
        )

    if isinstance(value, _FuncNamespace):
        raise InterfaceError(
            f'cannot use the {value._module} namespace itself as a value — call a function on it, '
            f'e.g. {value._module}.str_lower(...)'
        )

    return _Literal(value)


# ── stdlib function-call namespace ───────────────────────────────────────────
#
# `std.ilike(u.name, '...')`, `math.sqrt(u.x)`, `cal.foo(...)` — attribute
# access builds a function-call node for any PyQL stdlib function, validated
# against the real registry (see `pylon.stdlib`) so an unknown name or a bad
# argument count fails where it was written rather than at compile time.
#
# The alias table below covers the handful of PyQL operators that are
# keyword/infix-only in the *grammar* — there is no `std::ilike(...)`
# callable, only the `x ilike y` form — so they have no registry entry to
# validate against and genuinely can't be derived. Offering them as
# `std.ilike(...)` is a surface-syntax choice on the renderer's part, not a
# claim that PyQL exposes them as functions.
_INFIX_ALIASES = {
    'ilike': 'ilike',
    'like': 'like',
    'not_ilike': 'not ilike',
    'not_like': 'not like',
    'in_': 'in',
    'not_in': 'not in',
}

_INFIX_ALIAS_NAMES = frozenset(_INFIX_ALIASES)


class _FuncNamespace:
    """One PyQL stdlib namespace (`std`, `math`, `cal`, `sys`) exposed as a
    Python object. Attribute access is resolved lazily against the registry,
    so importing `pylon` doesn't pay to materialize it."""

    __slots__ = ('_module',)

    def __init__(self, module: str) -> None:
        self._module = module

    def __repr__(self) -> str:
        return f'<pylon {self._module}:: namespace>'

    def __dir__(self):
        from pylon import stdlib

        return sorted({stdlib.python_name(n) for n in stdlib.names(self._module)} | _INFIX_ALIAS_NAMES)

    def __getattr__(self, attr: str):
        if attr.startswith('__'):
            raise AttributeError(attr)

        from pylon import stdlib

        infix = _INFIX_ALIASES.get(attr)
        if infix is not None:

            def _infix_call(*args: Any) -> _Node:
                nodes = [_as_node(a) for a in args]
                if len(nodes) != 2:
                    raise TypeError(
                        f"{self._module}.{attr}(...) renders as the infix '{infix}' "
                        f'operator and takes exactly 2 arguments, got {len(nodes)}'
                    )
                return _Compare(nodes[0], infix, nodes[1])

            return _infix_call

        # `std.assert_` -> `std::assert`; see `stdlib.python_name`.
        name = stdlib.pyql_name(attr)

        # Surface an unknown name at the attribute itself, so a typo raises
        # where it was written rather than only once the result is called.
        # AttributeError (not InterfaceError) keeps `hasattr`/`getattr` with a
        # default behaving the way callers expect from any Python object.
        if name not in stdlib.names(self._module):
            raise AttributeError(stdlib.unknown_function_message(self._module, attr, _INFIX_ALIAS_NAMES))

        # Zero-arg immutable entries (`math.pi`, `math.e`) read as values
        # rather than calls. `_ConstantCall` accepts both spellings, so
        # picking one doesn't break the other.
        if stdlib.is_constant(self._module, name):
            return _ConstantCall(self._module, name, [])

        def _call(*args: Any) -> _Node:
            nodes = [_as_node(a) for a in args]
            stdlib.check_call(self._module, name, len(nodes))
            return _FuncCall(self._module, name, nodes)

        return _call


std = _FuncNamespace('std')
math = _FuncNamespace('math')
cal = _FuncNamespace('cal')
sys = _FuncNamespace('sys')


# ── Rendering: expression tree → PyQL text + params ──────────────────────────
#
# In a query, every literal leaf becomes a `$__mq_pN` parameter — never
# inlined as raw text — so values flow through the exact same
# type-coercion/binding path as hand-written PyQL parameters (see
# `_compile_and_bind` in client.py).
#
# A *schema default* is the exception: it's compiled once into DDL by
# `compile_scalar_default` on the Rust side, where there is no parameter
# binding to attach values to. There, literals have to be inlined as PyQL
# literal text instead. That's the only difference between the two modes, so
# they share one walker rather than duplicating the tree traversal.


class _RenderCtx:
    """State threaded through one render pass."""

    __slots__ = ('bindings', 'context', 'counter', 'inline_literals', 'params', 'prefix')

    def __init__(self, *, inline_literals: bool, context: str, prefix: str = '__mq_p') -> None:
        self.params: dict[str, Any] = {}
        self.counter = 0
        self.inline_literals = inline_literals
        #: One of `pylon.stdlib.CONTEXT_*`, used to gate stdlib calls.
        self.context = context
        self.prefix = prefix
        #: `id(node) -> binding name` for nodes hoisted into a WITH block.
        #: Populated before rendering; see `_plan_bindings`.
        self.bindings: dict[int, str] = {}

    def param(self, value: Any) -> str:
        name = f'{self.prefix}{self.counter}'
        self.counter += 1
        self.params[name] = value
        return f'${name}'


def render_expr(node: _Node) -> tuple[str, dict[str, Any]]:
    """Render for a query: literals become bound parameters."""
    from pylon import stdlib

    ctx = _RenderCtx(inline_literals=False, context=stdlib.CONTEXT_EXPRESSION)
    return _render(node, ctx), ctx.params


def render_default_expr(node: _Node) -> str:
    """Render for a pointer default: literals are inlined as PyQL text.

    Returns text only — a `DEFAULT` clause has nowhere to bind parameters,
    so producing any would be a silent correctness bug rather than something
    the caller could handle.
    """
    from pylon import stdlib

    ctx = _RenderCtx(inline_literals=True, context=stdlib.CONTEXT_DEFAULT)
    text = _render(node, ctx)
    assert not ctx.params, 'inline_literals mode must not produce parameters'
    return text


def _pyql_literal(value: Any) -> str:
    """One Python value as PyQL literal text.

    PyQL has literal syntax for exactly four kinds — string, integer, float,
    boolean — so everything else is spelled as a cast over a string literal
    (`<uuid>'...'`), which is how the same values are written by hand.
    """
    import datetime as _dt
    import decimal as _decimal
    import uuid as _uuid

    if value is None:
        # Pylon has no null: an absent value is the empty set.
        return '{}'
    # bool before int — bool is a subclass of int.
    if isinstance(value, bool):
        return 'true' if value else 'false'
    if isinstance(value, int):
        return str(value)
    if isinstance(value, float):
        return repr(value)
    if isinstance(value, str):
        return _pyql_str(value)
    if isinstance(value, _uuid.UUID):
        return f'<uuid>{_pyql_str(str(value))}'
    if isinstance(value, _decimal.Decimal):
        return f'<decimal>{_pyql_str(str(value))}'
    # datetime before date — datetime is a subclass of date.
    if isinstance(value, _dt.datetime):
        kind = 'datetime' if value.tzinfo is not None else 'cal::local_datetime'
        return f'<{kind}>{_pyql_str(value.isoformat())}'
    if isinstance(value, _dt.date):
        return f'<cal::local_date>{_pyql_str(value.isoformat())}'
    if isinstance(value, _dt.time):
        return f'<cal::local_time>{_pyql_str(value.isoformat())}'
    if isinstance(value, _dt.timedelta):
        return f'<duration>{_pyql_str(str(value))}'
    if isinstance(value, bytes):
        # PyQL has no bytes literal syntax, so there is nothing correct to
        # emit here — better to say so than to guess at an encoding.
        raise InterfaceError(
            'bytes has no PyQL literal syntax and cannot be inlined into a default — '
            'use a Default(...) expression that produces the value instead'
        )
    raise InterfaceError(
        f'{type(value).__name__} cannot be inlined into a PyQL default — supported types are '
        'str, bool, int, float, Decimal, UUID, date/time/datetime, timedelta, and None'
    )


def _pyql_str(value: str) -> str:
    """A PyQL single-quoted string literal.

    Backslash first, so the escapes added for quotes aren't re-escaped.
    """
    escaped = value.replace('\\', '\\\\').replace("'", "\\'")
    return f"'{escaped}'"


def _render(node: _Node, ctx: _RenderCtx) -> str:
    if isinstance(node, _FieldPath):
        if not node.segments:
            raise InterfaceError('filter expression references the whole object, not a field')
        return '.' + '.'.join(node.segments)
    if isinstance(node, _Literal):
        return _pyql_literal(node.value) if ctx.inline_literals else ctx.param(node.value)
    if isinstance(node, _Compare):
        return f'{_render(node.left, ctx)} {node.op} {_render(node.right, ctx)}'
    if isinstance(node, _BoolOp):
        # Unconditionally parenthesize both sides — this is generated text,
        # never shown to a user, so there's no cost to over-parenthesizing
        # and it sidesteps any precedence bugs entirely.
        return f'({_render(node.left, ctx)}) {node.op} ({_render(node.right, ctx)})'
    if isinstance(node, _Not):
        return f'not ({_render(node.operand, ctx)})'
    if isinstance(node, _FuncCall):
        if node.module:
            from pylon import stdlib

            # Deferred to render time on purpose: the same `std.*` object is
            # admissible in one context and not the other, and it doesn't know
            # which it's in until something renders it.
            stdlib.check_context(node.module, node.name, len(node.args), ctx.context)
        args_text = ', '.join(_render(a, ctx) for a in node.args)
        prefix = f'{node.module}::' if node.module else ''
        return f'{prefix}{node.name}({args_text})'
    if isinstance(node, _SubQuery):
        binding = ctx.bindings.get(id(node.model_set))
        if binding is None:
            raise InterfaceError(
                'a query used inside another query must be bound first — this is a bug in '
                'pylon.modelquery, which should have planned a binding for it'
            )
        return binding
    raise TypeError(f'unsupported filter expression node: {node!r}')


# ── WITH bindings for nested queries ─────────────────────────────────────────
#
# A `ModelSet` reused inside another query becomes a `with` binding. The rule
# is Python's own variable semantics: the *same object* referenced twice is
# one binding, referenced twice. Unlike elsewhere, hoisting isn't optional —
# PyQL has no inline form for a sub-statement in expression position.


def _collect_subqueries(node: Any, found: list[Any], seen: set[int]) -> None:
    """Every distinct `ModelSet` reachable from `node`, in first-seen order."""
    if isinstance(node, _SubQuery):
        key = id(node.model_set)
        if key not in seen:
            seen.add(key)
            # A nested query may itself filter on another one.
            if node.model_set._expr is not None:
                _collect_subqueries(node.model_set._expr, found, seen)
            found.append(node.model_set)
        return
    if isinstance(node, (_Compare, _BoolOp)):
        _collect_subqueries(node.left, found, seen)
        _collect_subqueries(node.right, found, seen)
    elif isinstance(node, _Not):
        _collect_subqueries(node.operand, found, seen)
    elif isinstance(node, _FuncCall):
        for arg in node.args:
            _collect_subqueries(arg, found, seen)


def _with_prefix(expr: _Node | None, ctx: _RenderCtx) -> str:
    """The `with a := (...), b := (...) ` prefix for an expression, or ''.

    Bindings are emitted in the order collected, which is dependency-first
    because `_collect_subqueries` descends into a nested query's own filter
    before recording it — a binding may reference an earlier binding.
    """
    if expr is None:
        return ''
    found: list[Any] = []
    _collect_subqueries(expr, found, set())
    if not found:
        return ''

    parts = []
    for index, model_set in enumerate(found):
        name = f'__mq_q{index}'
        # Registered before rendering the body so a nested query that refers
        # to an earlier one emits the name rather than recursing.
        ctx.bindings[id(model_set)] = name
        parts.append(f'{name} := ({_render_select_text(model_set, ctx)})')
    return f'with {", ".join(parts)} '


# ── ModelSet: a chainable, unexecuted query against one type ────────────────


class ModelSet:
    """A model class plus an optional filter expression — returned by
    `Model.filter(...)`, consumed by `Client.query`/`Client.execute`."""

    __slots__ = ('_delete', '_expr', 'model')

    def __init__(self, model: type, expr: _Node | None = None, delete: bool = False) -> None:
        self.model = model
        self._expr = expr
        self._delete = delete

    def filter(self, *args: Any, **kwargs: Any) -> ModelSet:
        if self._delete:
            raise InterfaceError('cannot call .filter() on an already-built delete query')
        expr = self._expr
        for fn in args:
            if not callable(fn):
                raise TypeError(f'.filter() positional arguments must be callables, got {type(fn).__name__}')
            result = fn(_FieldPath([]))
            if not isinstance(result, _Node):
                raise TypeError(
                    '.filter() lambda must return a filter expression built from comparisons/'
                    f'&/|/~/std.*(...), got {type(result).__name__}'
                )
            expr = result if expr is None else _BoolOp('and', expr, result)
        for field_name, value in kwargs.items():
            cond = _Compare(_FieldPath([field_name]), '=', _as_node(value))
            expr = cond if expr is None else _BoolOp('and', expr, cond)
        return ModelSet(self.model, expr, self._delete)

    def delete(self) -> ModelSet:
        if self._expr is None:
            raise InterfaceError(
                'refusing to build an unfiltered delete — call .filter(...) first '
                '(this guards against accidentally deleting an entire table)'
            )
        return ModelSet(self.model, self._expr, delete=True)


def filter_classmethod(cls: type, *args: Any, **kwargs: Any) -> ModelSet:
    """The function installed as `Model.filter` (see `_inject_query_methods`
    in `pylon.schema._decorators`)."""
    return ModelSet(cls).filter(*args, **kwargs)


# ── PyQL text generation for SELECT / DELETE ─────────────────────────────────


def _qualified_type_name(model: type) -> str:
    cfg = model.__pylon_config__
    return f'{cfg.module}::{cfg.name}'


def _default_shape(model: type) -> str:
    cfg = model.__pylon_config__
    fields = [name for name, meta in cfg.pointers.items() if meta.kind == 'property']
    return ', '.join(fields)


def _render_select_text(ms: ModelSet, ctx: _RenderCtx) -> str:
    """`select T { shape } [filter ...]`, without any `with` prefix.

    Used both for a top-level select and for the body of a `with` binding,
    so the two can't drift.
    """
    text = f'select {_qualified_type_name(ms.model)} {{ {_default_shape(ms.model)} }}'
    if ms._expr is not None:
        text += f' filter {_render(ms._expr, ctx)}'
    return text


def render_select(ms: ModelSet) -> tuple[str, dict[str, Any]]:
    ctx = _RenderCtx(inline_literals=False, context=_expression_context())
    prefix = _with_prefix(ms._expr, ctx)
    return prefix + _render_select_text(ms, ctx), ctx.params


def render_delete(ms: ModelSet) -> tuple[str, dict[str, Any]]:
    assert ms._expr is not None, 'ModelSet.delete() already guards against a missing filter'
    ctx = _RenderCtx(inline_literals=False, context=_expression_context())
    prefix = _with_prefix(ms._expr, ctx)
    text = f'delete {_qualified_type_name(ms.model)} filter {_render(ms._expr, ctx)}'
    return prefix + text, ctx.params


def render(obj: Any) -> tuple[str, dict[str, Any]] | None:
    """Converts a bare `@pylon.type` class or a `ModelSet` into PyQL text +
    params, or returns `None` if `obj` isn't something this module knows how
    to render (the caller should fall back to its own "not a str" error)."""
    if isinstance(obj, type) and hasattr(obj, '__pylon_config__'):
        return render_select(ModelSet(obj))
    if isinstance(obj, ModelSet):
        return render_delete(obj) if obj._delete else render_select(obj)
    return None


# ── save(): INSERT/UPDATE PyQL generation for a single instance ─────────────
#
# New-vs-existing is decided by the presence of `__pylon_saved__` in
# `obj.__dict__` — stashed by `pylon.query._decode()` on every hydrated
# instance, absent on a freshly `__init__`'d one. It is deliberately not
# decided by whether `id` is set: the update path also needs the *baseline*
# values to diff against, which only a hydrated instance carries.
#
# An insert never writes `id`. That column belongs to the database, whose
# default may be `uuidv7()` or a schema-declared generator that exists only
# in PyQL; sending a value from Python would bypass it. A new instance that
# already carries an id is therefore an error rather than a silent override.

_UNSET = object()


def _link_target_name(meta: Any) -> str:
    """The qualified type name a link points at.

    `_walker.walk()` rewrites `link_target` from the target class to its
    `module::Name` string, but a model can be rendered before the schema has
    ever been walked (any unit test that skips `finalize()`), so both forms
    have to resolve here.
    """
    target = meta.link_target
    if isinstance(target, str):
        return target
    cfg = getattr(target, '__pylon_config__', None)
    if cfg is not None:
        return f'{cfg.module}::{cfg.name}'
    raise InterfaceError(f'link {meta.name!r} has an unresolved target {target!r}')


def _junction_class(through: Any) -> type | None:
    """The junction class behind a `Through[...]` link.

    `meta.through` is the class before `_walker.walk()` runs and its
    `module::Name` afterwards, so resolve the string form back through the
    registry.
    """
    if isinstance(through, type):
        return through
    if not isinstance(through, str):
        return None
    from pylon.schema._registry import snapshot

    types, _enums, _scalars = snapshot()
    for cls in types:
        cfg = getattr(cls, '__pylon_config__', None)
        if cfg is not None and f'{cfg.module}::{cfg.name}' == through:
            return cls
    return None


def _junction_property_types(through: Any) -> dict[str, str]:
    """`{link_property: pyql_type_name}` for a junction, for building casts."""
    junction = _junction_class(through)
    cfg = getattr(junction, '__pylon_config__', None)
    if cfg is None:
        return {}
    from pylon.schema._scalars import pyql_type_name

    out: dict[str, str] = {}
    for name, meta in cfg.pointers.items():
        if meta.kind != 'property' or name == 'id':
            continue
        type_name = pyql_type_name(meta.scalar_type)
        if type_name is not None:
            out[name] = type_name
    return out


def _require_no_mandatory_link_props(meta: Any, owner: str, *, has_props: bool = False) -> None:
    """Reject a link whose junction requires properties the caller didn't set.

    Only fires for the bare `+=`/`-=` spelling: `x.tags += [t]` carries a
    target and nothing else, so a junction needing `@weight` gets nothing and
    Postgres reports a bare `null value in column "weight" ... violates
    not-null constraint`, which says neither why nor what to do.
    `LinkSet.add(t, weight=...)` supplies them, and skips this check.
    """
    if has_props:
        return
    junction = _junction_class(meta.through)
    if junction is None:
        return
    cfg = getattr(junction, '__pylon_config__', None)
    if cfg is None:
        return
    required = [
        name
        for name, pointer in cfg.pointers.items()
        if pointer.kind == 'property' and not pointer.nullable and name != 'id'
    ]
    if required:
        plural = 'properties' if len(required) > 1 else 'property'
        kwargs = ', '.join(f'{name}=...' for name in sorted(required))
        raise InterfaceError(
            f'{owner}.{meta.name} links through {cfg.name}, which requires link {plural} '
            f'{", ".join(sorted(required))} — `+=` supplies a target and nothing else. '
            f'Use `.add(target, {kwargs})` instead.'
        )


def _expression_context() -> str:
    from pylon import stdlib

    return stdlib.CONTEXT_EXPRESSION


def _render_link_value(cfg: Any, owner: str, pointer: str, value: Any, ctx: _RenderCtx) -> str:
    """One single-link assignment value.

    Shared by `prepare_save` and the DML builders so the two can't drift —
    the same split (bare uuid vs. subquery) has to hold wherever a link is
    written.
    """
    meta = cfg.pointers[pointer]
    target_id = _link_target_id(value, owner, pointer)
    if meta.through is not None:
        _require_no_mandatory_link_props(meta, owner)
        # A `Through[...]` link has no FK column at all — it's stored in a
        # junction table, exactly like a multi-link, and the compiler rejects
        # a bare uuid for it ("multilink value must be a CTE reference,
        # parenthesised subquery, or type path").
        return f'(select {_link_target_name(meta)} filter .id = <uuid>{ctx.param(target_id)})'
    # A plain link is an FK column, so assign the uuid straight to it rather
    # than via `(select T filter .id = $p)`. The subquery form yields NULL
    # when no row matches, which silently clears an optional link; the bare
    # form hits the foreign key and reports the bad id.
    return f'<uuid>{ctx.param(target_id)}'


def _render_multilink_value(
    cfg: Any, owner: str, pointer: str, items: list[Any], ctx: _RenderCtx, *, has_props: bool = False
) -> str:
    """One multi-link assignment value — always a subquery, because the
    junction rows are built from the selected target rows.

    Members are matched by unpacking a single array parameter, so the query
    text is the same whatever the member count — a set literal of one
    parameter per member (`.id in {<uuid>$a, <uuid>$b}`) would produce a
    different query for every length, fragmenting the compile cache and the
    prepared statements behind it.
    """
    meta = cfg.pointers[pointer]
    if meta.through is not None:
        _require_no_mandatory_link_props(meta, owner, has_props=has_props)
    ids = _multilink_member_ids(items, owner, pointer)
    target = _link_target_name(meta)
    return f'(select {target} filter .id in std::array_unpack(<array<uuid>>{ctx.param(ids)}))'


# ── Save ordering ────────────────────────────────────────────────────────────
#
# `client.save(bob)` where `bob.company` is an unsaved Company has to write
# the Company first, so its id exists to reference. Rather than make the
# caller order that by hand, `save_order` walks the object graph and returns
# everything that needs writing, dependencies first.
#
# Deliberately *separate statements* rather than one composed statement with
# the nested insert hoisted into a `with` binding. A composed statement
# returns only the outer row's id, so the Company object would keep
# `id is None` and a later `save(company)` would insert a second row. One
# statement per object, inside the single transaction `save` already opens,
# gets every id back.


def _model_instances_in(value: Any) -> list[Any]:
    """Model instances referenced by one link/multi-link value, including
    those queued in a `LinkSet`'s pending operations."""
    from pylon.datatypes import LinkSet

    out: list[Any] = []
    if value is None:
        return out
    if hasattr(type(value), '__pylon_config__'):
        return [value]
    if isinstance(value, LinkSet):
        if value.is_hydrated:
            out.extend(list.__iter__(value))
        for op, items, _link_props in value.ops:
            if op != 'remove':
                out.extend(items)
        return out
    if isinstance(value, (list, tuple)):
        out.extend(value)
    return out


def _unsaved_dependencies(obj: Any) -> list[Any]:
    """Link targets of `obj` that can't be referenced yet because they have
    no id."""
    cfg = getattr(type(obj), '__pylon_config__', None)
    if cfg is None:
        return []
    out: list[Any] = []
    for name, meta in cfg.pointers.items():
        if meta.kind not in ('link', 'multilink') or name not in obj.__dict__:
            continue
        for target in _model_instances_in(obj.__dict__[name]):
            if hasattr(type(target), '__pylon_config__') and target.__dict__.get('id') is None:
                out.append(target)
    return out


def save_order(objs: Any) -> list[Any]:
    """Every object that needs saving, dependencies first.

    Includes unsaved link targets reachable from `objs`, so constructing an
    object graph and saving the root writes the whole graph. Objects are
    deduplicated by identity, so one shared target is written once.
    """
    ordered: list[Any] = []
    state: dict[int, int] = {}  # 0 = visiting, 1 = done

    def visit(obj: Any, path: list[Any]) -> None:
        key = id(obj)
        marker = state.get(key)
        if marker == 1:
            return
        if marker == 0:
            names = ' -> '.join(type(o).__name__ for o in [*path, obj])
            raise InterfaceError(
                f'circular reference between unsaved objects: {names} — '
                'save one of them first so the other can reference it by id'
            )
        state[key] = 0
        for dep in _unsaved_dependencies(obj):
            visit(dep, [*path, obj])
        state[key] = 1
        ordered.append(obj)

    for obj in objs:
        visit(obj, [])
    return ordered


def _link_target_id(value: Any, owner: str, pointer: str) -> Any:
    """The `id` of a model instance being assigned to a single link.

    Only a *saved* instance can be referenced by id. An unsaved one has no
    id yet, and quietly inserting it as a side effect of saving something
    else would be a surprising amount of hidden work — so it's rejected with
    the fix spelled out.
    """
    if not hasattr(type(value), '__pylon_config__'):
        raise InterfaceError(f'{owner}.{pointer} expects a model instance or None, got {type(value).__name__}')
    target_id = value.__dict__.get('id')
    if target_id is None:
        # `Client.save` writes unsaved targets first (see `save_order`), so
        # this is only reachable when `prepare_save` is called directly.
        target_cfg = type(value).__pylon_config__
        raise InterfaceError(
            f'cannot render {owner}.{pointer}: it points at an unsaved {target_cfg.name} instance '
            f'with no id — save it through `client.save()`, which writes link targets first'
        )
    return target_id


def _multilink_member_ids(items: list[Any], owner: str, pointer: str) -> list[Any]:
    return [_link_target_id(item, owner, pointer) for item in items]


def prepare_save(obj: Any) -> tuple[str, dict[str, Any]] | None:
    """Renders one INSERT (new instance) or UPDATE (hydrated instance) for
    `obj`, or `None` if a hydrated instance has nothing to write.

    Properties are diffed against the `__pylon_saved__` shadow. Multi-links
    can't be: `+=` on a member that is already linked is a no-op
    server-side, and so is `-=` on one that isn't, so neither shows up as a
    state change. They're replayed from the `LinkSet` op log instead (see
    `pylon.datatypes.LinkSet`).
    """
    cfg = type(obj).__pylon_config__
    type_name = f'{cfg.module}::{cfg.name}'
    saved = obj.__dict__.get('__pylon_saved__')
    ctx = _RenderCtx(inline_literals=False, context=_expression_context(), prefix='__mq_s')
    params = ctx.params

    def _param(value: Any) -> str:
        return ctx.param(value)

    def _link_value(value: Any, pointer: str) -> str:
        return _render_link_value(cfg, type_name, pointer, value, ctx)

    def _multilink_value(items: list[Any], pointer: str, *, has_props: bool = False) -> str:
        return _render_multilink_value(cfg, type_name, pointer, items, ctx, has_props=has_props)

    def _link_props(pointer: str, link_props: dict[str, Any]) -> str:
        """The `{ @name := <type>$p, ... }` suffix on a multi-link value.

        Each parameter is cast to the junction property's declared type. An
        uncast one binds as text, and PostgreSQL rejects it against a typed
        column (`column "weight" is of type double precision but expression
        is of type text`).
        """
        if not link_props:
            return ''
        types = _junction_property_types(cfg.pointers[pointer].through)
        parts = []
        for name, value in sorted(link_props.items()):
            cast = f'<{types[name]}>' if name in types else ''
            parts.append(f'@{name} := {cast}{ctx.param(value)}')
        return f' {{ {", ".join(parts)} }}'

    if saved is None:
        # The id is the database's to generate. Sending one would bypass
        # whatever the schema declares — `uuidv7()`, or a custom generator
        # like `Default('default::generate_typed_id(42)')` that only exists
        # in PyQL — and it also requires `allow_user_specified_id`, a guard
        # that shouldn't be tripped as a side effect of calling `save()`.
        if obj.__dict__.get('id') is not None:
            raise InterfaceError(
                f'cannot insert {type_name} with an id set from Python — ids are generated by the '
                "database (see the type's own default). Leave id unset and it will be assigned "
                'on save; to choose ids explicitly, write the insert as PyQL with '
                'allow_user_specified_id enabled.'
            )

        assignments = []
        for name, meta in cfg.pointers.items():
            if name == 'id' or name not in obj.__dict__:
                continue
            value = obj.__dict__[name]

            if meta.kind == 'property':
                # A field left at its Python-side default (None) is
                # indistinguishable from "never touched" — omit it so any
                # server-side default (id generation, Default(...), sequences)
                # still applies. An explicitly-set None on a genuinely nullable
                # field is unrepresentable here, but has the same net effect:
                # the column is simply omitted and stays NULL either way.
                if value is None:
                    continue
                assignments.append(f'{name} := {_param(value)}')
            elif meta.kind == 'link':
                if value is None:
                    continue
                assignments.append(f'{name} := {_link_value(value, name)}')
            elif meta.kind == 'multilink':
                # An insert has no prior state, so `+=` and `:=` collapse —
                # but link properties still have to travel with the members
                # they were set on, so each distinct set of values is its own
                # clause. The first becomes `:=`, the rest `+=`.
                for index, (members, link_props) in enumerate(_linkset_insert_groups(value)):
                    if not members:
                        continue
                    op = ':=' if index == 0 else '+='
                    target = _multilink_value(members, name, has_props=bool(link_props))
                    assignments.append(f'{name} {op} {target}{_link_props(name, link_props)}')

        if not assignments:
            raise InterfaceError(f'cannot save a new {type_name} instance with no fields set')
        return f'insert {type_name} {{ {", ".join(assignments)} }}', params

    assignments = []
    for name, meta in cfg.pointers.items():
        if meta.is_readonly or name not in obj.__dict__:
            continue
        current = obj.__dict__[name]

        if meta.kind == 'property':
            if current == saved.get(name, _UNSET):
                continue
            assignments.append(f'{name} := {_param(current)}')
        elif meta.kind == 'link':
            previous = saved.get(name, _UNSET)
            if _same_link(current, previous):
                continue
            # `{}` is the empty set, which in assignment position clears the
            # link — Pylon has no null to assign.
            value_text = '{}' if current is None else _link_value(current, name)
            assignments.append(f'{name} := {value_text}')
        elif meta.kind == 'multilink':
            assignments.extend(_multilink_assignments(current, name, _multilink_value, _link_props))

    if not assignments:
        return None
    pid = obj.__dict__.get('id')
    if pid is None:
        raise InterfaceError(f'cannot update {type_name}: instance has no id')
    id_param = _param(pid)
    return f'update {type_name} filter .id = {id_param} set {{ {", ".join(assignments)} }}', params


def _same_link(current: Any, previous: Any) -> bool:
    """Whether a single link is unchanged.

    Compared by target id rather than by object identity, so re-assigning an
    equal-but-distinct instance isn't written as a spurious update.
    """
    if previous is _UNSET:
        return False
    current_id = None if current is None else current.__dict__.get('id')
    previous_id = None if previous is None else getattr(previous, '__dict__', {}).get('id')
    return current_id == previous_id


def _linkset_insert_groups(value: Any) -> list[tuple[list[Any], dict[str, Any]]]:
    """Members to write for a multi-link on a *new* instance, grouped by the
    link-property values they carry.

    An insert has no prior state, so the `+=`/`:=` distinction collapses —
    but `@prop` values attach to a whole clause, so members set with
    different values still can't share one.
    """
    from pylon.datatypes import LinkSet

    if not isinstance(value, LinkSet):
        return [(list(value or []), {})]

    if not value.ops and value.is_hydrated:
        # Populated directly rather than through `+=`/`add()`.
        return [(list(value), {})]

    groups: list[tuple[list[Any], dict[str, Any]]] = []
    for op, items, link_props in _coalesce_ops(value.ops):
        if op == 'remove':
            # Nothing to remove from a row that doesn't exist yet.
            continue
        groups.append((list(items), link_props))
    return groups


def _linkset_insert_members(value: Any) -> list[Any]:
    """Every member an insert would write, ignoring link properties — used by
    `save_order` to find unsaved targets."""
    return [item for members, _props in _linkset_insert_groups(value) for item in members]


def _multilink_assignments(value: Any, name: str, render_value: Any, render_props: Any) -> list[str]:
    """`set { }` entries for one multi-link on an existing instance.

    Consecutive ops with the same kind *and* the same link-property values
    are coalesced, so `x += [a]; x += [b]` emits one `+=`. Ops that differ in
    either respect stay separate: `+= [a]` then `-= [a]` is not the same as
    the reverse, and `@prop` values attach to a whole `+=` clause, so two
    targets with different values need two clauses.
    """
    from pylon.datatypes import LinkSet

    if not isinstance(value, LinkSet):
        # A plain list assigned over the top (`obj.tags = [a, b]`) replaces
        # the whole set — there's no op log to consult, and a bare list can
        # only mean "these are the members now".
        items = list(value or [])
        return [f'{name} := {render_value(items, name)}'] if items else [f'{name} := {{}}']

    out: list[str] = []
    for op, items, link_props in _coalesce_ops(value.ops):
        if not items:
            continue
        target = render_value(items, name, has_props=bool(link_props)) + render_props(name, link_props)
        if op == 'set':
            out.append(f'{name} := {target}')
        elif op == 'add':
            out.append(f'{name} += {target}')
        elif op == 'remove':
            out.append(f'{name} -= {target}')
    return out


def mark_saved(obj: Any) -> None:
    """Reset an instance's change tracking after a committed save.

    Two distinct mechanisms have to be reset together: the `__pylon_saved__`
    shadow that properties and single links diff against, and the op log each
    multi-link `LinkSet` accumulates. Leaving the op log in place would
    replay every `+=`/`-=` on the next save.

    Called only after the transaction commits — see the note in
    `Client.save` about why resetting inside the retry loop would make a
    retried attempt skip work it had already "done" on a rolled-back try.
    """
    from pylon.datatypes import LinkSet

    cfg = type(obj).__pylon_config__
    shadow: dict[str, Any] = {}
    for name, meta in cfg.pointers.items():
        if name not in obj.__dict__:
            continue
        value = obj.__dict__[name]
        if meta.kind in ('property', 'link'):
            shadow[name] = value
        elif meta.kind == 'multilink' and isinstance(value, LinkSet):
            value.clear_ops()
    obj.__dict__['__pylon_saved__'] = shadow


def _coalesce_ops(
    ops: list[tuple[str, list[Any], dict[str, Any]]],
) -> list[tuple[str, list[Any], dict[str, Any]]]:
    """Merge adjacent ops that would render identically.

    Link-property values are part of the identity: they attach to a whole
    `+=` clause, so two adds with different values can't share one.
    """
    merged: list[tuple[str, list[Any], dict[str, Any]]] = []
    for op, items, link_props in ops:
        if merged and merged[-1][0] == op and merged[-1][2] == link_props:
            merged[-1][1].extend(items)
        else:
            merged.append((op, list(items), link_props))
    return merged
