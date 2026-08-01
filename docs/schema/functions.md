# Functions

`@pylon.function` declares a user-defined PyQL function — the function body is a PyQL expression written as the Python function's **docstring**, not its body (the Python body is never executed; only the signature and docstring matter):

```python
@pylon.function
def mysum(a: pylon.Int64, b: pylon.Int64) -> pylon.Int64:
    """select a + b"""

@pylon.function(volatility=pylon.Volatility.Immutable)
def full_name(first: pylon.Str, last: pylon.Str) -> pylon.Str:
    """select first ++ " " ++ last"""
```

Call it from PyQL like any built-in: `select mysum(1, 2)`. Compiled to a real PostgreSQL `CREATE OR REPLACE FUNCTION ... LANGUAGE SQL` at DDL-export time (see [Migrations](../migrations.md)).

## Signature

| Keyword | Default | Meaning |
|---|---|---|
| `name=` | the Python function's own name | Override the function's PyQL name. |
| `module=` | inferred, same rule as a type ([Modules](modules.md)) | Which module the function belongs to. |
| `volatility=` | `None` (→ `Volatile` at DDL-emit time) | `pylon.Volatility.Immutable`, `.Stable`, or `.Volatile` — validated at decoration time; anything else is rejected immediately, not silently coerced. |
| `language=` | `pylon.Language.PyQL` | Only `pylon.Language.PyQL` is currently supported — anything else is rejected at decoration time. |

Every parameter and the return type need a real type annotation (a scalar type, or `set[T]` for an object-returning function) — a missing one is a `SchemaError` at `finalize()` time. An object-returning function must be annotated `set[T]`, never a bare `T`: single-object returns aren't supported.

## Overloading

Two functions with the same `(module, name)` are allowed as long as their parameter types differ — that's an overload. The exact same `(module, name, param_types)` signature twice is rejected:

```python
@pylon.function(module="shop", name="total")
def total_int(a: pylon.Int64, b: pylon.Int64) -> pylon.Int64:
    """select a + b"""

@pylon.function(module="shop", name="total")
def total_float(a: pylon.Float64, b: pylon.Float64) -> pylon.Float64:
    """select a + b"""
```

## Return-type checking

Like [computed pointers](computed.md), a function's body is actually compiled at `finalize()` time and its inferred return type checked against the declared one:

```python
@pylon.function
def bad(a: pylon.Int64) -> pylon.Str:
    """select a"""
# SchemaError: return type mismatch in function 'default::bad': declared text, body produces int8
```

Best-effort, not exhaustive — see [Validation](validation.md) for the exact scope (a body that's a bare parameter reference, a cast, or a literal is always checked; a body built from a nested function call or a binary operation may be silently skipped rather than falsely flagged, pending a future pass that widens what the checker can infer).
