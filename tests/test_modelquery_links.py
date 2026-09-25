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

"""Links in the model API: single-link assignment, the LinkSet op log, and
`+=` / `-=` on multi-links.

NOTE: this file intentionally omits ``from __future__ import annotations`` —
the schema DSL reads pointer metadata out of evaluated class-body
annotations.
"""

import uuid

import pytest

import pylon
from pylon.datatypes import LinkSet
from pylon.exceptions import InterfaceError
from pylon.modelquery import mark_saved, prepare_save, save_order
from pylon.schema import Link, MultiLink, Property
from pylon.schema._registry import clear as clear_registry
from pylon.schema._registry import snapshot
from pylon.schema._walker import walk


@pytest.fixture
def models():
    """A Post -> Author link and a Post -> Tag multi-link, walked so
    `link_target` is resolved the way it is in a real application."""
    clear_registry()

    @pylon.type(module='default', name='Tag')
    class Tag:
        name: Property[pylon.Str]

    @pylon.type(module='default', name='Author')
    class Author:
        name: Property[pylon.Str]

    @pylon.type(module='default', name='Post')
    class Post:
        title: Property[pylon.Str]
        # Optional so "link never set" and "link cleared" are both reachable.
        author: Link[Author] | None
        tags: MultiLink[Tag]

    schema = walk(*snapshot(), [])
    return Tag, Author, Post, schema


@pytest.fixture
def junction_models():
    """Junction-backed links, which behave differently from plain ones:
    they have no FK column, and their junction can carry link properties."""
    clear_registry()

    @pylon.type(module='default', name='Company')
    class Company:
        name: Property[pylon.Str]

    @pylon.type(module='default', name='Tag')
    class Tag:
        name: Property[pylon.Str]

    @pylon.junction
    class Employment:
        # Optional, so the link itself is still saveable.
        since: Property[pylon.Int64] | None

    @pylon.junction
    class Weighted:
        # Required, so it can't be supplied by `+=`.
        weight: Property[pylon.Float64]

    @pylon.type(module='default', name='Person')
    class Person:
        name: Property[pylon.Str]
        employer: Link[Company, pylon.Through[Employment]] | None
        labels: MultiLink[Tag, pylon.Through[Weighted]]

    walk(*snapshot(), [])
    return Company, Tag, Person


def _saved(cls, **kwargs):
    """An instance standing in for one loaded from the database."""
    obj = cls(**kwargs)
    obj.id = uuid.uuid4()
    mark_saved(obj)
    return obj


# ── LinkSet ──────────────────────────────────────────────────────────────────


class TestLinkSet:
    def test_is_a_list(self):
        # Multi-links were plain lists before; nothing should notice.
        ls = LinkSet(['a', 'b'])
        assert isinstance(ls, list)
        assert ls == ['a', 'b']
        assert len(ls) == 2
        assert list(ls) == ['a', 'b']

    def test_iadd_records_and_applies(self):
        ls = LinkSet(['a'])
        ls += ['b']
        assert ls == ['a', 'b']
        assert ls.ops == [('add', ['b'], {})]

    def test_isub_records_and_applies(self):
        ls = LinkSet(['a', 'b'])
        ls -= ['a']
        assert ls == ['b']
        assert ls.ops == [('remove', ['a'], {})]

    def test_isub_of_a_missing_member_is_a_no_op(self):
        # Mirrors the server: unlinking something that isn't linked does
        # nothing rather than failing.
        ls = LinkSet(['a'])
        ls -= ['zz']
        assert ls == ['a']
        assert ls.ops == [('remove', ['zz'], {})]

    def test_iadd_returns_self(self):
        # `x.tags += [y]` reassigns the attribute; returning self keeps that
        # a no-op instead of replacing the tracked object.
        ls = LinkSet()
        before = id(ls)
        ls += ['a']
        assert id(ls) == before

    def test_a_single_instance_is_not_iterated(self):
        ls = LinkSet()
        ls += 'one-value'
        assert ls.ops == [('add', ['one-value'], {})]

    def test_clear_ops(self):
        ls = LinkSet(['a'])
        ls += ['b']
        ls.clear_ops()
        assert ls.ops == []
        assert ls == ['a', 'b']


class TestUnhydratedLinkSet:
    def test_mutations_are_allowed(self):
        # `+=` is applied server-side, so it needs no knowledge of the
        # current members.
        ls = LinkSet(unhydrated=True)
        ls += ['a']
        ls -= ['b']
        assert ls.ops == [('add', ['a'], {}), ('remove', ['b'], {})]

    @pytest.mark.parametrize(
        'read',
        [len, list, bool, lambda ls: ls[0], lambda ls: 'a' in ls, lambda ls: ls == []],
        ids=['len', 'iter', 'bool', 'index', 'contains', 'eq'],
    )
    def test_reads_are_refused(self, read):
        # Showing an empty list would be a lie — the members exist, they
        # just weren't fetched.
        ls = LinkSet(unhydrated=True)
        with pytest.raises(AttributeError, match='was not fetched'):
            read(ls)

    def test_repr_says_so_rather_than_raising(self):
        # repr has to stay safe: a debugger or logger calling it shouldn't
        # blow up.
        assert repr(LinkSet(unhydrated=True)) == '<LinkSet not fetched>'

    def test_is_hydrated_flag(self):
        assert LinkSet().is_hydrated
        assert not LinkSet(unhydrated=True).is_hydrated

    def test_the_refusal_names_the_pointer_and_its_owner(self, models):
        # The message is the only thing the caller sees, and it surfaces deep
        # inside whatever iterated the link, so it has to say which one.
        _Tag, _Author, Post, _schema = models
        pointer = Post.__pylon_config__.pointers['tags']
        unfetched = LinkSet(unhydrated=True, pointer=pointer, owner='Post')
        with pytest.raises(AttributeError, match=r'cannot iterate Post\.tags — it was not fetched'):
            list(unfetched)

    def test_the_refusal_falls_back_when_there_is_nothing_to_name(self):
        with pytest.raises(AttributeError, match='cannot iterate a multi-link — it was not fetched'):
            list(LinkSet(unhydrated=True))


# ── Fresh instances ──────────────────────────────────────────────────────────


class TestNewInstances:
    def test_multilink_defaults_to_an_empty_linkset(self, models):
        _Tag, _Author, Post, _schema = models
        post = Post(title='x')
        assert isinstance(post.tags, LinkSet)
        assert post.tags == []

    def test_insert_includes_a_single_link(self, models):
        _Tag, Author, Post, _schema = models
        author = _saved(Author, name='Bob')
        pyql, params = prepare_save(Post(title='Hi', author=author))
        assert 'author := <uuid>$' in pyql
        assert author.id in params.values()

    def test_insert_includes_multilink_members(self, models):
        Tag, _Author, Post, _schema = models
        tags = [_saved(Tag, name='a'), _saved(Tag, name='b')]
        post = Post(title='Hi')
        post.tags += tags
        pyql, params = prepare_save(post)
        assert 'tags := (select default::Tag filter .id in std::array_unpack(' in pyql
        assert [t.id for t in tags] in params.values()

    def test_insert_omits_an_unset_link(self, models):
        _Tag, _Author, Post, _schema = models
        pyql, _params = prepare_save(Post(title='Hi'))
        assert 'author' not in pyql

    def test_insert_omits_an_empty_multilink(self, models):
        _Tag, _Author, Post, _schema = models
        pyql, _params = prepare_save(Post(title='Hi'))
        assert 'tags' not in pyql

    def test_rendering_an_unsaved_target_directly_is_rejected(self, models):
        # `client.save()` writes link targets first, so this is only
        # reachable by calling prepare_save() by hand.
        _Tag, Author, Post, _schema = models
        with pytest.raises(InterfaceError, match='unsaved Author instance'):
            prepare_save(Post(title='Hi', author=Author(name='Bob')))


class TestSaveOrder:
    """`client.save()` writes unsaved link targets before the objects that
    reference them, so an object graph can be constructed and saved as one."""

    def test_a_link_target_is_ordered_first(self, models):
        _Tag, Author, Post, _schema = models
        author = Author(name='Bob')
        post = Post(title='Hi', author=author)
        assert save_order([post]) == [author, post]

    def test_multilink_members_are_ordered_first(self, models):
        Tag, _Author, Post, _schema = models
        tag = Tag(name='t')
        post = Post(title='Hi')
        post.tags += [tag]
        assert save_order([post]) == [tag, post]

    def test_an_already_saved_target_is_not_re_added(self, models):
        _Tag, Author, Post, _schema = models
        author = _saved(Author, name='Bob')
        post = Post(title='Hi', author=author)
        assert save_order([post]) == [post]

    def test_a_shared_target_is_written_once(self, models):
        # Identity, not equality — the same object referenced twice is one
        # row, which is also what the CTE hoisting rule would have given.
        _Tag, Author, Post, _schema = models
        author = Author(name='Bob')
        first = Post(title='A', author=author)
        second = Post(title='B', author=author)
        order = save_order([first, second])
        assert order.count(author) == 1
        assert order.index(author) == 0

    def test_explicit_order_is_preserved(self, models):
        _Tag, Author, Post, _schema = models
        author = _saved(Author, name='Bob')
        post = _saved(Post, title='Hi')
        assert save_order([post, author]) == [post, author]

    def test_a_deep_chain_is_ordered(self, models):
        # Post -> Author, where the Author itself is unsaved.
        _Tag, Author, Post, _schema = models
        author = Author(name='Bob')
        post = Post(title='Hi', author=author)
        order = save_order([post])
        assert order.index(author) < order.index(post)

    def test_a_cycle_is_reported(self, models):
        # Two unsaved objects pointing at each other can't be ordered; one
        # has to exist before the other can reference it.
        _Tag, _Author, Post, _schema = models
        a = Post(title='A')
        b = Post(title='B')
        a.__dict__['author'] = b
        b.__dict__['author'] = a
        with pytest.raises(InterfaceError, match='circular reference'):
            save_order([a])


# ── Existing instances ───────────────────────────────────────────────────────


class TestUpdates:
    def test_changed_link_is_written(self, models):
        _Tag, Author, Post, _schema = models
        first, second = _saved(Author, name='A'), _saved(Author, name='B')
        post = _saved(Post, title='Hi', author=first)
        post.author = second
        pyql, params = prepare_save(post)
        assert 'author := <uuid>$' in pyql
        assert second.id in params.values()

    def test_unchanged_link_is_not_written(self, models):
        _Tag, Author, Post, _schema = models
        author = _saved(Author, name='A')
        post = _saved(Post, title='Hi', author=author)
        assert prepare_save(post) is None

    def test_an_equal_but_distinct_instance_is_not_a_change(self, models):
        # Compared by target id, not object identity, so re-assigning the
        # same row loaded twice isn't a spurious update.
        _Tag, Author, Post, _schema = models
        author = _saved(Author, name='A')
        post = _saved(Post, title='Hi', author=author)

        same_row = Author(name='A')
        same_row.id = author.id
        post.author = same_row
        assert prepare_save(post) is None

    def test_clearing_a_link_uses_the_empty_set(self, models):
        # Pylon has no null; `{}` is how a link is cleared.
        _Tag, Author, Post, _schema = models
        post = _saved(Post, title='Hi', author=_saved(Author, name='A'))
        post.author = None
        pyql, _params = prepare_save(post)
        assert 'author := {}' in pyql

    def test_append_renders_as_plus_equals(self, models):
        Tag, _Author, Post, _schema = models
        post = _saved(Post, title='Hi')
        post.tags += [_saved(Tag, name='a')]
        pyql, _params = prepare_save(post)
        assert 'tags += (select default::Tag' in pyql

    def test_remove_renders_as_minus_equals(self, models):
        Tag, _Author, Post, _schema = models
        post = _saved(Post, title='Hi')
        post.tags -= [_saved(Tag, name='a')]
        pyql, _params = prepare_save(post)
        assert 'tags -= (select default::Tag' in pyql

    def test_add_and_remove_keep_their_order(self, models):
        # `+= [a]` then `-= [a]` is not the same as the reverse.
        Tag, _Author, Post, _schema = models
        post = _saved(Post, title='Hi')
        post.tags += [_saved(Tag, name='a')]
        post.tags -= [_saved(Tag, name='b')]
        pyql, _params = prepare_save(post)
        assert pyql.index('tags +=') < pyql.index('tags -=')

    def test_consecutive_same_ops_are_coalesced(self, models):
        Tag, _Author, Post, _schema = models
        post = _saved(Post, title='Hi')
        post.tags += [_saved(Tag, name='a')]
        post.tags += [_saved(Tag, name='b')]
        pyql, params = prepare_save(post)
        assert pyql.count('tags +=') == 1
        # Both members land in the one array parameter.
        assert any(isinstance(v, list) and len(v) == 2 for v in params.values())

    def test_assigning_a_plain_list_replaces_the_set(self, models):
        # A bare list can only mean "these are the members now".
        Tag, _Author, Post, _schema = models
        post = _saved(Post, title='Hi')
        post.tags = [_saved(Tag, name='a')]
        pyql, _params = prepare_save(post)
        assert 'tags := (select default::Tag' in pyql

    def test_a_multilink_with_no_ops_is_not_written(self, models):
        _Tag, _Author, Post, _schema = models
        post = _saved(Post, title='Hi')
        assert prepare_save(post) is None

    def test_unhydrated_multilink_can_be_appended_to(self, models):
        # The whole point of the unhydrated mode: mutate without fetching.
        Tag, _Author, Post, _schema = models
        post = _saved(Post, title='Hi')
        post.__dict__['tags'] = LinkSet(unhydrated=True)
        post.tags += [_saved(Tag, name='a')]
        pyql, _params = prepare_save(post)
        assert 'tags += (select default::Tag' in pyql


class TestJunctionBackedLinks:
    """Verified against a live database — these differ from plain links in
    ways that only surface at execution time."""

    def test_junction_single_link_uses_a_subquery(self, junction_models):
        # A `Through[...]` link has no FK column, so the compiler rejects a
        # bare uuid for it: "multilink value must be a CTE reference,
        # parenthesised subquery, or type path".
        Company, _Tag, Person = junction_models
        company = _saved(Company, name='Acme')
        person = _saved(Person, name='Bob')
        person.employer = company
        pyql, _params = prepare_save(person)
        assert 'employer := (select default::Company filter .id = <uuid>$' in pyql
        assert 'employer := <uuid>$' not in pyql

    def test_plain_link_still_uses_a_bare_uuid(self, models):
        # The bare form is deliberate for plain links: the subquery form
        # yields NULL when no row matches, silently clearing the link.
        _Tag, Author, Post, _schema = models
        post = _saved(Post, title='Hi')
        post.author = _saved(Author, name='A')
        pyql, _params = prepare_save(post)
        assert 'author := <uuid>$' in pyql

    def test_bare_append_on_a_required_prop_junction_points_at_add(self, junction_models):
        # `+=` supplies a target and nothing else. Without this check the only
        # signal is a raw Postgres NOT NULL violation.
        _Company, Tag, Person = junction_models
        person = _saved(Person, name='Bob')
        person.labels += [_saved(Tag, name='t')]
        with pytest.raises(InterfaceError, match=r'Use `\.add\(target, weight=\.\.\.\)`'):
            prepare_save(person)


class TestLinkProperties:
    """`LinkSet.add(target, **props)` — the one case `+=` can't express."""

    def test_add_emits_the_link_property(self, junction_models):
        _Company, Tag, Person = junction_models
        person = _saved(Person, name='Bob')
        person.labels.add(_saved(Tag, name='t'), weight=1.5)
        pyql, params = prepare_save(person)
        assert '{ @weight := <float64>$' in pyql
        assert 1.5 in params.values()

    def test_the_parameter_is_cast_to_the_declared_type(self, junction_models):
        # An uncast parameter binds as text, and PostgreSQL rejects it:
        # `column "weight" is of type double precision but expression is of
        # type text`.
        _Company, Tag, Person = junction_models
        person = _saved(Person, name='Bob')
        person.labels.add(_saved(Tag, name='t'), weight=1.0)
        pyql, _params = prepare_save(person)
        assert '@weight := <float64>$' in pyql

    def test_different_values_need_separate_clauses(self, junction_models):
        # `@prop` attaches to a whole `+=` clause, so two weights can't share.
        _Company, Tag, Person = junction_models
        person = _saved(Person, name='Bob')
        person.labels.add(_saved(Tag, name='a'), weight=1.0)
        person.labels.add(_saved(Tag, name='b'), weight=2.0)
        pyql, _params = prepare_save(person)
        assert pyql.count('labels +=') == 2

    def test_equal_values_coalesce(self, junction_models):
        _Company, Tag, Person = junction_models
        person = _saved(Person, name='Bob')
        person.labels.add(_saved(Tag, name='a'), weight=1.0)
        person.labels.add(_saved(Tag, name='b'), weight=1.0)
        pyql, _params = prepare_save(person)
        assert pyql.count('labels +=') == 1

    def test_add_does_not_coalesce_with_a_bare_append(self, junction_models):
        _Company, Tag, Person = junction_models
        person = _saved(Person, name='Bob')
        person.labels.add(_saved(Tag, name='a'), weight=1.0)
        person.labels.add(_saved(Tag, name='b'), weight=1.0)
        pyql, _params = prepare_save(person)
        # Both carry properties, so one clause; a propertyless append would
        # be a second one.
        assert pyql.count('labels') == 1

    def test_an_unknown_property_is_caught_at_the_call(self, junction_models):
        _Company, Tag, Person = junction_models
        person = _saved(Person, name='Bob')
        with pytest.raises(InterfaceError, match='did you mean weight'):
            person.labels.add(_saved(Tag, name='t'), weigth=1.0)

    def test_props_on_a_plain_multilink_are_refused(self, models):
        # No junction means no link properties to set.
        Tag, _Author, Post, _schema = models
        post = _saved(Post, title='Hi')
        with pytest.raises(InterfaceError, match='has no junction'):
            post.tags.add(_saved(Tag, name='t'), weight=1.0)

    def test_add_without_props_behaves_like_append(self, models):
        Tag, _Author, Post, _schema = models
        post = _saved(Post, title='Hi')
        tag = _saved(Tag, name='t')
        post.tags.add(tag)
        assert post.tags.ops == [('add', [tag], {})]

    def test_there_is_no_remove_counterpart(self, junction_models):
        # The compiler rejects link properties when unlinking, so `-=` stays
        # the only spelling and there is nothing for a remove() to carry.
        _Company, _Tag, Person = junction_models
        person = _saved(Person, name='Bob')
        assert not hasattr(person.labels, 'remove_with_props')

    def test_an_insert_carries_properties_too(self, junction_models):
        _Company, Tag, Person = junction_models
        person = Person(name='Bob')
        person.labels.add(_saved(Tag, name='t'), weight=1.0)
        pyql, _params = prepare_save(person)
        assert pyql.startswith('insert ')
        assert '@weight := <float64>$' in pyql

    def test_optional_link_property_is_allowed(self, junction_models):
        # Employment.since is optional, so the link saves without it.
        Company, _Tag, Person = junction_models
        person = _saved(Person, name='Bob')
        person.employer = _saved(Company, name='Acme')
        assert prepare_save(person) is not None


class TestMemberMatching:
    def test_members_match_via_one_array_parameter(self, models):
        # One parameter regardless of member count, so the query text is
        # stable — a set literal per member would make every length a
        # different query, fragmenting the compile cache.
        Tag, _Author, Post, _schema = models
        post = _saved(Post, title='Hi')
        post.tags += [_saved(Tag, name='a'), _saved(Tag, name='b')]
        pyql, _params = prepare_save(post)
        assert '.id in std::array_unpack(<array<uuid>>$' in pyql
        assert pyql.count('$') == 2  # the array, plus the row id

    @pytest.mark.parametrize('count', [1, 2, 5])
    def test_the_query_text_is_the_same_for_any_member_count(self, count, models):
        Tag, _Author, Post, _schema = models
        post = _saved(Post, title='Hi')
        post.tags += [_saved(Tag, name=f't{i}') for i in range(count)]
        pyql, _params = prepare_save(post)
        assert '.id in std::array_unpack(<array<uuid>>$__mq_s0)' in pyql


class TestGeneratedIdsAreAttemptLocal:
    """`Client.save` assigns generated ids inside the transaction, because a
    later statement needs them to reference rows earlier ones wrote. That
    makes them attempt-local: an id from a rolled-back attempt names a row
    that doesn't exist."""

    def test_an_insert_never_writes_the_id(self, models):
        # The column belongs to the database — its default may be uuidv7()
        # or a schema-declared generator that only exists in PyQL, and
        # sending a value from Python would bypass it.
        _Tag, Author, _Post, _schema = models
        assert 'id :=' not in prepare_save(Author(name='Bob'))[0]

    def test_a_new_object_carrying_an_id_is_refused(self, models):
        # Silently discarding the caller's id would be its own surprise, and
        # honouring it would trip allow_user_specified_id as a side effect of
        # calling save().
        _Tag, Author, _Post, _schema = models
        author = Author(name='Bob')
        author.id = uuid.uuid4()
        with pytest.raises(InterfaceError, match='ids are generated by the database'):
            prepare_save(author)

    def test_save_order_treats_an_id_less_target_as_a_dependency(self, models):
        # Clearing the id has to put the object back in the dependency set,
        # or a retry would try to reference a row that was never written.
        _Tag, Author, Post, _schema = models
        author = Author(name='Bob')
        post = Post(title='Hi', author=author)

        assert save_order([post]) == [author, post]
        author.id = uuid.uuid4()
        assert save_order([post]) == [post]
        author.__dict__['id'] = None
        assert save_order([post]) == [author, post]


class TestMarkSaved:
    def test_ops_do_not_replay(self, models):
        # Without clearing the op log, every later save would re-apply the
        # same `+=`.
        Tag, _Author, Post, _schema = models
        post = _saved(Post, title='Hi')
        post.tags += [_saved(Tag, name='a')]
        assert prepare_save(post) is not None

        mark_saved(post)
        assert post.tags.ops == []
        assert prepare_save(post) is None

    def test_links_enter_the_shadow(self, models):
        # Single links diff against __pylon_saved__, so they have to be
        # recorded there or every save would rewrite them.
        _Tag, Author, Post, _schema = models
        author = _saved(Author, name='A')
        post = Post(title='Hi', author=author)
        post.id = uuid.uuid4()
        mark_saved(post)
        assert post.__dict__['__pylon_saved__']['author'] is author

    def test_multilinks_stay_out_of_the_shadow(self, models):
        # They're tracked by the op log; diffing them as well would
        # double-count.
        _Tag, _Author, Post, _schema = models
        post = _saved(Post, title='Hi')
        assert 'tags' not in post.__dict__['__pylon_saved__']


# ── The generated text has to compile ────────────────────────────────────────


class TestGeneratedPyQLCompiles:
    def _compile(self, pyql, schema):
        from pylon._core import compile as pycompile

        return pycompile(pyql, schema)

    def test_insert_with_links_compiles(self, models):
        Tag, Author, Post, schema = models
        post = Post(title='Hi', author=_saved(Author, name='A'))
        post.tags += [_saved(Tag, name='t')]
        self._compile(prepare_save(post)[0], schema)

    def test_update_with_both_operators_compiles(self, models):
        Tag, _Author, Post, schema = models
        post = _saved(Post, title='Hi')
        post.tags += [_saved(Tag, name='a')]
        post.tags -= [_saved(Tag, name='b')]
        self._compile(prepare_save(post)[0], schema)

    def test_clearing_a_link_compiles(self, models):
        _Tag, Author, Post, schema = models
        post = _saved(Post, title='Hi', author=_saved(Author, name='A'))
        post.author = None
        self._compile(prepare_save(post)[0], schema)

    def test_replacing_a_multilink_compiles(self, models):
        Tag, _Author, Post, schema = models
        post = _saved(Post, title='Hi')
        post.tags = [_saved(Tag, name='a')]
        self._compile(prepare_save(post)[0], schema)
