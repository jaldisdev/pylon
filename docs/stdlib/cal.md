# `cal`

Local (timezone-naive) date/time construction and arithmetic — the calendar counterpart to [`std`](std.md#datetime-and-duration)'s timezone-aware `datetime`/`duration` functions, operating on `local_datetime`/`local_date`/`local_time`/`relative_duration` instead.

| Function | Signature(s) | Returns | Description |
|---|---|---|---|
| `to_local_datetime` | `(dt: datetime, timezone: str)` / `(year, month, day, hour, min: int64, sec: float64)` / `(s: str, fmt: optional<str>)` | `local_datetime` | Convert a timezone-aware datetime to local time in a given zone, construct from discrete fields, or parse a string. Without `fmt` the string must be ISO 8601 (`2010-04-18T09:27:00`, `20100418 0927`); `fmt` may not name a time zone field. |
| `to_local_date` | `(dt: datetime, timezone: str)` / `(dt: local_datetime)` / `(year: int64, month: int64, day: int64)` / `(s: str, fmt: optional<str>)` | `local_date` | Take the date a timezone-aware datetime falls on in a given zone, extract the date part, construct from fields, or parse. Without `fmt` the string must be ISO 8601 (`2010-04-18`, `20100418`). |
| `to_local_time` | `(dt: datetime, timezone: str)` / `(dt: local_datetime)` / `(hour: int64, min: int64, sec: float64)` / `(s: str, fmt: optional<str>)` | `local_time` | Take the wall-clock time a timezone-aware datetime falls at in a given zone, extract the time part, construct from fields, or parse. Without `fmt` the string must be ISO 8601 (`18:43:27`, `18:43`, `184327`). |
| `date_get` | `(d: local_date, el: str)` | `float64` | Extract a field — `century`, `day`, `decade`, `dow`, `doy`, `isodow`, `isoyear`, `millennium`, `month`, `quarter`, `week`, `year`. |
| `time_get` | `(t: local_time, el: str)` | `float64` | Extract a field — `hour`, `minutes`, `seconds`, `milliseconds`, `microseconds`, or `midnightseconds` for the seconds since midnight. |
| `to_relative_duration` | `(named only years, months, days, hours, minutes: int64, seconds: float64, microseconds: int64, each defaulting to 0)` | `relative_duration` | Construct a calendar-aware duration (as opposed to `std::to_duration`'s fixed-length one) from discrete fields. |
| `to_date_duration` | `(named only years, months, days: int64, each defaulting to 0)` | `date_duration` | Construct a whole-day duration. The units below a day are the ones it cannot be given. |
| `duration_normalize_hours` | `(d: relative_duration)` | `relative_duration` | Roll excess minutes/seconds up into hours. |
| `duration_normalize_days` | `(d: relative_duration)` / `(d: date_duration)` | `relative_duration` | Convert 30-day chunks into months. Hours are left alone — compose with `duration_normalize_hours` to roll those up first. |
