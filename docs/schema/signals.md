# Signals

`@pylon.signal` registers an async Python callback that fires after a mutation on a given type **commits**:

```python
@pylon.signal(Person, on=pylon.On.Insert | pylon.On.Delete)
async def sync_to_crm(old: Person | None, new: Person | None) -> None:
    if new is not None:
        await push_to_crm(new)
    else:
        await remove_from_crm(old)
```

`on` defaults to `On.Insert | On.Update | On.Delete` (every event) if omitted. `old` is always `None` for `On.Insert`; `new` is always `None` for `On.Delete`. Both `old`/`new` are hydrated from the type's own stored properties and single-link foreign-key columns only, exposed as plain `<name>_id` values rather than resolved link objects — the same data a database-level trigger's own `OLD`/`NEW` would see, since neither a computed pointer nor a multilink has a backing column to capture in the first place.

## How it actually runs

Unlike everything else in this reference, a signal handler isn't compiled to SQL — it's a live Python callable, and the dispatch pipeline is built around that:

1. A schema-defined **capture trigger** (real Postgres DDL, emitted the same way any other trigger is) fires on the mutation and writes a row to `_pylon."SignalOutbox"` — this part happens at the database level, regardless of which process or server issued the mutation.
2. A separate, always-Python process — `pylon.signals.run_signal_dispatcher`, started via [`pylon worker start`](../cli.md#pylon-worker) — polls that outbox table, claims rows (`FOR UPDATE SKIP LOCKED`, plus a `LISTEN`/`NOTIFY` wakeup), and invokes every handler registered for that row's `(type, operation)`.
3. A handler that raises is retried with exponential backoff (up to 5 attempts) before being marked `Failed`; one failing handler doesn't block the rest of the batch.

## Deployment: signals need their own process

Every other background worker has a native Rust implementation. Vector and search indexing run in [`pylon-server`](../server.md) alone; cache invalidation runs either there or in a Python process, since it has to sit next to the cache directory it evicts from. **Signals are the one worker with no Rust implementation at all** — a registered handler is a live Python object that only exists in a Python process, so `pylon-server` never runs the dispatcher, under any flag combination.

If your schema has any `@pylon.signal` registrations, `pylon worker start` must be running *somewhere* — alongside `pylon-server`, or alongside a Python-embedded `Client`, it doesn't matter which server handles the mutation itself. The capture trigger is database-level and fires regardless; only the *dispatch* half needs a live Python process, and nothing warns you if it's missing — outbox rows just accumulate unprocessed. This was verified end-to-end: a mutation through `pylon-server` correctly lands a row in `_pylon."SignalOutbox"`; a running `pylon worker start` process then correctly claims it and invokes the registered handler with a properly hydrated `old`/`new`.
