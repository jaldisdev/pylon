# `crypto`

Requires the `pgcrypto` PostgreSQL extension (`CREATE EXTENSION IF NOT EXISTS pgcrypto;`) — same expectation as `pgvector`'s `vector` extension, neither auto-provisioned by Pylon.

| Function | Signature | Returns | Description |
|---|---|---|---|
| `digest` | `(data: str\|bytes, type: str)` | `bytes` | Hash `data` with the named algorithm (`'md5'`, `'sha1'`, `'sha256'`, `'sha512'`, ...). |
| `hmac` | `(data: str\|bytes, key: str\|bytes, type: str)` | `bytes` | HMAC of `data` under `key`, using the named hash algorithm. |
| `gen_salt` | `()` / `(type: str)` / `(type: str, iter_count: int64)` | `str` | Generate a password salt — defaults to blowfish (`'bf'`) with no arguments; `type` selects the algorithm (`'bf'`, `'md5'`, `'des'`, `'xdes'`); `iter_count` controls the algorithm's own cost/iteration parameter where applicable. |
| `crypt` | `(password: str, salt: str)` | `str` | One-way password hash, using the algorithm embedded in `salt` (as produced by `gen_salt`). |

```pyql
select crypt($password, gen_salt('bf'))
```
