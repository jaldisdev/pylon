# Changelog

Notable changes per release. Versions are shared across the whole workspace:
the `pylon-db` Python distribution and every `pylon-db-*` crate are published
from the same version number.

## 0.8.0

### Migrations

* `migration create` now records the state a migration reaches on that
  migration's own tracking row, and `migration apply` reads it rather than
  deriving one from the schema files. Since `create` refuses to run while
  anything is pending, editing the schema between creating a migration and
  applying it is the normal way to work — and every such edit used to be
  recorded as though it were already live. The next `create` then diffed
  against a database that never existed: a schema that had grown lost the
  delta under "No schema changes detected", one that had shrunk re-emitted
  statements the previous migration already carried.
* A migration created with changes left out — a rejected step, or stopping
  the prompt loop early — doesn't reach the compiled schema, so it records
  no baseline at all and says so. The next `create` falls back to
  introspecting the live database.
* `migration apply --to` no longer writes a baseline derived from the schema
  files when it stops short of the chain tip, and no longer publishes those
  files' shape to `_pylon."Schema"` for clients to read.
* `migration watch` now records itself as the writer of the schema snapshot,
  and `migration apply` reads that and rebases onto the DDL watch pushed
  without being told to. Forgetting `--dev-mode` used to abort a step with a
  bare Postgres duplicate-object error partway through; the flag is still
  accepted for databases carrying no marker. Applying rewrites the marker as
  its own, so the rebase stops once the drift is recorded.
* `migration status` reports that `watch` was the last writer from the
  snapshot row itself. `--dev-mode` still adds the introspection diff that
  says *what* drifted.
* `migration log --from-db` shows a created-but-unapplied migration as
  `(pending)` rather than printing `None` for its timestamp.
* Adding a multi-link to a type whose table already exists produced a
  migration that could not be applied: the junction table's target foreign
  key rendered ahead of the `CREATE TABLE` for the junction itself, so
  `apply` stopped on `relation "…" does not exist` and the file had to be
  reordered by hand and re-hashed. A foreign-key step now sorts after every
  step that creates a table. Most visible on an interface, where one
  multi-link means a junction table per implementor.
* `_pylon."Internal".version` is now 2: `_pylon."Schema"` gained
  `written_by` (a `_pylon."SchemaWriter"` enum) and `written_onto`. Additive,
  so the minimum supported version is unchanged.

## 0.7.0 — 2026-10-16

### Full-text search

* A delete written against an interface or a supertype left its rows'
  documents in a deferred (OpenSearch/Meilisearch) index forever. A search
  index is declared on the concrete type that holds the rows, so the
  statement's own type carried none and the delete enqueued nothing at all;
  the fan-out it already does for the rows themselves now enqueues one
  delete job per implementor, carrying that implementor's own type name.
* An update written against an interface or a supertype did not re-index the
  rows it changed either, for the same reason — their documents kept the
  values the update replaced.
* A delete read for its rows — `select (delete Product filter …) { id }` —
  left their documents behind too, whatever the type.

### Search and vector indexes

* `for … union (insert …)` — the bulk-write idiom — enqueued no index jobs at
  all, so the rows a loop wrote reached the table and neither a deferred
  search index nor a `VectorIndex`: no documents, and a NULL embedding that
  no vector search could ever match. A loop that updates had the same hole.
  Both now enqueue one job per row written, off the rows the loop itself
  wrote.
* A write bound to a name — `with d := (insert …)`, or a statement hoisted out
  of a link's value — enqueued no index jobs either, for the same reason: a
  binding's DML is emitted as a CTE of its own and nothing hung an enqueue off
  those. Each binding now carries its own jobs, named after the binding.

### Comparing against a set

An equality against a set is true when any pair of elements is equal, which is
a membership test. Three ways of writing one were read as a comparison against
a single value instead, and compiled to a scalar subquery over the whole set —
SQL that can only fail, with `more than one row returned by a subquery used as
an expression`, the moment the set holds two.

* A **backlink** compared to a binding of more than one row:
  `filter any(.<versions[is Knowledge] = sources)`. The walk was correctly
  lifted into an `EXISTS`, but the comparison inside it was built by hand. The
  same shape over a forward multi-link was already right, which is what it now
  matches.
* A walk ending on `.id` — `filter .posts.id = ids` — which reads the
  junction's target column rather than joining the target table, and built its
  own comparison for the same reason.
* A path read *off* such a binding, rather than the binding itself:
  `filter .name = sources.name`. One answer per row of the binding, however
  single-valued the walk off it. This one reached plain property comparisons
  too, not only walks. A binding capped at `limit 1` is still a single value.
* A set-valued `filter` condition over a backlink now warns and asks for
  `any()`, the way one over a multi-link already did. The reading is the same
  either way; only the multi-link spelling said so.

### Indexes

* Deleting a `pylon.Index` declaration emitted no `DROP INDEX` and did not
  change the migration at all — the diff only ever asked what Postgres was
  *missing*, so the index stayed alive in the database with nothing in the
  schema referring to it, maintained on every write. Replacing an `Index` with
  a `VectorIndex` on the same pointer was the shape that showed it: a B-tree
  over an unbounded text column, orphaned. A removed declaration now drops its
  index.
* An `Index` whose key, uniqueness or `unless` predicate changed under an
  unchanged name was read as already present and left as it was. It is now
  replaced by what the declaration says.
* `pylon.schema.export()`'s DDL dump left a plain `Index` unnamed, so Postgres
  auto-named it `Event_occurred_at_idx` where the migration diff names it
  `Event__occurred_at_idx`. Since an index with a key is settled by name, a
  database built from that dump had a *second* index proposed over the same
  column by its first `migration create`. Every path that names an index now
  reads one helper, so the dump, the create path, the drop path and the diff's
  baseline cannot disagree. A name past Postgres's 63-byte identifier limit is
  capped there too, rather than by three paths separately.

### Links

* A nested insert used as a **multi-link**'s value through the wrapped
  spelling — `tags := (select (insert Tag { … }) { id })`, the one a single
  link takes — was dropped on the way to SQL, and every existing row of the
  target type was linked in its place. No error, no warning; only the junction
  rows showed it. The statement is now hoisted into its own CTE exactly as the
  single-link form is, so what gets linked is what it wrote. The bare `tags :=
  (insert Tag { … })` and `with`-bound spellings were never affected.
* Nested DML that cannot be hoisted where a multi-link value is read —
  including a filter or a page written over it — is refused at compile time
  now, rather than quietly read as a plain select over the whole table.
* A filter or a page over nested DML written as a **single** link's value was
  dropped on the way to SQL rather than refused, the same silence a multi-link
  value is now held to. Both say the same thing now.
* Nested DML in a pointer's value inside a `for` loop reached Postgres as a
  value naming an iterator its CTE had no `FROM` entry for — `for d in … union
  (insert Doc { owner := (select (insert Tag { label := d }) { id }) })` failed
  with `missing FROM-clause entry`, in every spelling and for a link and a
  multi-link alike. A hoisted statement that reads the loop variable now
  stages its rows under the iteration that wrote them, and whatever reads it
  pairs on that key, so each row gets the row its own iteration wrote rather
  than every iteration's. Reading a loop variable from *outside* the loop the
  value sits in is refused — one key is carried, so there is nothing to pair a
  second one on; it used to reach an `unreachable!` and surface as a panic.

### Rust client

A `decimal` round-tripped through the Rust client as a `String` and nothing
else — the last scalar family with no native Rust type on either side of a
query, where the Python binding has surfaced one as a real `decimal.Decimal`
in both directions all along.

* `bigdecimal::BigDecimal` now binds as a `<decimal>$x` argument and decodes
  out of a result, including into a `#[derive(Queryable)]` field. Both decimal
  paths reach it: a `numeric` column, and a member a jsonb tuple's declaration
  says is a decimal rather than the float its digits would otherwise parse as.
  `bigdecimal` rather than `rust_decimal` because `numeric` is unbounded and
  the wire carries it losslessly — a fixed 96-bit, 28-digit carrier would have
  narrowed that silently at the one type where silence is least affordable.
  Reading a decimal as a `String` still works exactly as before.
* `numeric`'s `NaN` and infinities have no `BigDecimal` counterpart, so they
  are a decode error naming the value rather than some finite number that
  isn't it. A `String` still reads them.

## 0.6.0 — 2026-10-14

### Full-text search

`fts::search` could only ever search a whole type, and several things a query
routinely carries — a cast around the query text, a page, a parameter of its
own — did not survive the trip through a deferred (OpenSearch/Meilisearch)
backend.

* `fts::search` now takes a filtered subquery as its first argument — `select
  fts::search((select Product filter .seller = <uuid>$seller), $query)` — the
  second form `vector::search` already accepted, and the way a search is
  narrowed to the rows its caller may see.
* `limit` and `offset` now also apply in SQL for a deferred backend. The
  filter runs after the backend's window, so only SQL can trim the survivors
  to the page, and a parameterised limit reached the backend as nothing at
  all. The window itself is still sized by a literal `limit`, else 100, so a
  narrow filter over a large index can return fewer rows than asked for.
* A written-out cast around the query text — `<str>$query`, which says what
  `$query` says — left the plan with no parameter to resolve, and the search
  ran for the empty string.
* A deferred search carrying any parameter of its own failed with a bare
  `KeyError` on that parameter's name. Only the injected ids/scores and the
  query text are special now; a filter's parameters, a page size and a session
  global bind the way they do for a plain statement.
* Searching an index nothing has been written to yet raised. The backend
  creates the index with the first document pushed to it, so a surface kept
  failing until something happened to be indexed; no documents is no hits.
* `backend` and `api_key`/`api_key_env` under `[search]` in `pylon.toml` were
  read as names of connections and then dropped, so `backend = "meilisearch"`
  silently stayed on the OpenSearch default it was overriding.

### Search and vector indexes

Adding a search or vector index to a type that already held objects indexed
nothing: only writes enqueue indexing jobs, so the index covered the objects
written after the migration and no others. Keeping an index whole was an
operator's job, and nothing told them it had become one.

* `pylon migration create` now detects a deferred (OpenSearch/Meilisearch)
  search index or a `VectorIndex` that is new, or whose definition changed,
  and writes the outbox backfill for it into the migration file. Every
  database the migration reaches rebuilds the same way, and the backfill is
  visible in review. `pylon migration watch` does the same against a dev
  database.
* Changing a `VectorIndex`'s `metric` deliberately does not re-embed — it
  changes the hnsw operator class built over the vectors, not the vectors —
  so it rebuilds the index alone. Every other change to an index (pointers,
  model, dimensions, backend) counts as invalidating.
* `pylon migration create --no-index-backfill` leaves the backfill out, for
  running it at a controlled time instead.
* New `pylon index list` and `pylon index rebuild`, for what a migration
  cannot know about: a search engine wiped, moved, or replaced out from under
  the database. `rebuild` takes `--type`, `--index`, and `--kind`, reads the
  schema as actually applied, and reports how many objects each index covers
  before asking.

Three index changes also had no effect on the database at all, since the diff
engine reads introspected state and Postgres exposes none of them:

* Changing the pointers or weights of a `Postgres`-backed `SearchIndex` left
  the generated `tsvector` column with its original expression forever. The
  column is now dropped and re-added — which recomputes it for every object
  the type already holds — and its GIN index rebuilt.
* Changing a `VectorIndex`'s `dimensions` left the column at its original
  `vector(n)` width, so every subsequent write failed on a dimension
  mismatch. The column is now rebuilt at the new width and re-embedded.
* Removing an index, or moving a `SearchIndex` off the `Postgres` backend,
  left its column behind on the type — and a generated one was still
  recomputed on every write. Both are now dropped.

### CLI

* Declining a confirmation prompt — or pressing Ctrl-C at one — printed a bare
  `error:` with nothing after it, since `click.Abort` carries no message. It
  now reports `Aborted.`, without the error prefix: saying no is a decision,
  not a failure.

### Python

* Python 3.15 is tested and released against. The extension is not abi3, so
  each interpreter needs a wheel of its own; `pylon-db` now publishes
  `cp315` wheels for every platform it already covered, alongside the `cp313`
  and `cp314` ones. The supported floor is unchanged at 3.13.

## 0.5.0 — 2026-10-04

### Tuples

A tuple was stored and carried as jsonb, which has one number type and no
scale, so a `decimal` member read back as the nearest float and was
indistinguishable from a `float64` one. A tuple is the composite row it is
now — one PostgreSQL type per member, all the way down.

* A tuple-typed property's column is a composite type of its own, in the
  Postgres schema of the module that declares it: a `@pylon.named_tuple`
  class is named for itself (`geo::Point` becomes `geo."Point_t"`), and a
  structural `pylon.Tuple[...]` for its content. An `array<tuple<…>>`
  property holds one composite per element.
* Reading one back gives each member the type it was declared with, scale
  included, in a `Type { property }` shape, a bare `select Type.property`,
  and an aggregated set of them alike.
* `select Type.property.member` reads a field of that row. It arrives as
  the type it is declared with rather than as raw json, and naming a member
  the tuple doesn't declare is a compile error listing the ones it does —
  a composite has no absent key to hand back as an empty value.
* A tuple written out in a query is a row too, so `(amount :=
  <decimal>'12.3400', note := 'x')` keeps every digit beside a `float64`
  member, and a nested tuple keeps its own members' types.
* A tuple cast over a tuple written out in the query is a row as well:
  `<tuple<int64, str>>('1', 3)` coerces each member to the type the cast
  names, and `<module::Money>(amount := '12.3400', currency := 'EUR')` is a
  value of that declared type's own composite — so it compares against a
  column of it, which a json value could not.
* A tuple **parameter** is bound as a value of the tuple's own composite
  type, so each member arrives as the type it is declared with. Carried as
  json — which is how this used to travel — several member types had no
  representation at all: a `bytes` member arrived as hex text, a non-finite
  `float64` as nothing, a `uuid` as a string, and a `datetime` as something
  no timestamp accepts, which failed outright on the way into a column.
  A structural `tuple<…>` binds against whatever property declares that
  shape; where nothing does and a member is one json cannot carry, the query
  is refused with what to declare rather than arriving as the wrong thing.
  Every caller binds it that way — the Rust client and the web UI, which
  hands its arguments over as plain JSON, included.
* A free object (`{ a := 1 }`) is the same composite row, so a nested one's
  fields keep their own types too — `{ nested := { amount :=
  <decimal>'1.2300' } }` read back as a plain mapping of json-parsed values,
  losing a `decimal`'s digits and turning a `uuid` into a string. It is
  still described and rendered as an object, not a tuple.
* `<json>` of a tuple is unchanged: a named one becomes an object keyed by
  its member names, a positional one an array. A `notify()` payload also
  still travels as json, which is how a listener reads it.
* Comparing a tuple against one works: `filter .price = (amount := …,
  currency := …)` and `filter .price = <module::Money>$p` both read the
  value into the column's own type first, where the literal was refused as
  a dissimilar type and the parameter as a `std::json`.
* A tuple property under another name (`Line { p := .price }`) keeps its
  members reachable by name, rather than hydrating as a bare tuple.
* An array literal of tuples (`select [(a := 1), (a := 2)]`) keeps each
  element's members, where they used to come back as bare tuples.
* A member of a tuple that is unset — or of a tuple that is itself unset —
  contributes nothing to a path's results, matching every other empty. A
  tuple with one empty member is still a value, where `IS NOT NULL` on a
  composite would have discarded it for not being wholly set.
* A `@pylon.named_tuple` declared in a module using `from __future__ import
  annotations` had every member silently typed `text`, since its annotations
  arrived as strings. They are resolved the way a `@pylon.type`'s already
  were.

A database that stored tuples as jsonb is converted by its next migration:
each column is retyped and its rows read into the composite, positional
tuples (stored as json arrays) included. An `array<tuple<…>>` column holding
an empty array keeps one, rather than becoming an absent value. A named
tuple whose members change has its type replaced rather than altered —
PostgreSQL can append an attribute but not insert one, and a tuple's members
are read by position.

The conversion fills a tuple's members by name, so a stored object carrying
a key the tuple does not declare loses it silently and the member it was
meant for arrives unset. Check the keys actually stored
(`jsonb_object_keys`, per element for an array column) against the declared
member names *before* migrating a database whose history you don't know —
afterwards the original json is gone.

### Command line

* `pylon -d <name> query …` runs against the connection it names. It silently
  ran against the base `[database]` instead, so a query could read one
  database while reporting on another's schema; `pylon -d <name>` with no
  subcommand opened the interactive session on the base connection the same
  way.

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
