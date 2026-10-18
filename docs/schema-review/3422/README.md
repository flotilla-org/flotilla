# Interned string domains: owner schema checkpoint (#3422)

Status: proposed; awaiting the owner's look-over before implementation.

`fixtures.sql` is the complete prospective DDL snapshot, copied from
`crates/flotilla-schema-sqlite/tests/integration/snapshots/fixtures.sql` with only
`strings` reshaped. It is deliberately separate from the active snapshot and
runtime DDL: activating it requires changing interning writers, which is gated
on this review. After approval the active DDL and generated snapshot will adopt
this shape together. This draft is not ready to merge.

## Readable table view

`strings`: **3 columns** (previously 2), no enum expansion, no child tables.
No changed table exceeds 30 columns. All other tables and readable views in the
prospective snapshot are unchanged.

| Column | Storage | Meaning |
|---|---|---|
| `id` | INTEGER PRIMARY KEY | Globally unique local dictionary id, across domains |
| `domain` | TEXT NOT NULL | Nonempty short sort code, declared by the interning column |
| `s` | TEXT NOT NULL | Original text, including empty text when the type permits it |

The unique index is `(domain, s)`, replacing uniqueness on `s`. Equal text in
one domain shares an id; equal text in different domains has distinct ids.
There is no fallback/default domain. Domain codes are open-ended, not a SQL
CHECK list: new kinds may declare new sorts without rebuilding the dictionary.
The derive will require a nonempty domain declaration; short codes such as
`ad`, `wr`, and `cn` are a convention, not an arbitrary two-character SQL limit.

```diff
-CREATE TABLE strings (id INTEGER PRIMARY KEY, s TEXT NOT NULL UNIQUE) STRICT;
+CREATE TABLE strings (
+  id INTEGER PRIMARY KEY,
+  domain TEXT NOT NULL CHECK (length(domain) > 0),
+  s TEXT NOT NULL,
+  UNIQUE (domain, s)
+) STRICT;
```

## Intended implementation after approval

Each interned field will declare its sort, provisionally as
`#[store(intern = "ad")]`. Shared columns across enum variants must agree on
the domain, even if their Rust types differ; a mismatched or missing domain is
a compile error. The domain travels in the schema descriptor so the SQLite
adapter can use `(domain, text)` for both its lookup and cache keys. Codec
values, replicated events and resource documents continue to carry logical text.

The shared infrastructure also assigns domains explicitly: resource names
`rn`, homes `hm`, and log writers `wr`. Address fields use `ad`, conditions `cn`;
other field domains will be assigned by meaning when auditing the existing
intern annotations. Resource names and homes remain distinct sorts even when
their text happens to match a writer or an address.

Existing foreign keys continue to reference the globally unique `id`.
Readable kind views and `log_view` resolve text by that id, so two dictionary
rows containing the same text remain unambiguous. The unchanged view definitions
are included in the prospective snapshot. Compile-time domain checking protects
shared columns; this proposal does not add composite domain foreign keys to
every referencing table.

Per-domain counts and text byte sizes can be inspected with an aggregate over
`domain` (byte sizes use `length(CAST(s AS BLOB))`). These count text payload,
not SQLite page or index overhead. No statistics table or per-write counter is
proposed.

## Workload and evolution

The root WORKLOAD.md sizes change events at 1,000/s target, 10,000/s burst ceiling.
Interning occurs inside the existing local single-writer transaction; no new
network crossing or durable-store crossing is introduced. A cache miss uses the
unique `(domain, s)` index; id-to-text reads retain the primary-key lookup.
A uniqueness index on text alone fails domain isolation; per-domain tables would
lose one id space and require changing every reference and readable view.

Let N be distinct (domain, text) pairs and k be interned values per event.
Miss work is O(k log N) per event, at most 10,000*k indexed lookups per burst
second before accounting for cache hits. Dictionary disk storage grows with N
and text bytes; the domain is also present in the unique index. Cache entries
remain subject to the existing configured two-generation bounds; their keys
must include the domain. Statistics aggregate O(N) entries and should remain
an explicit diagnostic query, returning one row per domain, outside the event
path.

Named sizing gaps: distinct-string cardinality, average text bytes, domains per
text, k, and cache hit rate are not specified by WORKLOAD.md. No memory, disk, or
latency fit is claimed without those measurements. The dictionary already keeps
strings indefinitely; this change does not resolve its retention ceiling.
Benchmarking after approval must include repeated text across domains and
bounded-cache misses rather than assuming domain separation is free.

The issue explicitly gates this before store cut-over: no production data in
these tables needs migration. Existing deployed resource serialization and the
Quint specs are unchanged. Once this store is deployed, ADR 0047's stored-data
compatibility rule applies; this proposal is not a migration for deployed stores.
