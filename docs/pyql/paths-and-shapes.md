# Paths and shapes

## Paths

A path is a chain of property/link traversal steps. **Relative** paths start with `.` and are resolved against whatever object is currently in scope (a shape element, a `filter`, an `update ... set { }` assignment); **absolute** paths start with a type name and stand on their own:

```pyql
.name                          # relative — the current object's own `name`
.company.name                  # relative, through a link
Person.name                    # absolute — every Person's name, as a flat set
```

### Backlinks

`.<link_name` traverses a link **backwards** — every object of some other type whose own `link_name` link points at the current object:

```pyql
select Post filter exists .<posts[is Person]
select Post { title, authors := .<posts[is Person] { name } }
```

Since more than one type could plausibly declare a link named `posts`, a backlink is usually followed by a type filter — see below.

### Type intersection: `[is TypeName]`

Restricts a path (most often a backlink, or a polymorphic [interface](../schema/types.md) reference) to instances of a specific concrete type, and lets you reach fields that only exist on that type:

```pyql
select Account { *, [is Individual].* }
select Post { title, authors := .<posts[is Person] { name } }
```

### `detached`

```pyql
select Person { name, other := (select detached Person filter .id != .id) }
```

Evaluates the wrapped expression independently of the current implicit scope — breaks a path out of whatever object it would otherwise be resolved relative to. Useful inside a nested shape when you need a genuinely separate query, not one correlated to the row currently being projected.

## Shapes

`Expr { element, element, ... }` — the mechanism that turns "a set of objects" into "a set of specific fields," and the only thing that makes nested link data appear in a result at all (a bare `select Person` with no shape returns just `id`):

```pyql
select Person {
    name,
    age,
    company: { name },
    full_name := .first_name ++ " " ++ .last_name,
}
```

Each element is one of:

- **A bare pointer name** (`name`) — project that property/link as-is.
- **A nested shape** (`company: { name }`) — for a link, project only the given fields of the linked object(s) instead of just its `id`.
- **A computed override** (`full_name := expr`) — compute a value inline, same shape as a schema-level [`Computed`](../schema/computed.md) pointer but scoped to just this one query.
- **A splat**: `*` expands to every scalar property; `**` expands to every scalar property *and* every single link (each projected as an implicit `{ id }`).

### Per-element modifiers

A shape element for a multi-valued link can carry its own `filter`/`order by`/`offset`/`limit`, scoped to just that nested set — independent of the outer statement's own modifiers:

```pyql
select Author {
    name,
    posts: { title } filter .published = true order by .created_at desc limit 5,
}
```

### Link properties: `@name`

Inside a nested shape for a `Through[...]`-junction link, `@propname` reaches a property declared on the junction type itself, alongside the target object's own fields:

```pyql
select Product { name, tags: { name, @weight } }
```

See [Links § Link properties in PyQL](../schema/links.md#link-properties-in-pyql) and [Object types § Junctions](../schema/types.md#junctions-pylonjunction) for how the junction type itself is declared.

### Assignment operators (mutations only)

Inside an `update ... set { }` shape, a multi-link element can use `+=`/`-=` instead of `:=` — see [INSERT, UPDATE, DELETE § UPDATE](insert-update-delete.md#update).
