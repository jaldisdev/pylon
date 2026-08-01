# `std`

## Aggregates

| Function | Signature(s) | Returns | Description |
|---|---|---|---|
| `count` | `(set of anytype)` | `int64` | Number of elements in the set. |
| `sum` | `(set of int16\|int32\|int64\|float32\|float64\|decimal)` | matching widened type (`int16`/`int32`/`int64` → `int64`) | Sum of a set. |
| `min` / `max` | `(set of anytype)` | `T?` | Smallest/largest element; `{}` for an empty set. |
| `mean` | `(set of float64\|decimal\|int64)` | `float64`/`decimal`/`float64` | Arithmetic mean. |
| `all` | `(set of bool)` | `bool` | True if every element is true (`AND` aggregate). |
| `any` | `(set of bool)` | `bool` | True if any element is true (`OR` aggregate). |
| `array_agg` | `(set of anytype)` | `array<anytype>` | Collects a set into an array, preserving order. |

## Set

| Function | Signature(s) | Returns | Description |
|---|---|---|---|
| `enumerate` | `(set of anytype)` | `set of tuple<int64, anytype>` | Pairs each element with its 0-based position. |
| `assert_single` | `(set of anytype [, msg: str])` | `T?` | Raises if the set has more than one element; otherwise passes it through. |
| `assert_exists` | `(set of anytype [, msg: str])` | `set of anytype` | Raises if the set is empty; otherwise passes it through. |
| `assert_distinct` | `(set of anytype [, msg: str])` | `set of anytype` | Raises if the set has any duplicate element; otherwise passes it through. |
| `assert` | `(condition: bool [, msg: str])` | `bool` | Raises if `condition` is false (or null, for the 2-arg form); otherwise returns it. |

```pyql
select assert_exists((select Person filter .id = <uuid>$id))
```

## String

| Function | Signature(s) | Returns | Description |
|---|---|---|---|
| `str_lower` / `str_upper` / `str_title` | `(s: str)` | `str` | Case conversion (`str_title` is initcap-style). |
| `str_pad_start` / `str_pad_end` | `(s: str, n: int64 [, fill: str])` | `str` | Pad to length `n` on the left/right (default fill is a space). |
| `str_trim` | `(s: str [, trim: str])` | `str` | Trim characters from both ends (default: whitespace). |
| `str_trim_start` / `str_trim_end` | `(s: str [, trim: str])` | `str` | Trim from one end only. |
| `str_repeat` | `(s: str, n: int64)` | `str` | Repeat a string `n` times. |
| `str_replace` | `(s: str, old: str, new: str)` | `str` | Replace every occurrence of `old` with `new`. |
| `str_reverse` | `(s: str)` | `str` | Reverse a string's characters. |
| `str_split` | `(s: str, delim: str)` | `array<str>` | Split on a delimiter. |
| `str_contains` | `(s: str, sub: str)` | `bool` | Substring test. |
| `str_starts_with` / `str_ends_with` | `(s: str, prefix\|suffix: str)` | `bool` | Prefix/suffix test. |
| `str_slice` | `(s: str, start: int64 [, end: int64])` | `str` | 0-based substring, `end` exclusive (matches [indexing/slicing](../pyql/literals-and-types.md#indexing-and-slicing) elsewhere in PyQL). |
| `str_len` | `(s: str)` | `int64` | Character length. |
| `re_match` | `(pattern: str, s: str)` | `array<str>` | First regex match's captured groups (empty array if no match). |
| `re_match_all` | `(pattern: str, s: str)` | `set of array<str>` | Every match's captured groups. |
| `re_replace` | `(pattern: str, sub: str, s: str [, flags: str])` | `str` | Regex replace — note the argument order is `(pattern, replacement, subject)`, not PostgreSQL's own `regexp_replace(subject, pattern, replacement)` order. |
| `re_test` | `(pattern: str, s: str)` | `bool` | True if `s` matches `pattern`. |
| `find` | `(haystack: str, needle: str)` | `int64` | 0-based index of the first occurrence, or `-1` if not found. |

```pyql
select str_upper(.name)
select re_test('^[A-Z]', .name)
```

## Numeric

| Function | Signature(s) | Returns | Description |
|---|---|---|---|
| `abs` | `(n: int16\|int32\|int64\|float32\|float64\|decimal)` | same type | Absolute value. |
| `ceil` / `floor` | `(n: float64\|decimal)` | same type | Round toward +∞ / -∞. |
| `round` | `(n: float64\|decimal [, d: int64])` | same type | Round to the nearest integer, or to `d` decimal places. |
| `sign` | `(n: int64\|float64\|decimal)` | same type | `-1`, `0`, or `1`. |
| `sqrt` | `(n: float64\|decimal)` | same type | Square root. |
| `random` | `()` | `float64` | Uniform random value in `[0, 1)`. |

## Generic / polymorphic

| Function | Signature(s) | Returns | Description |
|---|---|---|---|
| `len` | `(str)` / `(bytes)` / `(array<anytype>)` | `int64` | Length — characters, bytes, or elements. |
| `contains` | `(str, str)` / `(bytes, bytes)` / `(array<anytype>, anytype)` / `(json, json)` / `(range<anypoint>, range<anypoint>\|anypoint)` / `(multirange<anypoint>, multirange<anypoint>\|range<anypoint>\|anypoint)` | `bool` | Membership/containment test, overloaded across strings, bytes, arrays, JSON, and ranges. |

## UUID

| Function | Signature(s) | Returns | Description |
|---|---|---|---|
| `uuid_generate_v4` | `()` | `uuid` | Random UUID (v4). |
| `uuid_generate_v7` | `()` | `uuid` | Time-ordered UUID (v7) — what every object's own `id` is generated with. |
| `uuid_extract_timestamp` | `(u: uuid)` | `datetime` | The embedded timestamp of a v7 (or v1/v6) UUID. |
| `uuid_extract_version` | `(u: uuid)` | `int64` | The UUID version number. |
| `to_uuid` | `(val: bytes)` | `uuid` | Interpret exactly 16 raw bytes as a UUID; raises otherwise. |

`uuid_generate_v7`/`uuid_extract_*` are Pylon-specific additions, since UUIDv7 is a newer standard not yet universally supported by comparable tools.

## JSON

| Function | Signature(s) | Returns | Description |
|---|---|---|---|
| `to_json` | `(s: str)` *(cast target)* | `json` | Parse a JSON string. Also reachable via `<json>expr`. |
| `json_typeof` | `(j: json)` | `str` | The JSON value's type name (`"object"`, `"array"`, `"string"`, ...). |
| `json_get` | `(j: json, *path: str)` | `json?` | Traverse a path of keys/indices; `{}` if any step doesn't exist. |
| `json_set` | `(j: json, path: array<str>, val: json)` | `json` | Return a copy with the value at `path` replaced. |
| `json_array_unpack` | `(j: json)` | `set of json` | Each element of a JSON array as its own row. |
| `json_object_unpack` | `(j: json)` | `set of tuple<str, json>` | Each key/value pair of a JSON object. |
| `json_array_length` | `(j: json)` | `int64?` | Number of elements in a JSON array. |

## Bitwise

| Function | Signature(s) | Returns | Description |
|---|---|---|---|
| `bit_and` / `bit_or` / `bit_xor` | `(l: T, r: T)` for `T` in `int16`/`int32`/`int64` | same type | Bitwise AND/OR/XOR. |
| `bit_not` | `(r: T)` | same type | Bitwise NOT. |
| `bit_count` | `(val: int16\|int32\|int64)` | `int64` | Number of set bits. |
| `bit_lshift` / `bit_rshift` | `(val: T, n: int64)` | same type as `val` | Bit shift left/right. |
| `to_hex` | `(n: int16\|int32\|int64)` | `str` | Hexadecimal string representation. |

## Bytes

| Function | Signature(s) | Returns | Description |
|---|---|---|---|
| `bytes_get_bit` | `(b: bytes, n: int64)` | `int64` | The `n`th bit. |
| `bytes_get` | `(b: bytes, n: int64)` | `int64` | The `n`th byte, as an integer. |
| `from_hex` | `(s: str)` | `bytes` | Decode a hex string. |
| `to_bytes` | `(s: str, encoding: str)` | `bytes` | Encode a string as bytes in the given encoding. |

## Array

| Function | Signature(s) | Returns | Description |
|---|---|---|---|
| `array_get` | `(a: array<anytype>, i: int64)` | `T?` | 0-based element access; `{}` out of range instead of raising. |
| `array_unpack` | `(a: array<anytype>)` | `set of anytype` | Each element as its own row. |
| `array_join` | `(a: array<str>, delim: str)` | `str` | Join elements into a string. |
| `array_slice` | `(a: array<anytype>, start: int64 [, end: int64])` | `array<anytype>` | 0-based slice. |
| `array_index_of` | `(a: array<anytype>, el: anytype)` | `int64` | 0-based index of the first match, or `-1`. |
| `array_fill` | `(el: anytype, n: int64)` | `array<anytype>` | An `n`-length array of `el` repeated. |
| `array_replace` | `(a: array<anytype>, old: anytype, new: anytype)` | `array<anytype>` | Replace every occurrence of `old` with `new`. |
| `array_reverse` | `(a: array<anytype>)` | `array<anytype>` | Reversed copy. |
| `array_set` | `(a: array<anytype>, idx: int64, val: anytype)` | `array<anytype>` | Copy with the element at `idx` replaced. |
| `array_insert` | `(a: array<anytype>, idx: int64, val: anytype)` | `array<anytype>` | Copy with `val` inserted before `idx`. |
| `array_rotate` | `(a: array<anytype>, n: int64)` | `array<anytype>` | Rotate elements by `n` positions. |

## Range and multirange

| Function | Signature(s) | Returns | Description |
|---|---|---|---|
| `range` | `(lower: anypoint, upper: anypoint [, inc_lower: bool, inc_upper: bool])` / `(empty: bool)` | `range<anypoint>` | Construct a range (default: lower-inclusive, upper-exclusive) or an empty range. |
| `range_unpack` | `(r: range<anypoint> [, step: anypoint])` | `set of anypoint` | Every discrete point in the range. |
| `range_get_lower` / `range_get_upper` | `(r: range<anypoint>)` | `T?` | The range's bound. |
| `range_is_empty` | `(r: range<anypoint>)` | `bool` | True for an empty range. |
| `range_is_inclusive_lower` / `range_is_inclusive_upper` | `(r: range<anypoint>)` | `bool` | Whether that bound is inclusive. |
| `overlaps` | `(a: range<anypoint>, b: range<anypoint>)` | `bool` | True if the ranges share any point. |
| `multirange` | `(ranges: array<range<anypoint>>)` | `multirange<anypoint>` | Construct a multirange from a set of ranges. |
| `strictly_below` / `strictly_above` | `(l: T, r: T)` for `T` in `range<anypoint>`/`multirange<anypoint>` | `bool` | Entirely before/after, with no overlap. |
| `bounded_above` / `bounded_below` | `(l: T, r: T)` | `bool` | Doesn't extend past the other's upper/lower bound. |
| `adjacent` | `(l: T, r: T)` | `bool` | Touches with no gap and no overlap. |
| `multirange_unpack` | `(val: multirange<anypoint>)` | `set of range<anypoint>` | Each constituent range as its own row. |

## Datetime and duration

| Function | Signature(s) | Returns | Description |
|---|---|---|---|
| `datetime_current` | `()` | `datetime` | Current time, re-evaluated per row (`clock_timestamp()`). |
| `datetime_of_transaction` | `()` | `datetime` | Current transaction's start time. |
| `datetime_of_statement` | `()` | `datetime` | Current statement's start time. |
| `datetime_get` | `(dt: datetime, el: str)` | `float64` | Extract a field — `el` is one of `year`, `month`, `day`, `hour`, `minute`, `second`, `microsecond`, `millisecond`, `epoch`, `timezone`, `dow`, `doy`, `week`, `quarter`. |
| `datetime_truncate` | `(dt: datetime, unit: str)` | `datetime` | Truncate to the given unit boundary. |
| `datetime_shift` | `(dt: datetime, delta: duration)` | `datetime` | Add a duration. |
| `duration_get` | `(d: duration, el: str)` | `float64` | Extract a field — `hours`, `minutes`, `seconds`, `microseconds`, `milliseconds`, `epoch`. |
| `duration_to_seconds` | `(d: duration)` | `decimal` | Total duration in seconds. |
| `duration_truncate` | `(dt: duration, unit: str)` | `duration` | Truncate to a unit boundary — `microseconds`, `milliseconds`, `seconds`, `minutes`, or `hours`. |
| `to_datetime` | `(s: str, fmt: str)` / `(year, month, day, hour, min: int64, sec: float64, timezone: str)` / `(s: str)` *(cast target)* / `(epoch_seconds: decimal)` | `datetime` | Parse/construct a timestamp from a format string, discrete fields, ISO 8601 text, or a Unix epoch. |
| `to_duration` | `(hours: int64, minutes: int64, seconds: float64)` | `duration` | Construct a duration from discrete fields. |

## Type conversion

| Function | Signature(s) | Returns | Description |
|---|---|---|---|
| `to_str` | `(v: datetime, fmt: str)` / `(v: datetime\|int16\|int32\|int64\|float32\|float64\|decimal\|bigint\|bool\|json\|duration\|uuid)` *(cast target)* / `(v: bytes, encoding: str)` | `str` | Stringify a value — with an explicit format for `datetime`, or a text encoding for `bytes`. |
| `to_int16` / `to_int32` / `to_int64` | `(s: str)` *(cast target)* / `(b: bool)` | matching int type | Parse from string, or `0`/`1` from a boolean. |
| `to_float32` / `to_float64` | `(s: str)` *(cast target)* / `(n: int64)` | matching float type | Parse from string, or widen from `int64`. |
| `to_decimal` | `(s: str)` *(cast target)* / `(n: int64)` | `decimal` | Parse from string, or widen from `int64`. |
| `to_bigint` | `(s: str)` *(cast target)* / `(n: int64)` | `bigint` | Parse from string, or widen from `int64`. |
| `to_bool` | `(s: str)` *(cast target)* / `(n: int16\|int32\|int64)` | `bool` | Parse from string (`'true'`/`'false'`), or `0 → false`, anything else → `true` for an integer. |

Functions marked *(cast target)* are also reachable via the `<type>expr` cast syntax (e.g. `to_int64(s)` and `<int64>s` are equivalent) — see [Literals and types](../pyql/literals-and-types.md#casts).

## Sequences

| Function | Signature(s) | Returns | Description |
|---|---|---|---|
| `sequence_next` | `(seq: anytype)` | `int64` | Advance and return the next value of a [`pylon.Sequence`](../schema/properties-and-scalars.md#built-in-scalars)-typed scalar's underlying `SEQUENCE`. This is what `Default(SequenceNext)` compiles to — see [Constraints § Default](../schema/constraints.md#default). |
| `sequence_reset` | `(seq: anytype [, val: int64])` | `int64` | Reset the sequence (to `1`, or to `val`). |
