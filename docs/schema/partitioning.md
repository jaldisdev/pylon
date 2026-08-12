# Partitioning

`Partition` declares a type's table as range-partitioned on a time property.
PostgreSQL stores the objects across one child table per time range instead of
one large table, which keeps queries filtered to a range from scanning the
whole set, and makes dropping old data an instant metadata operation rather
than a mass delete.

Partitions are created and dropped by `pg_partman`, driven by a maintenance
worker Pylon starts for you.

```python
@pylon.type
class Event(pylon.BaseObject):
    occurred_at: Property[pylon.DateTime]
    payload: Property[pylon.JSON]

    pylon.Partition('occurred_at', interval='monthly', retention=12)
```

## Arguments

`Partition(pointer, *, interval='monthly', premake=4, retention=None)`

| Argument | Meaning |
| --- | --- |
| `pointer` | The property to partition on. Must be a **required** `DateTime`, `LocalDateTime`, or `LocalDate` property of this type. |
| `interval` | Partition width: `'daily'`, `'weekly'`, `'monthly'`, or `'yearly'`. |
| `premake` | How many future partitions to keep ready. |
| `retention` | Drop partitions older than this many intervals. `None` keeps everything. |

## Rules

**One per type.** A table has exactly one partition key, so a second
`Partition` contradicts the first rather than refining it. Declaring two is an
error at class-definition time.

**Concrete types only.** `@pylon.abstract` has no table of its own — its
fields flatten into each concrete subtype, so partitioning is a decision each
of those makes for itself. `@pylon.interface` is backed by a view, which has
no storage to partition. Both are rejected.

**The key must be required.** PostgreSQL has no range for a `NULL` partition
key to land in, so an optional property is rejected.

**The key joins the primary key.** PostgreSQL requires a partitioned table's
primary key to include its partition column. Pylon adds it for you, so
`Event` above gets `PRIMARY KEY ("id", "occurred_at")`.

## Retention deletes data

`retention=12` with `interval='monthly'` keeps a rolling twelve months and
**drops** everything older, permanently — the partition is dropped, not
archived. That is the point of the setting, but it is worth being deliberate
about: the default is `None`, which keeps everything, because data disappearing
on a schedule should never be something you get by accident.

## Maintenance

`pg_partman` creates a table's initial partitions when the migration runs and
then does nothing further on its own. Something has to call
`run_maintenance` to create the next ranges and drop expired ones, and if
nothing does, everything works right up until writes reach a range nobody
created — at which point every insert past that boundary fails at once.

Pylon runs that for you: `pylon serve` starts a `PartitionMaintenanceWorker`
whenever the schema contains a `Partition`, on an hourly cycle. At the
shortest supported interval (daily) that is 24 passes per partition, so
maintenance has to be down for a long time before `premake` runs out.

If you run `pylon-server` with `--no-http` for a worker-only container, the
maintenance worker starts there too.

## Requirements

Partitioning needs the `pg_partman` extension. `pylon migration create`
emits `CREATE EXTENSION IF NOT EXISTS "pg_partman"` for a schema that declares
a `Partition`, but the extension must be **installed on the server** first —
it isn't part of a stock PostgreSQL distribution. On Debian/Ubuntu that is
`postgresql-18-partman`; several container images bundle it.

Without it, the maintenance worker logs that pg_partman isn't installed and
exits cleanly rather than retrying forever.

## Querying a partitioned type

Nothing changes. `select Event filter .occurred_at > <datetime>$since` is
written exactly as it would be for an unpartitioned type — PostgreSQL prunes
the partitions the filter can't match. Inserts route to the right partition
automatically.
