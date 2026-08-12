# Operators

## Comparison

| Operator | Meaning |
|---|---|
| `=` | Equal |
| `!=` | Not equal |
| `<`, `<=`, `>`, `>=` | Ordering |
| `like`, `ilike` | Pattern match (case-sensitive / case-insensitive) |
| `not like`, `not ilike` | Negated pattern match |
| `in`, `not in` | Set membership |

```pyql
select Person filter .name like 'A%'
select Person filter .age in {18, 21, 65}
```

## Logical

| Operator | Meaning |
|---|---|
| `and`, `or` | Boolean combination |
| `not` | Prefix negation |
| `exists` | Prefix — true if the operand set is non-empty |

```pyql
select Person filter .age >= 18 and .active = true
select Post filter exists .<posts[is Person]
```

## Arithmetic

| Operator | Meaning |
|---|---|
| `+`, `-`, `*`, `/` | Standard arithmetic |
| `//` | Floor division |
| `%` | Modulo |
| `^` | Exponentiation |
| `-` (prefix) | Unary negation |

## String concatenation: `++`

```pyql
select Person { full_name := .first_name ++ ' ' ++ .last_name }
```

`++` concatenates strings (and, per PostgreSQL's own overloads, other concatenable types like arrays).

## Coalesce: `??`

```pyql
select Person { display_name := .nickname ?? .first_name }
```

Returns the left operand if it's a non-empty result, otherwise the right — Pylon's empty-set semantics mean this is the closest equivalent to SQL's `COALESCE`/a null-coalescing operator, since there's no literal `null`.

## `is` — type check

```pyql
select Publishable filter Publishable is Post
```

Returns a boolean — true when the operand's actual concrete type matches (or, for an interface target, implements) the named type. See [Object types § `@pylon.interface`](../schema/types.md#pyloninterface-polymorphic-view) for the polymorphic-view context this is most useful in. Not to be confused with the `[is TypeName]` **type-intersection** path step (see [Paths and shapes](paths-and-shapes.md#type-intersection-is-typename)), which restricts/narrows a path rather than testing a boolean condition.

## Set operators: `union` / `except`

```pyql
select Person filter .age < 18 union select Person filter .age > 65
select Person except select Person filter .active = false
```

`union` combines two sets (compiles to `UNION ALL` — duplicates are kept); `except` returns the elements of the left operand not present in the right.

## Precedence (loosest to tightest)

```
union / except
if / else
or
and
not
comparison (=, !=, <, <=, >, >=, like, in, ...)
?? (coalesce)
+ / - / ++ (add, subtract, concat)
* / / / // / % (mul, div, floor-div, mod)
^ (power)
unary (not, -, exists, distinct)
postfix (path traversal, indexing, casts, function calls)
```
