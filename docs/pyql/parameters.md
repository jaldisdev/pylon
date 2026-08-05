# Parameters

Two forms, both bound from the client side rather than interpolated into the query string — never build a PyQL string by hand-interpolating a value, the same reasoning as parameterized SQL.

## Positional: `$0`, `$1`, ...

```pyql
select Person filter .name = $0 and .age > $1
```

**0-indexed** — the first positional argument is `$0`, not `$1`. From the client:

```python
await client.query("select Person filter .name = $0 and .age > $1", "Alice", 18)
```

`Client.query(pyql, *args, **kwargs)`'s positional `args` map directly to `$0`, `$1`, ... in order.

## Named: `$name`

```pyql
select Person filter .name = $name
```

```python
await client.query("select Person filter .name = $name", name="Alice")
```

Bound from `Client`'s keyword arguments — `kwargs["name"]` fills `$name`. Named parameters are generally preferable for readability once a query has more than one or two parameters.

## Casting a parameter

A bare `$param` takes its type from context where possible, but an explicit cast (`<uuid>$id`) is often necessary — e.g. Pylon can't always infer that a string parameter destined for a `uuid`-typed column should be parsed as one without being told:

```pyql
select Person filter .id = <uuid>$id
insert Person { id := <uuid>$id, name := $name }
```

(That last example needs `allow_user_specified_id` — see [Python client § with_config](../client/python.md#with_config); `id` is normally server-generated.)

## Missing/unused parameters are compile errors

Referencing `$name` without supplying it raises `MissingParameterError`; supplying a keyword argument the query never references raises `UnknownParameterError` — both at compile time, not silently ignored. See [Python client § Exceptions](../client/python.md#exceptions).
