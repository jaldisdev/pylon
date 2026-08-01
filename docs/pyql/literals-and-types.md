# Literals and types

## Literals

```pyql
'a string'
42
3.14
true
false
```

Four literal kinds: string, integer, float, boolean. There's no separate literal syntax for `null`/`none` — an empty result is represented by an empty set, not a null value (see [Operators § Coalesce](operators.md#coalesce)).

## Casts

```pyql
<str>$value
<uuid>$id
<int64>.some_text_column
```

`<TypeExpr>expr` — casts `expr` to the given type. `TypeExpr` is a named type (a built-in scalar like `str`/`int64`/`uuid`, or a schema-qualified `module::Name` for an enum/named tuple/custom scalar), a structural tuple type, or an array type — the same three shapes described below.

## Tuples

**Positional:**

```pyql
select (1, 'hello')
```

**Named:**

```pyql
select (x := 1.0, y := 2.0)
```

Field/positional access:

```pyql
(name := 'a', age := 1).name
(1, 3.14, 'red').2
```

A cast to a structural tuple type spells each element's type inside `tuple<...>`: `<tuple<str, bool>>expr`, or `<tuple<r: int16, g: int16, b: int16>>expr` for named elements. See [Properties and scalars § Structural tuples and arrays](../schema/properties-and-scalars.md#structural-tuples-and-arrays) for the schema-level declaration these correspond to.

## Arrays

```pyql
select ['a', 'b', 'c']
```

A cast to an array type: `<array<str>>expr`. Arrays are one-dimensional — an array of arrays isn't representable; nest a tuple instead if you need a fixed-size grouping inside each element.

## Set literals

```pyql
select { 1, 2, 3 }
```

Curly braces around a comma-separated list of values (not a shape — no property names) — a set literal, producing one result row per element. Don't confuse this with `{ field, field }`, a [shape](paths-and-shapes.md) applied to some other expression; which one a `{ }` block means is determined by what precedes it (nothing/a bare value list → set; an object-producing expression right before it → shape).

## Named tuples and enums

Reference a registered [named tuple](../schema/properties-and-scalars.md#named-tuples) type in a cast the same way as any other named type: `<module::PointName>expr`. Reference an [enum](../schema/properties-and-scalars.md#enums) member with the qualified type name and the member:

```pyql
select Person filter .status = default::Status.Active
```

## Indexing and slicing

```pyql
some_array[0]
some_array[1:3]
some_array[:3]
some_array[1:]
```

0-based, works on arrays and (where meaningful) strings — same semantics as Python slicing, either bound may be omitted.
