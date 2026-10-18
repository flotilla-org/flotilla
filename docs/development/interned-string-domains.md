# Interned string domains

Every dictionary row has a globally unique local `id`, a `domain` and original
text `s`. Uniqueness is `(domain, s)`: equal text in different sorts gets distinct
ids. Referencing columns and readable views continue to use the globally unique
id, so no view can confuse two rows with equal text.

String and string-newtype fields declare their domain as
`#[store(intern = "wr")]`. The derive requires an explicit literal and checks it
with the existing `Code` grammar: one to eight bytes, starting with a lowercase
ASCII letter, then lowercase ASCII letters, digits or `_`. Codes are open-ended;
there is no central SQL list and no default sort. The table's SQL CHECK enforces
only nonempty text; the derive and `Code` constructors impose the tighter rule
on application writes.

An `Address` column intrinsically declares `ad`; this also covers optional
addresses and addresses used as child-table keys. Engine name-only `Ref<K>`
columns intrinsically declare `rn`. SQLite identity references (`schema::Ref`)
remain ids in `resource_identity`, whose `name` and `home` refer to `rn` and `hm`
dictionary rows. Logical codec values and replicated documents still carry text;
domains come from the receiving schema, never local ids or sender-supplied tags.

This intentionally supersedes #3392 ruling D3's temporary Rust-type restriction:
the #3422 contract replaces it with agreement on the semantic domain. Both String
and its internable newtypes use the same `Value::Str` logical representation.
A column shared by variants must agree on its intern domain. Different Rust
string newtypes may share that column when their domains agree, including `T`
versus `Option<U>`. Each variant retains its own codec. Non-interned data still
requires one Rust type and storage form (allowing required/optional variants).

## Domain assignments

The complete generated assignment table for all registered engine and synthetic
fixture kinds is checked in as part of the
[readable schema snapshot](../../crates/flotilla-schema-sqlite/tests/integration/snapshots/schema_view.md).
It includes enum expansion, flattened fields, and child keys. Regenerating the
schema snapshot regenerates this table, so declarations cannot silently drift
from the review evidence. The same table is included in PR #3452's description.

| Code | Meaning |
|---|---|
| `ad` | Canonical address, including condition/authority addresses |
| `rn` | Resource name, including engine name-only references |
| `hm` | Home/host name |
| `wr` | Writer/principal name (including synthetic ticket owners/signatures) |
| `sn` | Consumer session name |
| `pk` | Promise kind |
| `ag` | Synthetic agent name |
| `sv` | Synthetic forge/service name |
| `lb` | Synthetic deliverable label |
| `st` | Synthetic workflow step |
| `qu` | Synthetic queue name |
| `rl` | Synthetic crew role |
| `pl` | Synthetic terminal pool |
| `sl` | Synthetic agent selector |
| `tx` | Isolated codec/derive fixture text, never a production fallback |

Shared tables have these additional declarations:

| Table | Column | Domain |
|---|---|---|
| `resource_identity` | `name` | `rn` |
| `resource_identity` | `home` | `hm` |
| `home_sequence` | `home` | `hm` |
| `log` | `home` | `hm` |
| `log` | `writer` | `wr` |

## Runtime audit and statistics

`Store::audit_domains()` explicitly checks all registered interned columns,
including child keys, and the five shared columns above. It refuses missing
string rows or ids whose dictionary domain disagrees with the declaration,
naming the table and column as `StoreError::Corrupt`. Nullable columns may be
null. The standalone `audit_domains(&Connection, &KindRegistry)` accepts a read-only
connection. For an engine store, operators can run:

```sh
cargo run -p flotilla-schema-sqlite --locked --example audit_domains -- /path/to/store.db
```

The example uses SQLite's read-only open flag, does not create missing databases
or rewrite schemas, prints success, and exits nonzero with the corruption error
on failure. It registers the engine's production kinds; applications adding kinds
must register those too when calling the standalone function to audit them.
`Store::audit_domains()` delegates to the same function with its full registry.
The diagnostic scans the relevant tables and is deliberately outside the
per-event/read path. Ordinary foreign-key
checks alone cannot detect ids from a different domain. Writes, cache keys,
index arguments, snapshots and replay all derive their sort from the same schema.

Per-domain counts and text payload bytes (including multibyte UTF-8) are available
without maintaining counters on each write:

```sql
SELECT domain, count(*) AS strings,
       sum(length(CAST(s AS BLOB))) AS text_bytes
FROM strings
GROUP BY domain;
```

These are payload bytes, not SQLite page/index sizes. This is an explicit O(N)
diagnostic aggregate, returning one row per domain; it is not an event-path
query or an API that materializes the dictionary.

## Workload and evolution

WORKLOAD.md sizes change events at 1,000/s target and 10,000/s burst ceiling.
For N distinct (domain, text) pairs and k interned values per event, misses require
O(k log N) indexed lookups per event (10,000*k per burst second), within the
existing single-writer transaction. Reads by dictionary id retain their primary
key lookup. There are no additional network or durable-store crossings.

The committed two-generation string cache retains its configured entry bound;
keys now include a `Code` and owned text. Lookups use a borrowed composite view
through std::HashMap's `Borrow` contract, so pending/committed hits allocate no
string, and old-generation promotion moves the existing owned key. The allocation
regression test measures a warm public scan with a 16 KiB home. Shared column
descriptors are reference-counted immutable slices: a lowering call takes one
handle and zips the relevant columns with values, rather than looking up the
kind for every value. Pending keys live only for their write
transaction and rewind with failed-write savepoints. The dictionary continues
to retain strings indefinitely; retention remains separate work.

One global dictionary preserves the existing foreign keys. Separate tables per
sort would require changing every reference and readable view. Keeping uniqueness
on text alone would violate the domain isolation contract.

Named workload gaps remain: distinct-string cardinality, domains per text,
mean text bytes, k, and hit rate. Domain text is repeated in each dictionary row
and its unique index; equal text across sorts also duplicates the text itself.
No claim of fitting the store's 10 GB ceiling is made without those inputs.
The test property deliberately uses a one-entry cache and colliding text across
sorts, exercising misses, rollback, restart and replication.

This is the baseline before the new store cut-over, as specified by #3422. No other package depends on
`flotilla-schema-sqlite` in this checkout; it has not replaced the live store.
A store missing `strings.domain` is refused at open (and by the standalone audit)
with `StoreError::LegacyDictionarySchema` before any writer or domain query runs.
This change provides no old-dictionary migration. ADR 0047's compatibility rule
applies when these tables are deployed. Resource serialization, out-of-repo
resource authors and Quint specifications do not change.

## Dictionary row-cost sample

A local Python SQLite in-memory experiment inserted 100,000 distinct 13-byte
texts, using the old DDL and then the new DDL with a two-byte `ad` domain.
Page size was 4,096 bytes; both counts include the table and unique index.
A second pass put the same text in `wr` as well, exercising cross-domain
collisions that the old schema could not represent.

| Dictionary | Rows | Allocated bytes | Insert seconds | 100k lookup seconds |
|---|---:|---:|---:|---:|
| Old, text-only uniqueness | 100,000 | 4,710,400 | 0.131 | 0.202 |
| New, one domain | 100,000 | 5,349,376 | 0.161 | 0.202 |
| New, same text in two domains | 200,000 | 10,731,520 | — | — |

This sample adds approximately 6.4 bytes per row for the domain in the table
and uniqueness index, including page allocation effects. Repeating text across
two sorts approximately doubles storage here. Timings are one local run and
include Python driver overhead; they do not measure Rust caches, WAL durability,
or deployment latency. They support the row-cost warning, not a workload-budget
fit claim. The Rust scenario property separately forces a one-entry cache and
exercises cache misses across domains.
