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

"""What may appear as a *value* in a query expression, and how a nested
query is bound.

NOTE: this file intentionally omits ``from __future__ import annotations`` —
the schema DSL reads pointer metadata out of evaluated class-body
annotations.
"""

import uuid

import pytest
from pylon._core import compile as pycompile

import pylon
from pylon import std
from pylon.exceptions import InterfaceError
from pylon.modelquery import mark_saved, render
from pylon.schema import Link, Property
from pylon.schema._registry import clear as clear_registry
from pylon.schema._registry import snapshot
from pylon.schema._walker import walk


@pytest.fixture
def models():
    clear_registry()

    @pylon.type(module='default', name='Company')
    class Company:
        name: Property[pylon.Str]

    @pylon.type(module='default', name='Person')
    class Person:
        name: Property[pylon.Str]
        company: Link[Company] | None

    schema = walk(*snapshot(), [])
    return Company, Person, schema


def _saved(cls, **kwargs):
    obj = cls(**kwargs)
    obj.id = uuid.uuid4()
    mark_saved(obj)
    return obj


class TestNestedQueries:
    def test_a_model_set_becomes_a_with_binding(self, models):
        # PyQL rejects a bare sub-statement in expression position
        # ("only valid as the subject of a SELECT result"), so a nested query
        # *must* be hoisted — unlike elsewhere, this isn't a choice.
        Company, Person, _schema = models
        acmes = Company.filter(name='Acme')
        text, _params = render(Person.filter(lambda p: std.in_(p.company, acmes)))
        assert text.startswith('with __mq_q0 := (select default::Company')
        assert 'filter .company in __mq_q0' in text

    def test_the_binding_compiles(self, models):
        Company, Person, schema = models
        acmes = Company.filter(name='Acme')
        text, _params = render(Person.filter(lambda p: std.in_(p.company, acmes)))
        pycompile(text, schema)

    def test_the_same_set_twice_is_one_binding(self, models):
        Company, Person, _schema = models
        acmes = Company.filter(name='Acme')
        text, _params = render(Person.filter(lambda p: std.in_(p.company, acmes) & std.not_in(p.company, acmes)))
        assert text.count('__mq_q0 := ') == 1
        assert text.count('__mq_q0') == 3

    def test_separate_sets_get_separate_bindings(self, models):
        Company, Person, _schema = models
        first = Company.filter(name='A')
        second = Company.filter(name='B')
        text, _params = render(Person.filter(lambda p: std.in_(p.company, first) & std.not_in(p.company, second)))
        assert '__mq_q0 := ' in text
        assert '__mq_q1 := ' in text

    def test_a_nested_query_carries_its_own_parameters(self, models):
        Company, Person, _schema = models
        acmes = Company.filter(name='Acme')
        _text, params = render(Person.filter(lambda p: std.in_(p.company, acmes)))
        assert 'Acme' in params.values()

    def test_delete_also_binds(self, models):
        Company, Person, _schema = models
        acmes = Company.filter(name='Acme')
        text, _params = render(Person.filter(lambda p: std.in_(p.company, acmes)).delete())
        assert text.startswith('with __mq_q0 := ')
        assert ' delete default::Person filter ' in text

    def test_a_query_with_no_nesting_has_no_prefix(self, models):
        _Company, Person, _schema = models
        text, _params = render(Person.filter(name='Bob'))
        assert not text.startswith('with ')


class TestModelInstancesAsValues:
    def test_a_saved_instance_compares_by_id(self, models):
        Company, Person, _schema = models
        acme = _saved(Company, name='Acme')
        text, params = render(Person.filter(lambda p: p.company == acme))
        assert '.company = $' in text
        assert acme.id in params.values()

    def test_it_compiles(self, models):
        Company, Person, schema = models
        acme = _saved(Company, name='Acme')
        text, _params = render(Person.filter(lambda p: p.company == acme))
        pycompile(text, schema)

    def test_an_unsaved_instance_is_refused(self, models):
        # It has no id, so there is nothing to match against. Binding it as a
        # parameter would compile and then behave unpredictably.
        Company, Person, _schema = models
        with pytest.raises(InterfaceError, match='unsaved Company instance'):
            render(Person.filter(lambda p: p.company == Company(name='Acme')))


class TestRejectedValues:
    """Previously these were silently wrapped as bound parameters: the query
    compiled, then did the wrong thing at execution."""

    def test_a_model_class_is_refused(self, models):
        Company, Person, _schema = models
        with pytest.raises(InterfaceError, match=r'did you mean Company\.filter'):
            render(Person.filter(lambda p: p.company == Company))

    def test_a_namespace_is_refused(self, models):
        _Company, Person, _schema = models
        with pytest.raises(InterfaceError, match='call a function on it'):
            render(Person.filter(lambda p: p.name == std))

    @pytest.mark.parametrize(
        'value',
        ['text', 42, 3.5, True, None, uuid.uuid4(), b'bytes', ['a', 'b']],
        ids=['str', 'int', 'float', 'bool', 'none', 'uuid', 'bytes', 'list'],
    )
    def test_ordinary_values_still_pass_through(self, value, models):
        # The check has to reject query constructs without narrowing what
        # counts as a legitimate parameter value.
        _Company, Person, _schema = models
        _text, params = render(Person.filter(lambda p: p.name == value))
        assert list(params.values()) == [value]
