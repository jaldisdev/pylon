"""Tests for @pylon.function declaration hygiene and schema-build validation.

NOTE: This file intentionally omits ``from __future__ import annotations`` so
that annotations inside test-local functions evaluate eagerly, matching
test_finalize.py's rationale for the same omission.
"""

import pytest

import pylon.exceptions as pylon_exceptions
import pylon.schema as pylon
from pylon.schema._registry import clear as clear_registry, functions_snapshot, snapshot
from pylon.schema._walker import SchemaError, walk

MISSING_LANGUAGE = "sql"


@pytest.fixture(autouse=True)
def _isolated_registry():
    """Snapshot/restore the registry around each test to avoid cross-test leakage."""
    before = snapshot()
    yield
    clear_registry()
    types, enums, scalars = before
    from pylon.schema._registry import register_type, register_enum, register_scalar
    for t in types:
        register_type(t)
    for e in enums:
        register_enum(e)
    for s in scalars:
        register_scalar(s)


class TestVolatilityValidation:
    def test_valid_volatility_accepted(self):
        @pylon.function_decorator(module="t", volatility=pylon.Volatility.Immutable)
        def f(a: pylon.Int64) -> pylon.Int64:
            """select a"""

        assert f.__pylon_function__.volatility == pylon.Volatility.Immutable

    def test_none_volatility_accepted(self):
        @pylon.function_decorator(module="t")
        def f(a: pylon.Int64) -> pylon.Int64:
            """select a"""

        assert f.__pylon_function__.volatility is None

    def test_invalid_volatility_rejected(self):
        with pytest.raises(SchemaError, match="invalid volatility"):
            @pylon.function_decorator(module="t", volatility="bogus")
            def f(a: pylon.Int64) -> pylon.Int64:
                """select a"""


class TestLanguageValidation:
    def test_default_language_accepted(self):
        @pylon.function_decorator(module="t")
        def f(a: pylon.Int64) -> pylon.Int64:
            """select a"""

        assert f.__pylon_function__.language == pylon.Language.PyQL

    def test_unsupported_language_rejected(self):
        with pytest.raises(SchemaError, match="invalid language"):
            @pylon.function_decorator(module="t", language=MISSING_LANGUAGE)
            def f(a: pylon.Int64) -> pylon.Int64:
                """select a"""


class TestReturnTypeConsistency:
    """Compiled-body type checking — function/computed/default declared type
    must match what the PyQL body actually produces (crates/pylon-core/src/validate.rs)."""

    def test_function_return_type_match_passes(self):
        @pylon.function_decorator(module="t", name="good_fn")
        def good_fn(a: pylon.Int64) -> pylon.Int64:
            """select a"""

        schema = walk([], [], [], [], functions=functions_snapshot())
        assert schema is not None

    def test_function_return_type_mismatch_rejected(self):
        @pylon.function_decorator(module="t", name="bad_fn")
        def bad_fn(a: pylon.Int64) -> pylon.Str:
            """select a"""

        with pytest.raises(pylon_exceptions.SchemaError, match="return type mismatch in function"):
            walk([], [], [], [], functions=functions_snapshot())

    def test_computed_return_type_match_passes(self):
        @pylon.type(module="t", name="Person")
        class Person:
            name: pylon.Str
            greeting: pylon.Computed[pylon.Str, ".name"]

        schema = walk([Person], [], [], [])
        assert schema is not None

    def test_computed_return_type_mismatch_rejected(self):
        @pylon.type(module="t", name="Person")
        class Person:
            name: pylon.Str
            bad: pylon.Computed[pylon.Int64, ".name"]

        with pytest.raises(pylon_exceptions.SchemaError, match="return type mismatch in computed pointer"):
            walk([Person], [], [], [])

    def test_default_type_match_passes(self):
        @pylon.type(module="t", name="Person")
        class Person:
            score: pylon.Property[pylon.Int64, pylon.Default(1)]

        schema = walk([Person], [], [], [])
        assert schema is not None

    def test_default_type_mismatch_rejected(self):
        @pylon.type(module="t", name="Person")
        class Person:
            score: pylon.Property[pylon.Int64, pylon.Default("'not a number'")]

        with pytest.raises(pylon_exceptions.SchemaError, match="default value type mismatch"):
            walk([Person], [], [], [])

    def test_rewrite_type_match_passes(self):
        @pylon.type(module="t", name="Person")
        class Person:
            name: pylon.Property[pylon.Str, pylon.Rewrite(pylon.On.Insert, "'unnamed'")]

        schema = walk([Person], [], [], [])
        assert schema is not None

    def test_rewrite_type_mismatch_rejected(self):
        @pylon.type(module="t", name="Person")
        class Person:
            name: pylon.Property[pylon.Str, pylon.Rewrite(pylon.On.Insert, "1")]

        with pytest.raises(pylon_exceptions.SchemaError, match="rewrite handler type mismatch"):
            walk([Person], [], [], [])

    def test_trigger_handler_compiles_passes(self):
        @pylon.type(module="t", name="Person")
        class Person:
            name: pylon.Str
            pylon.Trigger(on=pylon.On.Insert, timing=pylon.Timing.After, handler="select Person")

        schema = walk([Person], [], [], [])
        assert schema is not None

    def test_trigger_handler_unknown_field_rejected(self):
        @pylon.type(module="t", name="Person")
        class Person:
            name: pylon.Str
            pylon.Trigger(
                on=pylon.On.Insert,
                timing=pylon.Timing.After,
                handler="select Person filter .nonexistent_field = 1",
            )

        with pytest.raises(pylon_exceptions.SchemaError, match="nonexistent_field"):
            walk([Person], [], [], [])


class TestDuplicateFunctionSignature:
    def test_duplicate_signature_rejected(self):
        @pylon.function_decorator(module="shop", name="mysum")
        def mysum(a: pylon.Int64, b: pylon.Int64) -> pylon.Int64:
            """select a + b"""

        @pylon.function_decorator(module="shop", name="mysum")
        def mysum2(a: pylon.Int64, b: pylon.Int64) -> pylon.Int64:
            """select a - b"""

        with pytest.raises(SchemaError, match="Duplicate function signature"):
            walk([], [], [], [], functions=functions_snapshot())

    def test_overload_with_different_param_types_allowed(self):
        @pylon.function_decorator(module="shop", name="mysum")
        def mysum(a: pylon.Int64, b: pylon.Int64) -> pylon.Int64:
            """select a + b"""

        @pylon.function_decorator(module="shop", name="mysum")
        def mysum_float(a: pylon.Float64, b: pylon.Float64) -> pylon.Float64:
            """select a + b"""

        schema = walk([], [], [], [], functions=functions_snapshot())
        assert schema is not None

    def test_same_signature_different_module_allowed(self):
        @pylon.function_decorator(module="shop", name="mysum")
        def mysum(a: pylon.Int64, b: pylon.Int64) -> pylon.Int64:
            """select a + b"""

        @pylon.function_decorator(module="cal", name="mysum")
        def mysum2(a: pylon.Int64, b: pylon.Int64) -> pylon.Int64:
            """select a + b"""

        schema = walk([], [], [], [], functions=functions_snapshot())
        assert schema is not None
