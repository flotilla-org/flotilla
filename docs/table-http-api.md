# Table reads over HTTP

The daemon router serves `/tables`, `/tables/{table}` (the description), and
`/tables/{table}/rows`. These share the resource API's caller authentication;
an environment or unauthenticated caller cannot read them. The optional loopback
listener retains its Host and Origin policy.

For example, with the fleet host's configured bearer token:

```sh
curl -H "Authorization: Bearer $TOKEN" http://127.0.0.1:7878/tables
curl -H "Authorization: Bearer $TOKEN" \
  'http://127.0.0.1:7878/tables/convoys/rows?namespace=flotilla&limit=20&count=exact'
```

Resource table names are their resource plurals. When primitives are enabled,
the decided store also lists its derived kinds grouped by kind, using their
existing table names. `namespace` selects a resource namespace, or a primitive
home (default: the store's own home for primitives, `default` for resources).

Rows use `application/x-ndjson` by default. `Accept: application/cbor-seq`
selects an RFC 8742 CBOR sequence, with no length prefix or stack frame header.
`format=jsonl` or `format=cbor` overrides Accept. The shared vocabulary and
CDDL live in `flotilla-protocol/src/table.rs` and `table.cddl`.
Messages are capped at 16 MiB; snapshots split row batches around 512 KiB.

Every snapshot sends `table`, optional `count`, `rows`, and `synced`. Snapshot
batches and pages carry the store sequence they reflect. `watch` or `watch=true`
keeps the body open for `upsert`, `delete`, `progress`, and count changes.

- `limit` defaults to 200 and is capped at 10,000.
- `cursor` continues from the last sort values and natural primary key. It is
  opaque deterministic CBOR encoded as unpadded base64url. An unusable cursor
  sends `resync` followed by a fresh snapshot. Store-local identifiers never
  appear in cursors or row keys.
- `offset=N` jumps to a position and returns a normal continuation cursor.
- `sort=-status.phase,metadata.name` requests descending phase, then ascending
  name. Resource versions remain opaque strings and cannot be sorted.
  The primary key is appended to make the order total. Null sorts first
  ascending and last descending; strings compare by Unicode code point.
- `columns=spec,status` projects columns. Primary key columns always return;
  the description contains exactly the returned columns.
- `count=exact` or `count=estimate` opts into a count. `partial` marks a page
  that does not represent the whole result.
- Repeated `where` parameters are ANDed and use ADR 0066's condition grammar,
  for example `.metadata.name == example` or
  `.metadata.labels."flotilla.work/project" == flotilla` (URL encode these).
  Today's generic resource tier pushes down equality on declared queryable fields
  and labels. Other operators and unindexed columns return `unsupported_filter`;
  unknown columns and incompatible literals have separate errors. No filter is
  implemented by listing and filtering in the router.

Descriptions use Frictionless Table Schema v2 with native JSON/CBOR values,
including `sortable` and `whereOperators` field properties. `format=markdown`
(or `Accept: text/markdown`) renders the schema through `render_view`.
For a derived kind with a machine, `format=mermaid` renders the declared machine;
`format=observed` renders counts from its log, optionally restricted with `name`.

A watch follows the **whole filtered table** after its initial page. `limit`,
`cursor`, and `offset` bound the initial snapshot page; later upserts and deletes
may name rows outside that page. A position-aware live window is a view
subscription (ADR 0067 decision 15), which is outside this HTTP table carrier.

This first resource tier is intended for operational inventories that fit in the
daemon's memory. Filtering is pushed down, but sorting and paging materialize the
whole matching set. The page limit bounds response rows, not query work. Each
resource watch reconciles O(n) matching rows per consumed event. Primitive watches
check the home-wide sequence every 250 ms, so another kind's write can trigger a
read; SQLite work runs on blocking workers. Large inventories need a backend
ordered-query/notification port before this becomes a bulk export surface.
Batch sizing currently measures JSON bytes for both carriers to keep consistent
boundaries; CBOR then encodes that batch independently. This bounded extra encoding
cost is accepted for this operational tier.

Primitive table reads hold the engine store mutex while SQLite reads and decodes
rows, on a blocking worker. Non-log watches poll the home-wide sequence as described
above. Log watches return `unsupported_watch`; incremental log watching is deferred.

A `{kind}.log` row request returns only a bounded initial snapshot in ascending
sequence order. The SQL query applies `LIMIT` before materializing rows (the requested
limit plus one sentinel, capped at 10,001). The response marks truncation as partial,
without offering a continuation cursor. Cursor, offset, alternate sort and count
requests return `unsupported_log_paging`; full keyset paging is deferred. This bounds
returned history rows while the mutex is held; it does not promise index-bounded
query execution for arbitrary filters.

Primitive filters run in SQL, but predicates on resolved interned/reference
aliases are not promised to use column indexes. Resource-tier read-observer checks
prove use of the resource-store query API separately. Blob cells render as base64
strings in both carriers. CBOR and frozen object cells decode to their native JSON
values before either carrier encodes them. These blob/object columns cannot be
sorted or filtered.


Conditions are parsed into a neutral ordered list before a source lowers them. The
resource source supports equality; the decided source advertises and lowers `==`,
`!=`, `<`, `<=`, `>` and `>=` on scalar fields. Repeated fields remain separate
clauses, so `.number >= 1` and `.number <= 5` form a range. Bounded log reads can
use `.sequence > 500` without enabling cursor-based log paging.

Reference columns use schema-level `foreignKeys`. Each typed reference returns its
name plus a companion `<field>.home`; together these name the target table's full
`(resource.home, resource.name)` key. Untyped identity references also include
`<field>.kind`. Projecting a reference includes its key companions. The same-name
row on another home remains distinguishable. There is no field-level `reference`
extension. Markdown reads the descriptor directly, including semantic properties.

The HTTP routes are three axum GET routes over `server::tables::TableSource`. The
carrier-neutral module owns paging, projection and live diffs for both sources.
Source errors after the initial snapshot produce `resync` in-band and a bounded
resnapshot attempt, rather than silently truncating the body. Changes outside the
projection do not emit an upsert. Diagram renderings come from the engine-store
machine registration, which pairs a kind with its diagram once.

Commands and tables share Accept negotiation, error rendering, no-store headers and
CBOR limits. Media types are case-insensitive, whitespace around `q =` is accepted,
and invalid qualities or excluded formats return 406. Errors use the negotiated
JSON/CBOR encoding with `code` and `message`. JSON integers outside the browser-safe
range use the shared `{u64: "..."}` / `{i64: "..."}` profile; CBOR retains full 64-bit
integer values. The table protocol decoder accepts tagged envelope integers.

Cell profile decision for this version: cells and schemas remain JSON-shaped in
both carriers, including base64 blob strings. Native CBOR byte-string cells would
require a versioned profile, rather than silently changing #3249's frozen frame
contents. WebSocket request/subscription ids belong in that carrier's envelope,
not in each table message. Deferred optional model and validation work is tracked
in #3614; scale work, including observed-diagram aggregation, is tracked in #3601.
