# `cal`

Local (timezone-naive) date/time construction and arithmetic — the calendar counterpart to [`std`](std.md#datetime-and-duration)'s timezone-aware `datetime`/`duration` functions, operating on `local_datetime`/`local_date`/`local_time`/`relative_duration` instead.

| Function | Signature(s) | Returns | Description |
|---|---|---|---|
| `to_local_datetime` | `(dt: datetime, timezone: str)` / `(year, month, day, hour, min: int64, sec: float64)` / `(s: str, fmt: str)` | `local_datetime` | Convert a timezone-aware datetime to local time in a given zone, construct from discrete fields, or parse from a formatted string. |
| `to_local_date` | `(dt: local_datetime)` / `(year: int64, month: int64, day: int64)` / `(s: str, fmt: str)` | `local_date` | Extract the date part, construct from fields, or parse. |
| `to_local_time` | `(dt: local_datetime)` / `(hour: int64, min: int64, sec: float64)` / `(s: str, fmt: str)` | `local_time` | Extract the time part, construct from fields, or parse. |
| `local_datetime_get` | `(dt: local_datetime, el: str)` | `float64` | Extract a field — `year`, `month`, `day`, `hour`, `minute`, `second`, `microsecond`, `millisecond`, `epoch`, `dow`, `doy`, `week`, `quarter`. |
| `date_get` | `(d: local_date, el: str)` | `float64` | Extract a field — `year`, `month`, `day`, `dow`, `doy`, `week`, `quarter`. |
| `time_get` | `(t: local_time, el: str)` | `float64` | Extract a field — `hour`, `minute`, `second`, `microsecond`, `millisecond`. |
| `to_duration` | `(days, hours, minutes: int64, seconds: float64)` / `(years, months, days, hours, minutes: int64, seconds: float64, microseconds: int64)` | `relative_duration` | Construct a calendar-aware duration (as opposed to `std::to_duration`'s fixed-length one) from discrete fields — a 4-field short form or a full 7-field form. Both forms are positional; PyQL has no named-only parameter support, unlike the keyword-only convention this mirrors elsewhere. |
| `duration_normalize_hours` | `(d: relative_duration)` | `relative_duration` | Roll excess minutes/seconds up into hours. |
| `duration_normalize_days` | `(d: relative_duration)` | `relative_duration` | Roll excess hours up into days (after first normalizing hours). |
