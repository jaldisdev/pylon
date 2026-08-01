# Properties and scalars

## `Property[]`

```python
@pylon.type
class Product:
    name: str
    price: pylon.Property[pylon.Decimal, pylon.MinValue(0)]
    sku: pylon.Property[str, pylon.Exclusive]
```

A bare annotation (`name: str`) and `Property[T]` with no constraints are equivalent — `Property[T, *constraints]` is only needed once you attach [constraints](constraints.md) (`Default`, `Exclusive`, `MinValue`, etc.) or want to be explicit. A property is required (`NOT NULL`) unless its type is a `T | None` union, in which case it's nullable.

## Built-in scalars

| Pylon scalar | Python shorthand | PostgreSQL type |
|---|---|---|
| `pylon.Str` | `str` | `text` |
| `pylon.Int16` | — | `int2` |
| `pylon.Int32` | — | `int4` |
| `pylon.Int64` | `int` | `int8` |
| `pylon.Float32` | — | `float4` |
| `pylon.Float64` | `float` | `float8` |
| `pylon.Decimal` | `decimal.Decimal` | `numeric` |
| `pylon.Bool` | `bool` | `boolean` |
| `pylon.DateTime` | `datetime.datetime` | `timestamptz` |
| `pylon.LocalDateTime` | — | `timestamp` |
| `pylon.LocalDate` | `datetime.date` | `date` |
| `pylon.LocalTime` | `datetime.time` | `time` |
| `pylon.Duration` | `datetime.timedelta` | `interval` |
| `pylon.UUID` | `uuid.UUID` | `uuid` |
| `pylon.JSON` | — | `jsonb` |
| `pylon.Bytes` | — | `bytea` |
| `pylon.Sequence` | — | `int8` (backed by a PostgreSQL `SEQUENCE`) |

The Python shorthand column is a plain-Python-type annotation Pylon recognizes and resolves to the matching scalar automatically — `name: str` and `name: pylon.Property[pylon.Str]` produce an identical column. Use `pylon.Sequence` with `Default(pylon.SequenceNext)` for an auto-incrementing column backed by a real Postgres sequence — see [Constraints](constraints.md#default).

## Custom scalars

Two ways to define one, both via `pylon.scalar(...)` or the `@pylon.scalar` decorator form (`pylon/schema/_scalars.py`).

**Functional form, registered** — creates a real PostgreSQL `DOMAIN`; its constraints compile into the domain's own `CHECK`, enforced by Postgres on every write, and any property using it gets the domain as its actual column type:

```python
EmailStr = pylon.scalar(pylon.Str, pylon.Regexp(r"^[^@]+@[^@]+\.[^@]+$"), name="EmailStr")
Rating = pylon.scalar(pylon.Int16, pylon.MinValue(1), pylon.MaxValue(5), name="Rating")

@pylon.type
class Review:
    rating: Rating
```

**Functional form, unregistered** — omit `name=` and no named PostgreSQL type is created; the constraints still apply, just individually on each property that uses the scalar rather than via a shared domain:

```python
PositiveInt = pylon.scalar(pylon.Int64, pylon.MinValue(0))
```

**Decorator form**, for full control over Python-side (de)serialization — `validate()`/`from_db()`/`to_db()` hooks on a `pylon.Scalar` subclass, for logic a plain `CHECK` constraint can't express:

```python
@pylon.scalar(pylon.Str)
class Email(pylon.Scalar):
    @staticmethod
    def validate(value: str) -> None:
        if "@" not in value:
            raise ValueError(f"Invalid email: {value!r}")

    @staticmethod
    def from_db(value: str) -> "Email":
        return Email(value)

    @staticmethod
    def to_db(value: "Email") -> str:
        return str(value)
```

## Enums

```python
@pylon.enum("Active", "Inactive", "Pending")
class Status(pylon.Enum):
    pass

Status.Active            # <Status.Active: 'Active'>
Status.Active.value      # 'Active'
```

Members are declared positionally on the decorator, not as class-body assignments — the decorated class body itself is discarded. Member names are PascalCase; PostgreSQL stores the member name string verbatim in a native `ENUM` type. Reference a member in PyQL with `module::EnumName.Member` (see [Literals and types](../pyql/literals-and-types.md)).

## Named tuples

```python
@pylon.named_tuple
class Point(pylon.NamedTuple):
    x: pylon.Float64
    y: pylon.Float64

p = Point(x=1.0, y=2.0)
```

A registered value type — stored as `jsonb` with a type marker so query results decode back into real `Point` instances, not plain dicts. Use as a property's type directly: `location: Point`.

## Structural tuples and arrays

`Tuple[...]` and `Array[T]` are *structural* — no separate declaration needed, unlike a named tuple:

```python
from pylon.schema import Tuple, Array

@pylon.type
class Shape:
    origin: Tuple[("x", pylon.Float64), ("y", pylon.Float64)]   # named elements
    dims: Tuple[pylon.Float64, pylon.Float64]                    # positional elements
    tags: Array[pylon.Str]                                        # pylon.Array[T], or bare list[str]
```

`Tuple[...]` elements are either all named (`("name", type)` pairs) or all positional — mixing the two forms is rejected. A tuple always stores as `jsonb`; an array is a native PostgreSQL array column (`text[]`, `int8[]`, ...), never `jsonb`, so it decodes without a type marker — this holds even when the array's own element type is itself a tuple (`Array[Tuple[...]]` → `jsonb[]`, still a real array, just of jsonb-backed elements). A bare `list[str]` is shorthand for `Array[pylon.Str]`, the same way `str` is shorthand for `pylon.Str`. Tuples can nest (`Tuple[("origin", Tuple[...]), ...]`); an array cannot directly contain another array (`Array[Array[T]]` is rejected — wrap the inner dimension in a tuple or use a genuine 2D use case differently).

**Only scalar types are allowed as elements** — an object type (`@pylon.type`/`@pylon.interface`) used inside `Array[...]` or `Tuple[...]`, at any nesting depth, is rejected at `finalize()` time with a clear `SchemaError` rather than silently degrading to an opaque `text[]`/`jsonb` column with no way to actually reference the object:

```python
Array[Product]                          # SchemaError: expected a scalar type, got an object type
Tuple[Product, pylon.Str]               # same
Tuple[Tuple[Product, pylon.Str], int]   # caught even nested
```

If you need a collection of *object references*, that's a [`MultiLink`](links.md), not an array.
