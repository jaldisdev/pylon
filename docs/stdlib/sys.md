# `sys`

| Function | Signature(s) | Returns | Description |
|---|---|---|---|
| `get_current_database` | `()` | `str` | The connected PostgreSQL database name. |
| `get_version_as_str` | `()` | `str` | The Pylon version as a plain string (e.g. `"0.1.0"`). |
| `get_version` | `()` | `tuple<int64, int64, str, int64, array<str>>` | The Pylon version, structured: `(major, minor, stage, stage_no, local)` — `stage` is one of `"final"`, `"alpha"`, `"beta"`, `"rc"`, `"dev"`; `stage_no` is the pre-release number (`0` for a final release); `local` is currently always an empty array, reserved for future local-build metadata. |
