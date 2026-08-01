# Constraints

Most constraints attach inside a `Property[T, ...]`/`Link[T, ...]` annotation. A few also (or only) work as a standalone class-body expression, for composite/whole-type constraints.

## `Default`

```python
created_at: pylon.Property[pylon.DateTime, pylon.Default(pylon.Now)]
seq: pylon.Property[pylon.Sequence, pylon.Default(pylon.SequenceNext)]
score: pylon.Property[pylon.Int64, pylon.Default(0)]
```

`Default(Now)` and `Default(SequenceNext)` use the two special sentinels (`pylon.Now`, `pylon.SequenceNext`) that compile to `now()` and `nextval(...)` respectively — the latter only valid on a `pylon.Sequence`-typed property, which additionally provisions a real PostgreSQL `SEQUENCE`. Any other value (a literal, or a PyQL expression string) becomes the column's server-side default, compiled and type-checked the same way a [computed pointer](computed.md) is — see [Validation](validation.md).

`id` always gets a default (`uuidv7()`) automatically; never declare one yourself.

## `Exclusive`

```python
# Single pointer — bare class reference, no parentheses:
email: pylon.Property[str, pylon.Exclusive]
owner: pylon.Link[User, pylon.Exclusive]

# Composite, class-body form:
@pylon.type
class Membership:
    tenant_id: pylon.UUID
    slug: str
    pylon.Exclusive(("tenant_id", "slug"))
    pylon.Exclusive(("tenant_id", "slug"), unless=".deleted")
```

Inside `Property[...]`/`Link[...]`, `Exclusive` is used **bare** — the class object itself, not an instance (`Exclusive()` with no arguments would raise, since the class-body form's `__init__` requires a `pointers` argument). For a composite uniqueness constraint across more than one pointer, instantiate it in the class body instead, optionally with `unless=` for a partial unique index (only enforced where the given PyQL boolean expression is false/null).

## `Expression`

```python
@pylon.type
class Booking:
    start_date: pylon.DateTime
    end_date: pylon.DateTime
    pylon.Expression("__subject__.start_date <= __subject__.end_date")
```

An arbitrary PyQL boolean expression, checked on every write — a `CHECK` constraint. `__subject__` refers to the row being validated.

## `Readonly`

```python
created_by: pylon.Link[User, pylon.Readonly]
slug: pylon.Property[str, pylon.Readonly, pylon.MaxLen(120)]
```

Also a bare class reference. The column is still writable at the database level (e.g. by a migration, or Postgres itself); Pylon's PyQL transpiler rejects any query that tries to assign to it.

## Value and length bounds

```python
price: pylon.Property[pylon.Decimal, pylon.MinValue(0)]
rating: pylon.Property[pylon.Int16, pylon.MinValue(1), pylon.MaxValue(5)]
name: pylon.Property[str, pylon.MinLen(1), pylon.MaxLen(120)]
```

| Constraint | Meaning |
|---|---|
| `MinValue(n)` / `MaxValue(n)` | Inclusive numeric lower/upper bound. |
| `MinExValue(n)` / `MaxExValue(n)` | Exclusive numeric lower/upper bound. |
| `MinLen(n)` / `MaxLen(n)` | Minimum/maximum character (or element) length. |
| `Regexp(pattern)` | The value must match a regular expression. |
| `OneOf(*values)` | The value must be one of an explicit set — for an ad hoc restricted set of values without declaring a full [`Enum`](properties-and-scalars.md#enums). |

Every one of these applies equally whether used inline on a built-in scalar or baked into a [registered custom scalar](properties-and-scalars.md#custom-scalars) via `pylon.scalar(base, *constraints, name=...)` — in the latter case it becomes part of the scalar's own PostgreSQL `DOMAIN` `CHECK`, shared by every property that uses it, instead of being repeated per-property.

## `Description`

```python
price: pylon.Property[pylon.Decimal, pylon.Description("Price excl. tax")]

@pylon.type
class Product:
    pylon.Description("A product available for purchase.")
```

Purely documentation — surfaces in introspection output. Works both inline on a pointer and as a standalone class-body expression describing the type itself (overriding the class docstring, if the class body also has one).
