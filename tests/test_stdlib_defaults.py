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

"""`std` expressions as pointer defaults, and the inline-literal renderer
that makes them expressible.

NOTE: this file intentionally omits ``from __future__ import annotations``.
The schema DSL reads pointer metadata out of evaluated class-body
annotations, so postponed evaluation would leave `Property[..., Default(...)]`
as an unparsed string and every `Default` here would silently go missing —
which is precisely the failure mode these tests exist to catch.
"""

import datetime
import decimal
import uuid

import pytest

from pylon import std
from pylon.exceptions import InterfaceError
from pylon.modelquery import _Literal, render_default_expr


def _lit(value: object) -> str:
    return render_default_expr(_Literal(value))


class TestInlineLiterals:
    def test_no_parameters_are_produced(self):
        # A DEFAULT clause has nowhere to bind parameters, so producing one
        # would be a silent correctness bug.
        assert '$' not in render_default_expr(std.str_lower('ABC'))

    @pytest.mark.parametrize(
        ('value', 'expected'),
        [
            (42, '42'),
            (-7, '-7'),
            (3.5, '3.5'),
            (True, 'true'),
            (False, 'false'),
            ('draft', "'draft'"),
            (None, '{}'),
        ],
    )
    def test_primitive_literals(self, value, expected):
        assert _lit(value) == expected

    def test_bool_is_not_rendered_as_an_int(self):
        # bool subclasses int, so order of checks matters.
        assert _lit(True) == 'true'
        assert _lit(1) == '1'

    def test_none_is_the_empty_set(self):
        # Pylon has no null — an absent value is the empty set.
        assert _lit(None) == '{}'

    def test_string_quotes_are_escaped(self):
        assert _lit("it's") == r"'it\'s'"

    def test_backslashes_are_escaped_before_quotes(self):
        # Escaping quotes first would double-escape the backslashes it added.
        assert _lit('a\\b') == r"'a\\b'"
        assert _lit("\\'") == r"'\\\''"

    def test_uuid_is_cast(self):
        assert _lit(uuid.UUID(int=7)) == "<uuid>'00000000-0000-0000-0000-000000000007'"

    def test_decimal_is_cast_from_a_string(self):
        # Via str, not float — the point of Decimal is the exact value.
        assert _lit(decimal.Decimal('1.10')) == "<decimal>'1.10'"

    def test_naive_datetime_is_local(self):
        assert _lit(datetime.datetime(2026, 1, 2, 3, 4, 5)) == "<cal::local_datetime>'2026-01-02T03:04:05'"

    def test_aware_datetime_is_a_datetime(self):
        value = datetime.datetime(2026, 1, 2, 3, 4, 5, tzinfo=datetime.UTC)
        assert _lit(value).startswith('<datetime>')

    def test_date_and_time(self):
        assert _lit(datetime.date(2026, 1, 2)) == "<cal::local_date>'2026-01-02'"
        assert _lit(datetime.time(1, 2, 3)) == "<cal::local_time>'01:02:03'"

    def test_bytes_is_rejected_with_a_reason(self):
        # PyQL has no bytes literal syntax; guessing an encoding would be
        # worse than saying so.
        with pytest.raises(InterfaceError, match='no PyQL literal syntax'):
            _lit(b'abc')

    def test_unsupported_type_is_rejected(self):
        with pytest.raises(InterfaceError, match='cannot be inlined'):
            _lit(object())


class TestDefaultExpressions:
    def test_zero_arg_volatile_function(self):
        assert render_default_expr(std.uuid_generate_v7()) == 'std::uuid_generate_v7()'

    def test_nested_call_with_a_literal(self):
        assert render_default_expr(std.str_lower('ABC')) == "std::str_lower('ABC')"

    def test_aggregate_is_rejected(self):
        with pytest.raises(InterfaceError, match='is an aggregate'):
            render_default_expr(std.count('x'))

    def test_set_returning_is_rejected(self):
        with pytest.raises(InterfaceError, match='returns a set'):
            render_default_expr(std.array_unpack('x'))

    def test_query_only_gate_does_not_apply(self):
        # Volatility is the whole point of a std default, so the expression
        # context's restrictions must not leak into this one.
        assert render_default_expr(std.datetime_current()) == 'std::datetime_current()'


class TestSchemaIntegration:
    """`Default(...)` reaching `default_pyql` on the descriptor."""

    def _pointer(self, annotation_default):
        import pylon
        from pylon.schema import Default, Property

        @pylon.type
        class WithDefault:
            name: Property[pylon.Str]
            token: Property[pylon.UUID, Default(annotation_default)]

        return WithDefault

    def test_std_expression_reaches_default_pyql(self):
        from pylon.schema._walker import _make_default_pyql

        model = self._pointer(std.uuid_generate_v7())
        meta = model.__pylon_config__.pointers['token']
        assert _make_default_pyql(meta) == 'std::uuid_generate_v7()'

    def test_string_form_still_works(self):
        from pylon.schema._walker import _make_default_pyql

        model = self._pointer('std::uuid_generate_v7()')
        meta = model.__pylon_config__.pointers['token']
        assert _make_default_pyql(meta) == 'std::uuid_generate_v7()'

    def test_expression_default_emits_no_sql_default(self):
        # The PyQL path and the SQL path are mutually exclusive; an
        # expression default must not also produce a SQL literal.
        from pylon.schema._walker import _make_default_sql

        model = self._pointer(std.uuid_generate_v7())
        meta = model.__pylon_config__.pointers['token']
        assert _make_default_sql(meta) is None

    def test_unsupported_sentinel_raises_instead_of_vanishing(self):
        # Previously an unrecognized sentinel fell through both the SQL and
        # PyQL paths, emitting no default at all and saying nothing.
        from pylon.schema._walker import _make_default_pyql

        model = self._pointer(object())
        meta = model.__pylon_config__.pointers['token']
        with pytest.raises(TypeError, match='not a supported default'):
            _make_default_pyql(meta)

    @pytest.mark.parametrize('value', [0, 1.5, True, False, None])
    def test_literal_sentinels_still_take_the_sql_path(self, value):
        from pylon.schema._walker import _make_default_pyql

        model = self._pointer(value)
        meta = model.__pylon_config__.pointers['token']
        assert _make_default_pyql(meta) is None

    def test_a_string_sentinel_is_a_pyql_expression_not_a_string_literal(self):
        # Long-standing behaviour, unchanged here and easy to trip over:
        # `Default('draft')` compiles `draft` as PyQL, it does not default the
        # column to the text "draft". A literal string default needs its own
        # quotes: `Default("'draft'")`.
        from pylon.schema._walker import _make_default_pyql, _make_default_sql

        model = self._pointer('draft')
        meta = model.__pylon_config__.pointers['token']
        assert _make_default_pyql(meta) == 'draft'
        assert _make_default_sql(meta) is None


class TestCompiles:
    """The generated text has to survive the real compiler, not just look
    right. `validate_schema_types` compiles every `default_pyql` and infers
    its type, so it catches both unparseable text and a default whose value
    doesn't fit the column."""

    def _schema(self, module, declare):
        import pylon
        from pylon.schema._registry import clear as clear_registry
        from pylon.schema._registry import snapshot
        from pylon.schema._walker import walk

        clear_registry()
        declare(pylon, module)
        types, enums, scalars = snapshot()
        return walk(types, enums, scalars, [])

    def test_std_default_compiles_and_type_checks(self):
        from pylon._core import validate_schema_types

        from pylon.schema import Default, Property

        def declare(pylon, module):
            @pylon.type(module=module, name='Token')
            class Token:
                token: Property[pylon.UUID, Default(std.uuid_generate_v7())]

        schema = self._schema('stdlib_default_ok', declare)
        validate_schema_types(schema)  # raises if the default won't compile

    @pytest.mark.xfail(
        reason='validate.rs infers no type for a stdlib call, so the mismatch is not detected yet',
        strict=True,
    )
    def test_mistyped_std_default_is_reported(self):
        # str_lower produces str; the column is a uuid. validate.rs already
        # has the machinery (compile + infer_ir_type + types_compatible) but
        # infer_ir_type yields nothing for a function call, so the check is
        # skipped rather than failing. Marked strict so this starts passing
        # loudly if inference gains that case.
        from pylon._core import validate_schema_types

        from pylon.exceptions import PylonError
        from pylon.schema import Default, Property

        def declare(pylon, module):
            @pylon.type(module=module, name='Token')
            class Token:
                token: Property[pylon.UUID, Default(std.str_lower('abc'))]

        schema = self._schema('stdlib_default_bad', declare)
        with pytest.raises(PylonError, match='token'):
            validate_schema_types(schema)

    def test_escaped_literal_survives_compilation(self):
        # A mis-escaped quote produces text that looks fine here but only
        # fails once a migration is generated, so compile it for real.
        from pylon._core import validate_schema_types

        from pylon.schema import Default, Property

        def declare(pylon, module):
            @pylon.type(module=module, name='Note')
            class Note:
                body: Property[pylon.Str, Default(std.str_lower("it's \\ tricky"))]

        schema = self._schema('stdlib_default_escape', declare)
        validate_schema_types(schema)

    def test_export_schema_emits_the_default_on_the_right_column(self):
        # Asserting `'uuidv7()' in ddl` alone passes vacuously — every table's
        # `id` column already defaults to uuidv7(). The assertion has to name
        # the column, or it can't fail.
        from pylon._core import export_schema

        from pylon.schema import Default, Property

        def declare(pylon, module):
            @pylon.type(module=module, name='Token')
            class Token:
                token: Property[pylon.UUID, Default(std.uuid_generate_v7())]
                made: Property[pylon.DateTime, Default(std.datetime_current())]

        ddl = export_schema(self._schema('stdlib_default_ddl', declare))
        assert '"token" uuid NOT NULL DEFAULT uuidv7()' in ddl, ddl
        assert '"made" timestamptz NOT NULL DEFAULT clock_timestamp()' in ddl, ddl

    def test_an_enum_default_emits_its_label(self):
        # A Pylon enum member is a `str`, so it used to take the PyQL path,
        # where its label parses as a bare identifier and compiles to nothing
        # -- leaving a NOT NULL column with no default at all.
        from pylon._core import export_schema

        from pylon.schema import Default, Property

        def declare(pylon, module):
            @pylon.enum('Guest', 'Member')
            class MembershipType(pylon.Enum):
                pass

            @pylon.type(module=module, name='Membership')
            class Membership:
                type: Property[MembershipType, Default(MembershipType.Member)]

        ddl = export_schema(self._schema('stdlib_default_enum', declare))
        assert '"type" ' in ddl and "DEFAULT 'Member'" in ddl, ddl

    def test_string_form_emits_the_same_default(self):
        from pylon._core import export_schema

        from pylon.schema import Default, Property

        def declare(pylon, module):
            @pylon.type(module=module, name='Token')
            class Token:
                token: Property[pylon.UUID, Default('std::uuid_generate_v7()')]

        ddl = export_schema(self._schema('stdlib_default_str', declare))
        assert '"token" uuid NOT NULL DEFAULT uuidv7()' in ddl, ddl
