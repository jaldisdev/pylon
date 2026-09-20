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

"""The `std`/`math`/`cal` namespaces and the registry gate behind them."""

from __future__ import annotations

import pytest

from pylon import cal, math, modelquery, std, stdlib
from pylon.exceptions import InterfaceError
from pylon.modelquery import render_expr


def _path(*segments: str) -> modelquery._FieldPath:
    return modelquery._FieldPath(list(segments))


class TestRegistry:
    def test_registry_is_populated(self):
        assert len(stdlib.registry()) > 100

    def test_every_entry_has_the_expected_keys(self):
        expected = {
            'namespace',
            'name',
            'qualified_name',
            'params',
            'return_type',
            'volatility',
            'cast_target',
            'aggregate',
            'returns_set',
            'intrinsic',
            'variadic',
        }
        for entry in stdlib.registry():
            assert set(entry) == expected, entry['qualified_name']

    def test_only_default_namespaces_are_materialized(self):
        # postgis alone is several thousand overloads; it stays opt-in.
        assert {e['namespace'] for e in stdlib.registry()} <= set(stdlib.DEFAULT_NAMESPACES)

    def test_volatility_is_classified(self):
        assert stdlib.overloads('std', 'uuid_generate_v7')[0]['volatility'] == 'volatile'
        assert stdlib.overloads('std', 'random')[0]['volatility'] == 'volatile'
        assert stdlib.overloads('std', 'str_lower')[0]['volatility'] == 'immutable'
        assert all(e['volatility'] == 'modifying' for e in stdlib.overloads('std', 'sequence_reset'))

    def test_aggregate_and_set_shape_are_flagged(self):
        assert all(e['aggregate'] for e in stdlib.overloads('std', 'count'))
        assert all(e['returns_set'] for e in stdlib.overloads('std', 'array_unpack'))
        assert not any(e['aggregate'] for e in stdlib.overloads('std', 'str_lower'))

    def test_infix_aliases_do_not_shadow_real_functions(self):
        # `ilike`/`in_`/... are grammar keywords the namespace offers as
        # pseudo-functions. If one ever gained a real registry entry the alias
        # would silently win and hide it.
        for alias in modelquery._INFIX_ALIASES:
            assert alias not in stdlib.names('std'), alias


class TestGeneratedStub:
    def test_stub_is_current(self):
        # Editors don't run generators, so `namespaces.pyi` is checked in.
        # It has to be regenerated whenever the registry changes.
        from pylon import _stubgen

        assert _stubgen.stub_path().read_text() == _stubgen.generate(), (
            'pylon/namespaces.pyi is stale — regenerate with `python -m pylon._stubgen`'
        )

    def test_stub_is_valid_python(self):
        import ast

        from pylon import _stubgen

        ast.parse(_stubgen.stub_path().read_text())

    def test_keyword_names_are_escaped_in_the_stub(self):
        from pylon import _stubgen

        text = _stubgen.stub_path().read_text()
        assert 'def assert_(' in text
        assert 'def assert(' not in text


class TestKeywordNames:
    def test_assert_is_reachable_under_a_trailing_underscore(self):
        # `std.assert(...)` is a Python syntax error, so the function would be
        # unreachable without the alias.
        text, _ = render_expr(std.assert_(_path('age')))
        assert text == 'std::assert(.age)'

    def test_name_mapping_round_trips(self):
        assert stdlib.python_name('assert') == 'assert_'
        assert stdlib.pyql_name('assert_') == 'assert'
        assert stdlib.python_name('str_lower') == 'str_lower'
        assert stdlib.pyql_name('str_lower') == 'str_lower'

    def test_non_keyword_trailing_underscore_is_not_stripped(self):
        # `str_lower_` must stay unknown rather than silently resolving to
        # `str_lower`, since only keyword collisions get the alias.
        assert stdlib.pyql_name('str_lower_') == 'str_lower_'
        assert not hasattr(std, 'str_lower_')

    def test_dir_lists_the_python_spelling(self):
        assert 'assert_' in dir(std)
        assert 'assert' not in dir(std)


class TestNamespaceCalls:
    def test_builds_a_function_call(self):
        text, params = render_expr(std.str_lower(_path('name')))
        assert text == 'std::str_lower(.name)'
        assert params == {}

    def test_literal_arguments_become_parameters(self):
        text, params = render_expr(std.str_pad_start(_path('name'), 10, '0'))
        assert text == 'std::str_pad_start(.name, $__mq_p0, $__mq_p1)'
        assert params == {'__mq_p0': 10, '__mq_p1': '0'}

    def test_infix_alias_renders_as_an_operator(self):
        text, params = render_expr(std.ilike(_path('name'), '%bob%'))
        assert text == '.name ilike $__mq_p0'
        assert params == {'__mq_p0': '%bob%'}

    def test_variadic_accepts_any_trailing_count(self):
        for extra in ([], ['a'], ['a', 'b', 'c']):
            node = std.json_get(_path('data'), *extra)
            assert render_expr(node)[0].startswith('std::json_get(.data')

    def test_call_result_is_comparable(self):
        text, _ = render_expr(std.str_lower(_path('name')) == 'bob')
        assert text == 'std::str_lower(.name) = $__mq_p0'

    def test_dir_lists_registry_and_alias_names(self):
        listing = dir(std)
        assert 'str_lower' in listing
        assert 'ilike' in listing

    def test_namespaces_are_independent(self):
        assert 'pi' in dir(math)
        assert 'to_local_date' in dir(cal)


class TestConstants:
    def test_zero_arg_immutable_entries_are_values(self):
        # `math.pi()` would read badly beside Python's own `math.pi`.
        text, params = render_expr(math.pi)
        assert text == 'math::pi()'
        assert params == {}

    def test_constants_are_also_callable(self):
        # Which spelling reads better varies by name (`math.pi` vs
        # `sys.get_version`), so both are accepted rather than guessed at.
        assert render_expr(math.pi())[0] == 'math::pi()'
        assert render_expr(math.pi)[0] == render_expr(math.pi())[0]

    def test_stable_zero_arg_entries_are_not_constants(self):
        # `datetime_of_statement` is fixed within a statement but differs
        # between them, so it must not present as a settled value.
        assert not stdlib.is_constant('std', 'datetime_of_statement')
        assert not stdlib.is_constant('std', 'datetime_of_transaction')
        assert stdlib.overloads('std', 'datetime_of_statement')[0]['volatility'] == 'stable'
        assert callable(std.datetime_of_statement)

    def test_volatile_zero_arg_entries_stay_callable(self):
        # `std.random` as a bare value would hide that it re-evaluates.
        assert callable(std.random)
        assert render_expr(std.random())[0] == 'std::random()'

    def test_is_constant_classification(self):
        assert stdlib.is_constant('math', 'pi')
        assert stdlib.is_constant('math', 'e')
        assert not stdlib.is_constant('std', 'random')
        assert not stdlib.is_constant('std', 'str_lower')


class TestUnknownNames:
    def test_unknown_name_raises_attribute_error(self):
        # AttributeError, not InterfaceError, so hasattr/getattr-with-default
        # behave the way they do on any other Python object.
        with pytest.raises(AttributeError):
            _ = std.definitely_not_a_function

    def test_hasattr_is_false_for_unknown_names(self):
        assert not hasattr(std, 'definitely_not_a_function')
        assert hasattr(std, 'str_lower')

    def test_typo_gets_a_suggestion(self):
        with pytest.raises(AttributeError, match=r'did you mean std\.str_lower'):
            _ = std.strlower

    def test_typo_against_an_infix_alias_gets_a_suggestion(self):
        with pytest.raises(AttributeError, match=r'did you mean std\.ilike'):
            _ = std.iilike

    @pytest.mark.parametrize('name', ['round'])
    def test_math_functions_that_live_in_std_point_there(self, name):
        # These deliberately live in std, but Python's own math module is
        # where a developer will look first.
        with pytest.raises(AttributeError, match=rf'it lives in std, use std\.{name}'):
            getattr(math, name)

    @pytest.mark.parametrize('name', ['sqrt', 'abs', 'ceil', 'floor'])
    def test_math_functions_gel_declares_in_math_resolve_there(self, name):
        # EdgeQL spells these `math::ceil` and so does every ported query;
        # answering "it lives in std" made a valid query fail to compile.
        assert getattr(math, name) is not None

    def test_std_pi_points_at_math(self):
        with pytest.raises(AttributeError, match=r'it lives in math, use math\.pi'):
            _ = std.pi


class TestArity:
    def test_too_few_arguments(self):
        with pytest.raises(InterfaceError, match=r'takes 1 argument\(s\), got 0'):
            std.str_lower()

    def test_too_many_arguments(self):
        with pytest.raises(InterfaceError, match=r'takes 1 argument\(s\), got 2'):
            std.str_lower(_path('a'), _path('b'))

    def test_multiple_arities_are_listed(self):
        # std::str_trim has 1- and 2-argument overloads.
        with pytest.raises(InterfaceError, match=r'takes 1 or 2 argument\(s\), got 5'):
            std.str_trim(*[_path('a')] * 5)

    def test_infix_alias_arity_is_enforced(self):
        with pytest.raises(TypeError, match='exactly 2 arguments'):
            std.ilike(_path('name'))


class TestContextGate:
    def test_aggregate_rejected_as_a_default(self):
        with pytest.raises(InterfaceError, match='is an aggregate'):
            stdlib.check_context('std', 'count', 1, stdlib.CONTEXT_DEFAULT)

    def test_set_returning_rejected_as_a_default(self):
        with pytest.raises(InterfaceError, match='returns a set'):
            stdlib.check_context('std', 'array_unpack', 1, stdlib.CONTEXT_DEFAULT)

    def test_volatile_allowed_as_a_default(self):
        # The whole point of std in a Default(...).
        stdlib.check_context('std', 'uuid_generate_v7', 0, stdlib.CONTEXT_DEFAULT)
        stdlib.check_context('std', 'datetime_current', 0, stdlib.CONTEXT_DEFAULT)

    def test_modifying_rejected_in_an_expression(self):
        with pytest.raises(InterfaceError, match='modifies database state'):
            stdlib.check_context('std', 'sequence_reset', 1, stdlib.CONTEXT_EXPRESSION)

    def test_volatile_allowed_in_an_expression(self):
        # `filter .expires_at > std.datetime_current()` is volatile and
        # entirely legitimate, so volatility alone must not disqualify.
        stdlib.check_context('std', 'datetime_current', 0, stdlib.CONTEXT_EXPRESSION)
        stdlib.check_context('std', 'random', 0, stdlib.CONTEXT_EXPRESSION)

    def test_aggregate_allowed_in_an_expression(self):
        stdlib.check_context('std', 'count', 1, stdlib.CONTEXT_EXPRESSION)
