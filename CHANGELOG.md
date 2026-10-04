# Changelog

Notable changes per release. Versions are shared across the whole workspace:
the `pylon-db` Python distribution and every `pylon-db-*` crate are published
from the same version number.

## 0.4.0 — 2026-10-04

### Connections

A connection pool's type registry — the enum, domain and `vector` OIDs a
database assigns locally — is no longer read only once, when the pool connects.

* A result carrying an OID the registry doesn't know re-reads `pg_type` and
  decodes again, so a database whose contents were replaced underneath a
  running process recovers on its own rather than failing every query that
  touches an enum column until a restart.
* `migration apply` refreshes the registry after each migration, and
  `reload_schema()` (Python and Rust clients) refreshes it alongside the schema
  snapshot.

### Reading

An `array<tuple<…>>` property described itself as a plain scalar, so its
elements came back as the raw decoded jsonb — `headers[0]["value"]`, with
`headers[0].value` raising `AttributeError`.

* Each element now hydrates to the named-tuple value its declaration asks for,
  with its members reachable by name, in a `Type { property }` shape and in a
  bare `select Type.property` alike.
* `Array[SomeNamedTuple]` gets the `jsonb[]` column its values need; the
  nominal-tuple marker used to lose the array on the way to DDL.
* A field access through such a property (`.headers.name`) is reported as the
  error it always was, rather than compiling to a jsonb lookup on an array.
* A path select bound in a `with` (`with entries := Webhook.headers select
  entries`) decodes as the path itself does. It read the CTE's own result
  column as a plain scalar, so a tuple came back as raw jsonb and an enum as
  its bare label.
* `/api/schema` describes an array-typed property by its element: an
  `array<tuple<…>>` as the named tuple it holds, rather than `array<std::json>`
  — which is what left the web UI editing one as `[object Object]`. An enum
  array was worse than imprecise: the enum branch answered for it too, leaving
  the `[]` inside the name (`integration::WebhookEvent"[]`).

* A tuple literal carries the members it names. `select (amount := 9.99,
  note := 'x')` described none, so the value hydrated as a plain mapping
  rather than a named tuple, and the web UI — which renders `(key := value,
  …)` from that list — had nothing to render and showed `()`. A nested
  literal recurses and an enum member keeps the type its labels decode
  against. A free object (`{ a := 1 }`) is deliberately left without one: it
  is not a tuple to its reader.

A database carries the schema its last migration stored, so a client only sees
this once `migration apply` (or a dev-mode sync) has written the schema
snapshot again.

### Empty results

A path reaching a property yielded one result per row it crossed, including
the rows where that property is unset — a `None` standing in for a value that
isn't there. An empty set is nothing, not a NULL, so those rows now contribute
nothing at all.

* `select Webhook.headers`, over two webhooks one of which has none, is one
  result rather than one and a `None`; the same holds for the set bound in a
  `with`.
* Aggregated into an array (`.<account[is Webhook].description`) the empty was
  a NULL element, and for an array-typed property Postgres refused it outright
  — "cannot accumulate null arrays" — so that shape failed rather than merely
  differing.
* A link step already behaved this way, and a shape still reads an unset
  property as `None`: there the object is the result, not the property.
* A required property adds no condition, so its queries are emitted exactly
  as before.

### Sets of arrays

A computed pointer reaching an array-valued property through a multi-link
(`Crew { tag_sets := .<crew[is Hand].tags }`) accumulated one row's array
beside another's, which PostgreSQL has no type for: "cannot accumulate arrays
of different dimensionality". Every such pointer failed at execution.

* Each element now travels as a record of one field — the same way a set of
  objects already does — so a set of arrays is readable at all.
* Every element keeps its own column type rather than being flattened through
  jsonb: a `uuid` array reads back as `UUID`s, a `decimal` array as
  `Decimal`s, an enum array as its members, and an `array<tuple<…>>` as the
  named tuples it declares.

### Writing

A tuple-typed parameter now binds from every shape a caller holds the value
in. Only a dict used to work: jsonb keys a tuple's named members, and nothing
below the cast knew those names, so a positional `("X-Foo", "bar")` was
refused with `cannot bind a composite value as a query parameter` — which is
what made a webhook's headers unsavable.

* `<tuple<…>>$p` / `<array<tuple<…>>>$p` take a tuple, a `NamedTupleValue`
  read back from an earlier query, a `@pylon.named_tuple` class instance, or a
  mapping — including nested tuple members.
* An all-unnamed `tuple<str, bool>` binds too; it is a jsonb array rather than
  an object, and had no encoding at all before.
* `Client.save()` casts a tuple-typed property it writes, so a property
  holding one saves from an instance or a tuple the same way.
* Arity and type are reported against the argument that carries them
  (`invalid input for query argument $headers: … (expected 2 elements in
  tuple<name: …, value: …>, got 3)`) before the statement is sent, leaving the
  connection usable.

### The web UI

* `/api/query` carries the value-tag tree its `shape` field was always
  declared to: an enum reads as `module::Enum.Member`, a named tuple as
  `(x := 1, y := 2)`, a uuid as `<uuid>`. It was `null` until now, so the
  query editor rendered every value as the plain JSON it arrives as. Mirrors
  `pylon/query.py`'s `shape_value_tags`, which stays the readable statement
  of the contract.

* A decimal keeps every digit it was written with. `/api/query` parsed one
  into a float64, so `12.3400` arrived as `12.34` — the scale a money column
  is written in — and a value beyond float64's reach arrived as the nearest
  float it has. It travels as its own digits now, with the position tagged
  `decimal` so the UI still renders it as the number it is rather than as a
  quoted string. The tag comes from the values, since a shape says only
  "scalar" for both `numeric` and `float64`.

* A decimal inside a tuple keeps its digits too. A tuple travels as jsonb,
  whose numbers were parsed into an `f64` on the way out, so `12.3400` came
  back as `12.34` and a wider value as the nearest float. A jsonb number now
  carries the digits it was written with (`DecodedValue::JsonNumber`), and
  the member's own declaration decides what to build from them: a `decimal`
  member hydrates to one, a `float64` member beside it still reads as a
  float, and a json value with no declaration behind it reads as the float
  it always has. The web UI tags such a member `decimal` and renders it
  accordingly.

### Running a server

`pylon-server` is published as a container image, `ghcr.io/jaldisdev/pylon-server`,
for linux/amd64 and linux/arm64. It was the one part of a release with no
distribution at all before: not on crates.io, not in the Python package, so
running it meant a source checkout and a local build.

* The image carries the web UI compiled in and nothing else — no Python, no
  separate assets to ship. Mount a `pylon.toml` at `/etc/pylon/pylon.toml`
  and the database password arrives through the environment variable its
  `password_env` names.
* The same image runs as a worker-only container with `--no-http`.
* New `/health` route, liveness only and with no database round trip, for an
  orchestrator's probes; the image declares a healthcheck against it.

## 0.3.0 — 2026-10-02

### Loops

A `for` body that mutates now compiles in the positions it could already be
written in — each of these was accepted by the parser and then failed, either
in compilation or in the database.

* A loop whose body mutates can be a `select`'s source: `select (for x in S
  union (insert T { … })) { id }`. A source is emitted as a `CROSS JOIN
  LATERAL` and PostgreSQL takes a data-modifying statement only at the top
  level of a `WITH`, so the loop is bound to a name of its own and the select
  re-rooted there.
* A loop body that updates an interface fans out into one statement per
  implementor, each driven from the iteration and unioned back under the
  interface's common columns.
* `with` bindings in a loop body that read *each other* are driven from the
  iteration too, not just the ones naming the loop variable directly. Each
  carries an iteration key named after itself, so chained reads stay
  unambiguous.
* A for-update's iterator is defined ahead of the bindings that read it, since
  a `WITH` name is only in scope for what follows it.

### Conditional writes

A guarded insert — `(insert T { … }) if cond else {}` — writes nothing at all
when its condition is false.

* The condition reaches everything the insert writes: the row, the values
  nested inside it, and a multi-link's rows, which are hoisted beside the
  statement rather than inside it. Those were previously left behind, owned by
  nothing.
* A nested insert no longer claims the guard belonging to the one that
  encloses it. The condition is taken before anything nested compiles, the way
  a guarded update or delete already took it.

### Query compilation

* `expr is Type` reads as a single value where a single value is required — a
  FILTER, or an `if … else` condition. Naming a type other than the one being
  selected asked the question once per row of that type and produced a
  `boolean[]`, which PostgreSQL rejects as a WHERE clause or a CASE/WHEN
  condition, so the whole query failed.
* `in` answers once per element of the set on its left, rather than collapsing
  that set into one scalar subquery — which aborted with "more than one row
  returned by a subquery used as an expression". This holds for a multi-link on
  the left as well, where a single verdict over the whole link cannot express
  that some linked objects match and others don't.
* `any(…)` and `all(…)` reduce those per-element answers back to one, over the
  set's own elements rather than over the rows of the query.

### Server

* Schema and globals are served per connection: `GET /api/<connection>/schema`
  and `GET /api/<connection>/globals`. Each connection addresses a database of
  its own, with its own `_pylon."Schema"` row, so two connections agree only
  when the same migrations have been applied to both. `GET /api/schema` and
  `GET /api/globals` remain as unprefixed aliases for `main`'s.

### Schema declaration

* An optional in a function signature is recognised on Python 3.13, where
  `X | None` is a `types.UnionType`. The signature parser did not count that as
  a union, so the `None` was never stripped, the declared type resolved to
  `text`, and a body returning exactly what it declared was rejected as a
  mismatch. Parameters were read the same way.

## 0.2.0 — 2026-09-27

### Dates, times and durations

The `cal` namespace was reworked: its overloads, its unit names and its string
parsing are now consistent with `std`'s, and every constructor accepts either
discrete fields or a string.

* `date_duration` is its own scalar type, separate from `relative_duration`.
  Both carry months and both travel as one PostgreSQL type, so what
  distinguishes them is the function that built the value.
* A month-bearing duration reads back into Python as
  `pylon.datatypes.RelativeDuration`, which keeps months, days and microseconds
  apart rather than flattening them into a `timedelta` that has no room for a
  month. It is accepted as a parameter in that form too.
* A duration can be read and truncated by calendar unit.
* Dates, times and durations parse and render consistently; `to_str` and a cast
  to `str` both produce ISO 8601.
* A timezone-aware datetime converts to a local date or time in a named zone.

### Standard library

* Named arguments: a regular expression's flags, base64's alphabet and padding,
  a logarithm's base, and a range's bounds (with either endpoint left open).
* A call may take named arguments after a variadic one.
* JSON: write a value at a path of any depth; `json_get` and `array_get` fall
  back to the default they are given.
* Byte strings: encode a string, a JSON value or an integer as bytes; join an
  array of byte strings with a delimiter; count set bits; find a byte sequence,
  and an element from a position.
* Numerics: parse a formatted string into every numeric type, take the natural
  logarithm of a decimal, convert a boolean to an integer of any width, and read
  a timestamp from epoch seconds given as a float.
* A multirange is readable through the range accessors.
* `round` to a digit count is offered only for the types PostgreSQL can actually
  round that way, instead of failing in the database.

### Query compilation

* A walk binds its head, so a limit on the head stays on the head; a walk reads
  as one value where one value is what is wanted; and a walk's inner filter stays
  on its own subject when the outer select orders.
* A `with` binding keeps its array type through an aggregate and a condition.
* An optional return keeps its type, so a cast reads its JSON value out.
* A set-returning call's loop variable takes its type from the element type the
  call declares.
* An aggregate's set argument is read inside a shape, not only at the top level.
* A rewrite that reads a multi-link runs once the statement's link rows are in.

### Client

* `numeric` is encoded and decoded from its own digits rather than through a
  28-place carrier, so a value wider than that survives the round trip. This
  drops the `rust_decimal` dependency from `pylon-db-pgcon`.
* A string bound to a non-text parameter is refused before it is sent, with the
  parameter named, instead of failing in the database.
* Reading an unfetched multi-link names the pointer and its owner.
* `LinkSet` is exported, so a caller can ask whether a link was fetched.
* A script runs from every entry point, not only `Client.query`.

### CLI

* `pylon version`, `pylon info` and the REPL banner report the installed
  version. They looked it up under a distribution name that is not the one
  published (`pylon` rather than `pylon-db`), so an installed Pylon always
  called itself `(development)`. `pylon._core.__version__` now exists as well,
  which is what `pylon info` reads for the core version.

### Schema and migrations

* An interface's `exclusive` guard function is emitted with whichever implementor
  renders first, so the generated DDL is valid whatever order the implementors
  come out in.
* `pylon database wipe` clears the `default` module out of `public` rather than
  leaving it behind. `public` is shared, so its objects are dropped
  individually — modules with a schema of their own are still dropped
  `CASCADE` — and anything owned by an installed extension is left alone.

## 0.1.0

Initial release.
