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

"""Tests for the REPL/CLI result formatter in pylon.cli.commands.query —
specifically that internal `__pylon_*` bookkeeping attributes (the
`__pylon_saved__` diffing shadow `Client.save()` uses, in particular) never
leak into printed output for nested dataclass values.
"""

import re

from pylon.cli.commands.query import _format_results, _pformat_value, _selected, _value
from pylon.datatypes import LinkSet, PylonSet

_ANSI_RE = re.compile(r'\x1b\[[0-9;]*m')


def _strip_ansi(s: str) -> str:
    return _ANSI_RE.sub('', s)


def _make_nested_company(company_cls):
    obj = object.__new__(company_cls)
    obj.__dict__.update(
        {
            '__pylon_type__': 'default::Company',
            'id': '019f5af3-6dd9-7f1f-82d4-1846d7fa964d',
            '__pylon_saved__': {'id': '019f5af3-6dd9-7f1f-82d4-1846d7fa964d'},
        }
    )
    return obj


class TestNestedDataclassFormatting:
    def test_pformat_value_excludes_pylon_saved(self):
        import dataclasses

        @dataclasses.dataclass
        class Company:
            pass

        obj = _make_nested_company(Company)
        out = _strip_ansi(_pformat_value(obj, depth=1, max_width=100))
        assert '__pylon_saved__' not in out
        assert '__pylon_type__' not in out
        assert 'id:' in out

    def test_value_excludes_pylon_saved(self):
        import dataclasses

        @dataclasses.dataclass
        class Company:
            pass

        obj = _make_nested_company(Company)
        out = _strip_ansi(_value(obj))
        assert '__pylon_saved__' not in out
        assert '__pylon_type__' not in out
        assert 'id:' in out


def _make_object(cls, type_name: str, **pointers):
    obj = object.__new__(cls)
    obj.__dict__.update({'__pylon_type__': type_name, **pointers})
    return obj


def _object_node(type_name: str, *names: str, **children: dict) -> dict:
    pointers = [{'kind': 'scalar', 'name': '__type__', 'position': 0}]
    pointers += [{'kind': 'scalar', 'name': name, 'position': i + 1} for i, name in enumerate(names)]
    pointers += [{**child, 'name': name} for name, child in children.items()]
    return {'kind': 'object', 'name': '', 'type_name': type_name, 'pointers': pointers}


class TestSelectedPointers:
    """A linked object carries every pointer of its type, including the
    multi-links nobody fetched — which refuse to be read. Only what the shape
    selected is shown, at every level, as `select Assessment { ** }` needs."""

    def _assessment(self):
        import dataclasses

        @dataclasses.dataclass
        class Brand:
            pass

        @dataclasses.dataclass
        class Answer:
            pass

        @dataclasses.dataclass
        class Assessment:
            pass

        brand = _make_object(Brand, 'brand::Brand', name='JALDIS', access_grants=LinkSet(unhydrated=True))
        answer = _make_object(Answer, 'brand::Answer', id='a1', value='x', options=LinkSet(unhydrated=True))
        answers = PylonSet([answer])
        assessment = _make_object(Assessment, 'brand::Assessment', id='s1', brand=brand, answers=answers)
        node = _object_node(
            'brand::Assessment',
            'id',
            brand=_object_node('brand::Brand', 'name'),
            answers={'kind': 'array', 'element': _object_node('brand::Answer', 'id')},
        )
        return assessment, node

    def test_an_unfetched_nested_multi_link_is_left_out(self):
        assessment, node = self._assessment()
        out = _strip_ansi(_format_results([_selected(assessment, node)]))
        assert 'access_grants' not in out
        assert "brand::Brand {name: 'JALDIS'}" in out

    def test_a_set_of_objects_shows_what_its_element_selected(self):
        assessment, node = self._assessment()
        out = _strip_ansi(_format_results([_selected(assessment, node)]))
        assert 'brand::Answer {id:' in out
        assert 'value' not in out
        assert 'options' not in out

    def test_a_nested_set_is_indented_to_its_depth(self):
        answers = PylonSet([{'__display_type__': 'brand::Answer', 'id': str(i) * 30} for i in range(3)])
        out = _strip_ansi(_pformat_value(answers, depth=2, max_width=60))
        lines = out.splitlines()
        assert lines[1].startswith('      brand::Answer')
        assert lines[-1] == '    }'
