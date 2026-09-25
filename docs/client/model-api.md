# Model API

Build and run queries against your `@pylon.type` classes instead of writing PyQL text — the model classes *are* the query builder.

```python
import pylon
from pylon import std
from models import Person

async with pylon.create_async_client() as client:
    everyone = await client.query(Person)
    bobs = await client.query(Person.filter(lambda p: std.ilike(p.name, '%bob%')))

    bob = Person(name='Bob')
    await client.save(bob)

    bob.name = 'Robert'
    await client.save(bob)

    await client.execute(Person.filter(id=bob.id).delete())
```

Everything here renders to ordinary PyQL text plus a params dict and goes through the same compile/cache/execute pipeline as a hand-written query. There is no separate execution path — anything the model API can express, you could have written by hand.

## Selecting

`client.query(Model)` selects every row, with a shape covering the model's properties:

```python
people = await client.query(Person)          # select default::Person { id, name, age }
```

`Model.filter(...)` narrows it. Keyword arguments are equality tests; a callable receives a field-path proxy and returns an expression:

```python
Person.filter(name='Bob')
Person.filter(lambda p: p.age > 30)
Person.filter(lambda p: p.company.name == 'Acme')     # follows links
```

Chained `.filter()` calls are combined with `and`:

```python
Person.filter(lambda p: p.age > 18, name='Bob')       # both, ANDed
Person.filter(name='Bob').filter(lambda p: p.age > 18)
```

### Combining expressions

Python's `and`, `or`, and `not` cannot be overloaded, so use `&`, `|`, and `~`:

```python
Person.filter(lambda p: (p.age > 18) & (p.name == 'Bob'))
Person.filter(lambda p: (p.age < 18) | (p.age > 65))
Person.filter(lambda p: ~(p.name == 'Bob'))
```

Using a filter expression in a boolean context raises `TypeError` rather than silently evaluating as always-true:

```python
Person.filter(lambda p: p.age > 18 and p.name == 'Bob')   # TypeError
```

Every literal becomes a bound parameter, never inlined text, so values go through the same type-coercion path as a hand-written `$param`.

### Comparing against a saved object

A saved instance stands for the row it identifies, and compares by id:

```python
Person.filter(lambda p: p.company == acme)      # filter .company = $p0, bound to acme.id
```

An *unsaved* instance is refused — it has no id, so there is nothing to match against.

### Reusing one query inside another

Assign a query to a variable and use it as a value; it becomes a `with` binding:

```python
acmes = Company.filter(name='Acme')
await client.query(Person.filter(lambda p: std.in_(p.company, acmes)))
```

```pyql
with __mq_q0 := (select default::Company { id, name } filter .name = $p0)
select default::Person { id, name } filter .company in __mq_q0
```

The same rule as everywhere else: the *same object* referenced twice is one binding. Two separately-constructed queries stay separate.

Binding isn't optional here — PyQL rejects a bare sub-statement in expression position, so a nested query is always hoisted.

### What can't be a value

Objects that mean "a query construct" rather than a value are rejected rather than bound as parameters:

```python
Person.filter(lambda p: p.company == Company)   # the type itself — did you mean Company.filter(...)?
Person.filter(lambda p: p.name == std)          # the namespace itself — call a function on it
```

### Deleting

```python
await client.execute(Person.filter(id=bob.id).delete())
```

`.delete()` requires a filter. An unfiltered delete raises rather than emptying the table — write the PyQL by hand if you genuinely mean it.

## The `std` namespace

`std`, `math`, `cal`, and `sys` expose the [standard library](../stdlib/index.md) as Python objects:

```python
from pylon import std, math, cal

Person.filter(lambda p: std.ilike(p.name, '%bob%'))
Person.filter(lambda p: std.str_lower(p.name) == 'bob')
Person.filter(lambda p: math.ln(p.score) > 1.0)
```

Names and argument counts are checked against the real registry when you write the call, not when the query reaches the database:

```python
std.strlower(...)      # AttributeError: unknown function std.strlower() — did you mean std.str_lower?
std.str_lower()        # InterfaceError: std.str_lower() takes 1 argument(s), got 0
math.sqrt(...)         # AttributeError: unknown function math.sqrt() — it lives in std, use std.sqrt()
```

That last one is worth knowing: several functions a Python developer expects in `math` deliberately live in `std` (`std.sqrt`, `std.abs`, `std.ceil`, `std.floor`, `std.round`). The error points you across.

Two naming rules:

- **Constants are values, not calls.** `math.pi` and `math.e` are zero-argument immutable entries, so they read as attributes. Both `math.pi` and `math.pi()` work, so picking one doesn't break the other.
- **Python keywords get a trailing underscore.** `std::assert` is reachable as `std.assert_`, since `std.assert(...)` is a syntax error. The same convention already applies to the infix aliases `in_` / `not_in`.

### Infix operators

A few PyQL operators are keyword-only in the grammar and have no callable form. They're offered as functions for convenience and render back to infix syntax:

| Written | Renders as |
|---|---|
| `std.ilike(a, b)` | `a ilike b` |
| `std.like(a, b)` | `a like b` |
| `std.not_ilike(a, b)` | `a not ilike b` |
| `std.not_like(a, b)` | `a not like b` |
| `std.in_(a, b)` | `a in b` |
| `std.not_in(a, b)` | `a not in b` |

### What's admissible where

The same `std` object is used in filters and in [pointer defaults](../schema/constraints.md#default), but the two accept different things:

| | Filter / query | Pointer default |
|---|---|---|
| Pure functions (`std.str_lower`, `math.ln`) | yes | yes |
| Volatile (`std.uuid_generate_v7`, `std.datetime_current`) | yes | yes |
| Aggregates (`std.count`, `std.sum`) | yes | **no** — nothing to aggregate over |
| Set-returning (`std.array_unpack`) | yes | **no** — a column needs one value |
| Modifying (`std.sequence_next`, `std.sequence_reset`) | **no** — fires per row | via `Default(SequenceNext)` |

Volatility on its own is *not* disqualifying in a filter: `.expires_at > std.datetime_current()` is volatile and entirely correct.

The `to_*` cast functions (`std.to_int64`, `std.to_str`, …) exist here too, but are rarely what you want from Python: a Python value already has a type, and it becomes a typed parameter. They're for casting a *column* — `std.to_str(p.age)`.

## Saving

```python
bob = Person(name='Bob')
await client.save(bob)      # INSERT; bob.id is populated

bob.name = 'Robert'
await client.save(bob)      # UPDATE, and only if something changed
```

An instance that was never loaded from a query is inserted; one that was is diffed against the values it was loaded with. An unchanged object is skipped, not rewritten. `save()` accepts any number of instances and writes them in one transaction.

Fields left at their Python-side default of `None` are omitted from the INSERT, so server-side defaults still apply.

## Links

Assign a saved instance:

```python
bob.company = acme
await client.save(bob)

bob.company = None          # clears it
await client.save(bob)
```

The target doesn't have to be saved yet. `save()` writes unsaved link targets first, so you can build an object graph and save the root:

```python
acme = Company(name='Acme')
bob = Person(name='Bob', company=acme)
await client.save(bob)          # inserts Acme, then Bob referencing it
```

Everything happens in one transaction, and every object gets its generated `id` written back — including the ones you didn't pass in. A target shared by several objects is written once, decided by object identity:

```python
acme = Company(name='Acme')
await client.save(Person(name='A', company=acme), Person(name='B', company=acme))
# one Acme, two People
```

Two unsaved objects that point at each other can't be ordered, and raise rather than looping — save one first so the other can reference it.

### Multi-links

`+=` and `-=` add and remove members:

```python
bob.friends += [alice, carol]
bob.friends -= [dave]
await client.save(bob)
```

Assigning a plain list replaces the whole set:

```python
bob.friends = [alice]       # tags := (select ...) — a full replace
```

These are recorded as *operations*, not as a state diff, because `+=` on a member that is already linked is a no-op server-side and so is `-=` on one that isn't — neither shows up as a change. Order is preserved (`+= [a]` then `-= [a]` is not the same as the reverse) and consecutive same-kind operations are merged into one statement.

### Links are not fetched by default

`client.query(Person)` does not load links — doing so would fan out across tables for data most callers don't want. Request them in a shape when you need them:

```python
people = await client.query('select Person { name, friends: { name } }')
```

A multi-link that wasn't fetched still accepts `+=` and `-=`, because PyQL applies those server-side without needing the current members:

```python
p = await client.query_single('select Person { id, name } filter .id = <uuid>$i', i=some_id)
p.friends += [alice]        # fine — no fetch needed
await client.save(p)
```

Reading one that wasn't fetched raises instead of showing an empty list, which would be a lie:

```python
len(p.friends)              # AttributeError: cannot take the length of Person.friends — it was not fetched
```

Code that would rather fall back to a second query than raise can ask first — `is_hydrated` is the only read an unfetched `LinkSet` allows:

```python
from pylon import LinkSet

friends = p.friends
if isinstance(friends, LinkSet) and not friends.is_hydrated:
    friends = await client.query('select Person { name } filter .friends.id = <uuid>$i', i=p.id)
```

### Link properties

A `Through[...]` link stores its members in a junction, which can carry properties of its own. `+=` supplies a target and nothing else, so those use `.add()`:

```python
product.tags += [plain]                    # no link properties
product.tags.add(featured, weight=1.0)     # with link properties
product.tags.add(sale, weight=0.5)
await client.save(product)
```

```pyql
tags += (select default::Tag filter .id in std::array_unpack(<array<uuid>>$a)),
tags += (select default::Tag filter .id in std::array_unpack(<array<uuid>>$b)) { @weight := <float64>$w1 },
tags += (select default::Tag filter .id in std::array_unpack(<array<uuid>>$c)) { @weight := <float64>$w2 }
```

**One target per call.** A `@prop` value attaches to the whole selection of a `+=` clause, so targets with different values are different clauses — one call maps to exactly one. Calls sharing the same values are merged back into a single clause.

**No `remove()` counterpart.** The compiler rejects link properties when unlinking (*"link properties (`@prop := value`) cannot be assigned when removing a link"*), so `-=` stays the only spelling.

Property names are checked against the junction as you write the call:

```python
product.tags.add(tag, weigth=1.0)   # InterfaceError: unknown link property weigth on tags — did you mean weight?
```

Using bare `+=` on a junction that *requires* a property is refused, pointing at `.add()`:

```python
product.tags += [tag]   # InterfaceError: ... requires link property weight — `+=` supplies a
                        # target and nothing else. Use `.add(target, weight=...)` instead.
```

Reading link properties still needs a shaped query — `select Product { tags: { name, @weight } }`.

## What the model API doesn't cover

It deliberately handles the common cases rather than all of PyQL. Write PyQL text for:

- shapes other than "all properties" — nested link shapes, computed shape elements
- `order by`, `limit`, `offset`, `group`, `for`
- `unless conflict` / upserts
- *reading* link properties (writing them is covered by [`.add()`](#link-properties))
- anything the [PyQL reference](../pyql/index.md) covers that has no builder equivalent

`client.query()` and `client.execute()` accept a model, a `ModelSet`, or a plain string, so the two mix freely in one codebase.
