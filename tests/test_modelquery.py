"""Unit tests for pylon.modelquery — the expression DSL and PyQL text
renderer. Pure, no DB/Client involvement."""

from __future__ import annotations

import uuid
from types import SimpleNamespace

import pytest

from pylon import modelquery
from pylon.exceptions import InterfaceError
from pylon.modelquery import ModelSet, cal, math, render, render_expr, std


# ── Fixtures ─────────────────────────────────────────────────────────────────


def _pointer(kind: str):
    return SimpleNamespace(kind=kind)


class Person:
    """A stand-in for an @pylon.type class — just needs __pylon_config__."""

    __pylon_config__ = SimpleNamespace(
        module="default",
        name="Person",
        pointers={
            "id": _pointer("property"),
            "name": _pointer("property"),
            "age": _pointer("property"),
            "company": _pointer("link"),
            "friends": _pointer("multilink"),
            "full_name": _pointer("computed"),
        },
    )


class Account:
    """A stand-in for an interface type — no .filter attached in real usage,
    but the renderer itself doesn't care; the gating lives in _decorators."""

    __pylon_config__ = SimpleNamespace(module="default", name="Account", pointers={})


# ── Expression tree / render_expr ───────────────────────────────────────────


class TestRenderExpr:
    def test_simple_equality_comparison(self):
        node = modelquery._FieldPath([]).name == "Bob"
        text, params = render_expr(node)
        assert text == ".name = $__mq_p0"
        assert params == {"__mq_p0": "Bob"}

    def test_comparison_operators(self):
        u = modelquery._FieldPath([])
        cases = [
            (u.age < 30, "<"),
            (u.age <= 30, "<="),
            (u.age > 30, ">"),
            (u.age >= 30, ">="),
            (u.age != 30, "!="),
        ]
        for node, op in cases:
            text, params = render_expr(node)
            assert text == f".age {op} $__mq_p0", text
            assert params == {"__mq_p0": 30}

    def test_nested_field_path(self):
        u = modelquery._FieldPath([])
        text, params = render_expr(u.company.name == "Acme")
        assert text == ".company.name = $__mq_p0"
        assert params == {"__mq_p0": "Acme"}

    def test_boolean_and_is_parenthesized(self):
        u = modelquery._FieldPath([])
        node = (u.name == "Bob") & (u.age > 30)
        text, params = render_expr(node)
        assert text == "(.name = $__mq_p0) and (.age > $__mq_p1)"
        assert params == {"__mq_p0": "Bob", "__mq_p1": 30}

    def test_boolean_or_is_parenthesized(self):
        u = modelquery._FieldPath([])
        node = (u.name == "Bob") | (u.name == "Robert")
        text, params = render_expr(node)
        assert text == "(.name = $__mq_p0) or (.name = $__mq_p1)"

    def test_nested_boolean_combination_parenthesizes_every_level(self):
        u = modelquery._FieldPath([])
        node = ((u.name == "Bob") & (u.age > 30)) | (u.name == "Robert")
        text, params = render_expr(node)
        assert text == "((.name = $__mq_p0) and (.age > $__mq_p1)) or (.name = $__mq_p2)"
        assert params == {"__mq_p0": "Bob", "__mq_p1": 30, "__mq_p2": "Robert"}

    def test_not_operator(self):
        u = modelquery._FieldPath([])
        node = ~(u.name == "Bob")
        text, params = render_expr(node)
        assert text == "not (.name = $__mq_p0)"

    def test_std_function_call(self):
        u = modelquery._FieldPath([])
        node = std.foo(u.name, "x")
        text, params = render_expr(node)
        assert text == "std::foo(.name, $__mq_p0)"
        assert params == {"__mq_p0": "x"}

    def test_math_and_cal_namespaces_render_their_own_module(self):
        u = modelquery._FieldPath([])
        text, _ = render_expr(math.sqrt(u.age))
        assert text == "math::sqrt(.age)"
        text, _ = render_expr(cal.today())
        assert text == "cal::today()"

    def test_nested_func_call_inside_comparison(self):
        u = modelquery._FieldPath([])
        node = std.foo(u.name) == "x"
        text, params = render_expr(node)
        assert text == "std::foo(.name) = $__mq_p0"

    def test_ilike_renders_as_infix_operator_not_a_function_call(self):
        # std::ilike has no bare callable form in the PyQL stdlib — only
        # the `x ilike y` infix operator exists, so std.ilike(...) must
        # render using that surface syntax, not std::ilike(...).
        u = modelquery._FieldPath([])
        node = std.ilike(u.name, "%bob%")
        text, params = render_expr(node)
        assert text == ".name ilike $__mq_p0"
        assert params == {"__mq_p0": "%bob%"}

    def test_ilike_wrong_arg_count_raises(self):
        u = modelquery._FieldPath([])
        with pytest.raises(TypeError, match="exactly 2 arguments"):
            std.ilike(u.name)

    def test_field_to_field_comparison(self):
        u = modelquery._FieldPath([])
        text, params = render_expr(u.name == u.company.name)
        assert text == ".name = .company.name"
        assert params == {}

    def test_uuid_literal_is_parameterized_not_inlined(self):
        u = modelquery._FieldPath([])
        pid = uuid.uuid4()
        text, params = render_expr(u.id == pid)
        assert text == ".id = $__mq_p0"
        assert params == {"__mq_p0": pid}

    def test_boolean_context_raises_a_clear_error(self):
        u = modelquery._FieldPath([])
        node = u.name == "Bob"
        with pytest.raises(TypeError, match="boolean context"):
            bool(node)
        with pytest.raises(TypeError, match="boolean context"):
            if node:  # noqa: SIM108
                pass

    def test_bare_root_path_cannot_be_rendered(self):
        with pytest.raises(InterfaceError, match="whole object"):
            render_expr(modelquery._FieldPath([]))


# ── ModelSet / render_select / render_delete ────────────────────────────────


class TestModelSet:
    def test_bare_class_renders_default_shape_no_filter(self):
        text, params = render(Person)
        assert text == "select default::Person { id, name, age }"
        assert params == {}

    def test_default_shape_excludes_links_multilinks_and_computed(self):
        text, _ = render(Person)
        assert "company" not in text
        assert "friends" not in text
        assert "full_name" not in text

    def test_lambda_filter(self):
        ms = ModelSet(Person).filter(lambda u: u.name == "Bob")
        text, params = render(ms)
        assert text == "select default::Person { id, name, age } filter .name = $__mq_p0"
        assert params == {"__mq_p0": "Bob"}

    def test_kwarg_filter(self):
        pid = uuid.uuid4()
        ms = ModelSet(Person).filter(id=pid)
        text, params = render(ms)
        assert text == "select default::Person { id, name, age } filter .id = $__mq_p0"
        assert params == {"__mq_p0": pid}

    def test_multiple_kwargs_are_anded(self):
        ms = ModelSet(Person).filter(name="Bob", age=30)
        text, params = render(ms)
        assert " and " in text
        assert params == {"__mq_p0": "Bob", "__mq_p1": 30}

    def test_chained_filter_calls_are_anded(self):
        ms = ModelSet(Person).filter(name="Bob").filter(age=30)
        text, params = render(ms)
        assert " and " in text
        assert params == {"__mq_p0": "Bob", "__mq_p1": 30}

    def test_lambda_and_kwargs_combined_in_one_call(self):
        ms = ModelSet(Person).filter(lambda u: u.age > 18, name="Bob")
        text, params = render(ms)
        assert " and " in text

    def test_filter_positional_arg_must_be_callable(self):
        with pytest.raises(TypeError, match="callables"):
            ModelSet(Person).filter("not a lambda")

    def test_filter_lambda_must_return_an_expression(self):
        with pytest.raises(TypeError, match="filter expression"):
            ModelSet(Person).filter(lambda u: "not an expression")

    def test_delete_requires_a_prior_filter(self):
        with pytest.raises(InterfaceError, match="unfiltered delete"):
            ModelSet(Person).delete()

    def test_filtered_delete_renders_delete_statement(self):
        pid = uuid.uuid4()
        ms = ModelSet(Person).filter(id=pid).delete()
        text, params = render(ms)
        assert text == "delete default::Person filter .id = $__mq_p0"
        assert params == {"__mq_p0": pid}

    def test_cannot_filter_after_delete(self):
        ms = ModelSet(Person).filter(name="Bob").delete()
        with pytest.raises(InterfaceError, match="already-built delete"):
            ms.filter(age=30)

    def test_render_returns_none_for_unrecognized_objects(self):
        assert render("select 1") is None
        assert render(object()) is None
        assert render(42) is None

    def test_filter_classmethod_helper(self):
        ms = modelquery.filter_classmethod(Person, name="Bob")
        assert isinstance(ms, ModelSet)
        text, _ = render(ms)
        assert "filter .name" in text
