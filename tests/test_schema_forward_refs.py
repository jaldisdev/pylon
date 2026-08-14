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

"""What happens when a link annotation names a type that isn't defined yet.

Two failure modes lived here, both silent:

* An annotation that could not be resolved fell back to its raw *string*,
  which then matched none of the Link/MultiLink branches and came out the far
  end as an ordinary ``text`` property. A link became a text column, a
  migration created it, and nothing anywhere reported a problem. The only
  visible symptom was a much later, unrelated-looking query error
  ("sub-statement used as expression is only valid as the subject of a SELECT
  result") when something tried to assign an object to it.

* ``pylon.lazy``, the documented way out of a circular reference, never
  resolved anything: ``Annotated['Author', lazy(...)]`` stores its first
  argument as a ``ForwardRef``, not a ``str``, and the walker only matched
  ``str``. Every lazy ref fell through to "unresolvable Annotated type".

These tests build real modules on disk, because both bugs depend on module
identity and on annotations being resolved against a real module namespace —
a class defined inside a test function cannot reproduce either.
"""

from __future__ import annotations

import json
import sys
import textwrap

import pytest

from pylon.schema._registry import clear as clear_registry
from pylon.schema._registry import snapshot
from pylon.schema._walker import SchemaError, walk


_written: list[tuple[str, str]] = []


@pytest.fixture(autouse=True)
def _clean_registry():
    clear_registry()
    yield
    clear_registry()
    # Torn down *after* the test, not inside `_write_module`: `pylon.lazy`
    # resolves by importing the module at `walk()` time, so it has to stay
    # importable for as long as the test runs.
    for name, directory in _written:
        sys.modules.pop(name, None)
        if directory in sys.path:
            sys.path.remove(directory)
    _written.clear()


def _write_module(tmp_path, name: str, source: str):
    """Write `source` to an importable module and import it fresh."""
    path = tmp_path / f'{name}.py'
    path.write_text(textwrap.dedent(source))
    directory = str(tmp_path)
    if directory not in sys.path:
        sys.path.insert(0, directory)
    sys.modules.pop(name, None)
    _written.append((name, directory))
    return __import__(name)


def _type_json(schema, name: str):
    return next(t for t in json.loads(schema.to_json())['types'] if t['name'] == name)


class TestUnresolvedAnnotation:
    def test_a_forward_link_reference_is_an_error_not_a_text_column(self, tmp_path):
        """The regression that matters: this used to produce
        ``author text`` and ``tags text`` and report nothing."""
        with pytest.raises(SchemaError) as exc:
            _write_module(
                tmp_path,
                'fwd_plain',
                """
                from __future__ import annotations

                import pylon.schema as ps
                from pylon.schema import Link, MultiLink

                @ps.type(module='m', name='Post')
                class Post:
                    title: str
                    author: Link[Author] | None
                    tags: MultiLink[Tag]

                @ps.type(module='m', name='Author')
                class Author:
                    name: str

                @ps.type(module='m', name='Tag')
                class Tag:
                    label: str
                """,
            )

        message = str(exc.value)
        # Names the class, both offending pointers, and the way out.
        assert "Type 'Post'" in message
        assert 'author' in message and 'tags' in message
        assert 'pylon.lazy' in message

    def test_a_resolvable_reference_still_builds_a_link(self, tmp_path):
        """The ordinary case — referenced type declared first — is unaffected."""
        mod = _write_module(
            tmp_path,
            'fwd_ordered',
            """
            from __future__ import annotations

            import pylon.schema as ps
            from pylon.schema import Link, MultiLink

            @ps.type(module='m', name='Author')
            class Author:
                name: str

            @ps.type(module='m', name='Tag')
            class Tag:
                label: str

            @ps.type(module='m', name='Post')
            class Post:
                title: str
                author: Link[Author] | None
                tags: MultiLink[Tag]
            """,
        )
        assert mod is not None
        post = _type_json(walk(*snapshot(), []), 'Post')
        assert [(link['name'], link['target']) for link in post['links']] == [('author', 'm::Author')]
        assert [(link['name'], link['target']) for link in post['multilinks']] == [('tags', 'm::Tag')]
        assert [p['name'] for p in post['properties']] == ['id', 'title']

    def test_types_declared_inside_a_function_can_reference_each_other(self):
        """A widespread pattern in this suite, and one `get_type_hints` cannot
        resolve on its own: the sibling classes are function locals, visible
        in neither module globals nor class vars. These silently became text
        columns — including in `test_migrations_live.py`, which never looked
        at the column and so never noticed."""
        import pylon.schema as pylon
        from pylon.schema import Link, MultiLink

        @pylon.type(module='m', name='Tag')
        class Tag:
            label: str

        @pylon.type(module='m', name='Author')
        class Author:
            name: str

        @pylon.type(module='m', name='Widget')
        class Widget:
            name: str
            author: Link[Author] | None
            tags: MultiLink[Tag]

        widget = _type_json(walk(*snapshot(), []), 'Widget')
        assert [(link['name'], link['target']) for link in widget['links']] == [('author', 'm::Author')]
        assert [(link['name'], link['target']) for link in widget['multilinks']] == [('tags', 'm::Tag')]
        assert [p['name'] for p in widget['properties']] == ['id', 'name']

    def test_a_typo_in_a_scalar_annotation_is_also_reported(self, tmp_path):
        with pytest.raises(SchemaError, match='nosuchtype'):
            _write_module(
                tmp_path,
                'fwd_typo',
                """
                from __future__ import annotations

                import pylon.schema as ps

                @ps.type(module='m', name='Thing')
                class Thing:
                    name: nosuchtype
                """,
            )


class TestLazyResolution:
    """`pylon.lazy` has to work under `from __future__ import annotations` —
    that is the mode its own docstring demonstrates, and the mode in which a
    circular reference actually arises."""

    LAZY_SOURCE = """
        from __future__ import annotations

        from typing import Annotated

        import pylon.schema as ps
        from pylon.schema import Link, MultiLink, lazy

        @ps.type(module='m', name='Post')
        class Post:
            title: str
            author: Link[Annotated['Author', lazy('{name}')]] | None
            tags: MultiLink[Annotated['Tag', lazy('{name}')]]

        @ps.type(module='m', name='Author')
        class Author:
            name: str

        @ps.type(module='m', name='Tag')
        class Tag:
            label: str
    """

    def test_a_lazy_forward_reference_resolves(self, tmp_path):
        _write_module(tmp_path, 'lazy_pep563', self.LAZY_SOURCE.format(name='lazy_pep563'))
        post = _type_json(walk(*snapshot(), []), 'Post')
        assert [(link['name'], link['target']) for link in post['links']] == [('author', 'm::Author')]
        assert [(link['name'], link['target']) for link in post['multilinks']] == [('tags', 'm::Tag')]
        # Critically: not a text column.
        assert [p['name'] for p in post['properties']] == ['id', 'title']

    def test_a_lazy_reference_without_pep_563_resolves_too(self, tmp_path):
        source = self.LAZY_SOURCE.format(name='lazy_plain').replace('from __future__ import annotations\n', '')
        _write_module(tmp_path, 'lazy_plain', source)
        post = _type_json(walk(*snapshot(), []), 'Post')
        assert [(link['name'], link['target']) for link in post['links']] == [('author', 'm::Author')]

    def test_a_lazy_reference_to_a_missing_type_still_errors(self, tmp_path):
        _write_module(
            tmp_path,
            'lazy_missing',
            """
            from __future__ import annotations

            from typing import Annotated

            import pylon.schema as ps
            from pylon.schema import Link, lazy

            @ps.type(module='m', name='Post')
            class Post:
                title: str
                author: Link[Annotated['Nowhere', lazy('lazy_missing')]] | None
            """,
        )
        with pytest.raises(SchemaError, match='did not resolve to a Pylon type'):
            walk(*snapshot(), [])
