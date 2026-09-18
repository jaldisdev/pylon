# Computed pointers

`Computed[return_type, "pyql_expression"]` declares a pointer whose value is evaluated by a PyQL expression at query time, not stored:

```python
@pylon.type
class Person:
    first_name: str
    last_name: str
    full_name: pylon.Computed[pylon.Str, '.first_name ++ " " ++ .last_name']
    recent_orders: pylon.Computed[pylon.MultiLink[Order], "(select .orders order by .created_at desc limit 5)"]
```

The expression is inlined wherever the computed pointer is referenced in a query — `.first_name`/`.last_name` above resolve against the same object the computed pointer is projected from, exactly like they would in a hand-written `select Person { first_name, last_name }`.

A computed pointer:

- Is excluded from `__init__` — you never assign one directly on a new instance.
- Cannot appear in a constraint or an index (nothing to index — it isn't a column).
- Only appears in a query result when explicitly selected in the shape, same as any other pointer.

## Sub-selects

`filter`, `order by`, `offset` and `limit` need a `select` of their own — an expression is not a statement, so they can't trail a bare path. Wrap the path in parentheses:

```python
recent_orders: pylon.Computed[pylon.MultiLink[Order], "(select .orders order by .created_at desc limit 5)"]
primary_email: pylon.Computed[pylon.Str, "(select .emails filter .primary = True limit 1).address"]
```

The two produce different kinds of pointer, and which one you get follows from what the sub-select ends in:

- Ending on a link (`(select .orders … )`) makes it an object pointer — the linked objects, with a shape, exactly as if the query had written `orders: { … } order by … limit 5` inline.
- Projecting a property off it (`(select .emails … ).address`) makes it a scalar — one correlated subquery per row, returning that column.

A sub-select over a [function](functions.md) that returns objects works the same way, as long as it projects a property: `"(select account::owner(.id) limit 1).name"`. The call itself stays object-valued, so it can't be the whole expression.

## Return-type checking

The declared `return_type` isn't just documentation — `pylon.finalize()` compiles the expression and checks that it actually produces that type, the same way it checks [function](functions.md) return types:

```python
bad: pylon.Computed[pylon.Int64, ".first_name"]   # SchemaError: declared int8, expression produces text
```

This check is best-effort, not exhaustive — see [Validation](validation.md) for exactly which expression shapes it can and can't verify. A mismatch it *can* detect is rejected at `finalize()` time, well before the expression would otherwise only fail the first time some query happened to select it.
