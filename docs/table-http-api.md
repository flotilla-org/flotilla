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
  name. The primary key is appended to make the order total. Null sorts first
  ascending and last descending; strings compare by Unicode code point.
- `columns=spec,status` projects columns. Primary key columns always return;
  the description contains exactly the returned columns.
- `count=exact` or `count=estimate` opts into a count. `partial` marks a page
  that does not represent the whole result.
- Repeated `where` parameters are ANDed and use ADR 0066's condition grammar,
  for example `.metadata.name == example` or
  `.metadata.labels."flotilla.work/project" == flotilla` (URL encode these).
  Today's generic resource tier pushes down equality on declared indexed fields
  and labels. Other operators and unindexed columns return `unsupported_filter`;
  unknown columns and incompatible literals have separate errors. No filter is
  implemented by listing and filtering in the router.

Descriptions use Frictionless Table Schema v2 with native JSON/CBOR values,
including `sortable` and `whereOperators` field properties. `format=markdown`
(or `Accept: text/markdown`) renders the schema through `render_view`.
For a derived kind with a machine, `format=mermaid` renders the declared machine;
`format=observed` renders counts from its log, optionally restricted with `name`.
