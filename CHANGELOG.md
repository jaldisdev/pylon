# Changelog

Notable changes per release. Versions are shared across the whole workspace:
the `pylon-db` Python distribution and every `pylon-db-*` crate are published
from the same version number.

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
