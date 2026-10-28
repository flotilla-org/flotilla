# Tables

flotillad serves its resource kinds and engine kinds as self-describing tables. A client reads a table's description, then a stream of row messages.

Fenced blocks tagged `list <name>` below (routes, messages, column types) are checked against the code by a test.

## Routes

```list routes
/tables
/tables/{table}
/tables/{table}/rows
```

All are `GET`. The caller must be a user (socket, bearer or session); anonymous callers get `401`, environment and hook tokens get `403`, code `unauthorized`.

- `/tables` lists the kinds: `{"kinds":[{"kind","root","tables":[...]}]}`. `root` is the kind's root table; `tables` are the names usable in the other two routes. Engine kinds are listed only when the daemon has its store.
- `/tables/{table}` describes one table (below).
- `/tables/{table}/rows` streams row messages.

## Formats

A format comes from `?format=` or, when that is absent, from `Accept` (media ranges with `q` values; the first listed type below wins a tie, and an empty `Accept` selects the first).

| Route | `?format=` | `Accept` / response type |
|---|---|---|
| `/tables` | ignored; use `Accept` | `application/json`, `application/cbor` |
| `/tables/{table}` | `json`, `jsonl`, `cbor`, `markdown`, `mermaid`, `observed` | `application/json`, `application/cbor`, `text/markdown`, `text/vnd.mermaid` |
| `/tables/{table}/rows` | `json`, `jsonl`, `cbor` | `application/x-ndjson`, `application/json`, `application/jsonl`, `application/cbor-seq` |

- Rows are one message per line (JSON Lines; `\n`-terminated) or a CBOR sequence of independently encoded messages. `format=json` and `format=jsonl` both give `application/x-ndjson` on the rows route.
- In JSON, integers beyond ±2^53 are written as `{"u64":"…"}` or `{"i64":"…"}`. CBOR carries them natively.
- `?format=markdown` on a table describes its own columns. `mermaid` is the kind's declared state machine; `observed` is the transitions seen so far (`namespace` and `name` select the scope and a record). Both exist only for engine kinds; others answer `no_machine`.
- Responses carry `Cache-Control: no-store`.

## Errors

An error is an object `{"code","message"}`, JSON or CBOR to match the request.

| Status | Codes |
|---|---|
| 401, 403 | `unauthorized` |
| 404 | `not_found`, `no_machine` |
| 406 | `unsupported_format` |
| 500 | `table_source` |
| 400 | `invalid_filter`, `unsupported_filter`, `unknown_column`, `type_mismatch`, `unsupported_sort`, `invalid_sort`, `invalid_paging`, `invalid_count`, `unsupported_watch`, `unsupported_log_paging`, `invalid_request` |

Errors found while a stream is already open arrive as a `resync` message instead.

## Query parameters

Parameters apply to the rows route. Each is validated against the table's description before any row is read.

- `where=<field> <op> <literal>`, repeatable, all conditions must hold. The expression is the condition grammar of `flotilla-address` (`crates/flotilla-address/src/condition.rs`) with the field path and literal; it is not copied here. A column accepts an operator only if it appears in the column's `whereOperators`. Literals must fit the column type: integers parse as `i64`, booleans are `true`/`false`, timestamps are RFC 3339, `code` columns take one of their values. `cbor`, `frozen` and `blob` columns cannot be filtered. On resource kinds, `metadata.labels.<key>` filters by label with `==`.
- `sort=a,-b`: comma-separated column names, `-` for descending. Columns must be `sortable`. The table's key columns are appended in ascending order, so the order is total. Null sorts before every value ascending, and after every value descending.
- `columns=a,b`: the columns to return. Key columns are always included, and a `reference` column brings its `.kind` and `.home` companions.
- `limit`: rows per page; 200 when absent, otherwise 1 to 10000 and `offset` (default 0).
- `cursor`: the `cursor` of the previous `rows` message when it was not null. It is opaque, and bound to the table, `namespace`, `sort` and `where` of the query that produced it; a cursor used with a different query yields a `resync` message instead of rows. Paging is keyset: rows strictly after the cursor's sort values are returned, so inserts and deletes between pages do not shift it.
- `count=exact|estimate`: adds a `count` message.
- `namespace` (default `default`): the scope read. Flotilla's own records live in the `flotilla` namespace, so a client that omits `namespace` gets empty tables and no error; pass `namespace=flotilla`. For engine kinds, a namespace other than `default` names the home.
- `watch` (bare, or `true`): keep the stream open and send changes.

`.log` tables accept no `watch`, `cursor`, `offset`, `count` or custom `sort`; they return a bounded snapshot in `home, sequence` order.

## Messages

```list messages
table
rows
synced
upsert
delete
progress
resync
count
```

Every message is an object with a `type` field holding one of the names above. `sequence` is the source position the message describes.

| `type` | Other fields | Meaning |
|---|---|---|
| `table` | `table`, `schema`, `incarnation`, `sequence` | The description (below), limited to the requested `columns`. |
| `count` | `count`, `estimate`, `sequence` | Only when `count` was requested. |
| `rows` | `rows`, `sequence`, `partial`, `cursor` | A batch of rows keyed by column name. `partial` is true when this response is not the whole table: more rows follow, or the request gave `offset` or `cursor`. A client pages on `cursor`, not `partial`: a non-null `cursor` on the last batch of a page means more pages, and null means there are none. |
| `synced` | `sequence` | The snapshot is complete. |
| `upsert` | `key`, `row`, `sequence` | A row appeared or changed. `key` is the values of the primary key columns. |
| `delete` | `key`, `sequence` | A row disappeared. |
| `progress` | `sequence` | Nothing changed; sent at least every 15 seconds on a watch. |
| `resync` | `reason` | Discard what you hold. Sent for a bad cursor (followed by a fresh page), a changed `incarnation` (followed by a fresh snapshot), or a source failure. |

Order: `[resync]` (bad cursor only), `table`, `[count]`, one or more `rows`, `synced`. With `watch`, afterwards any number of `upsert`, `delete`, `count` and `progress` messages; a change that alters no projected row produces `progress`. After a `resync` for a changed `incarnation`, a fresh `table`, `rows`, `synced` sequence follows. After a source failure the server retries, up to three times, and then ends the stream.

## Description

`/tables/{table}` returns the same document as the `schema` of the `table` message, a Frictionless-style table schema:

```json
{
  "fields": [ { "name": "…", "type": "…", "constraints": {"required": true}, … } ],
  "primaryKey": ["…"],
  "foreignKeys": [ { "fields": ["…"], "reference": {"resource": "…", "fields": ["…"]} } ]
}
```

It is built from `TableSet` (one kind's tables) and `Table`/`ColumnDef` in `crates/flotilla-schema/src/descriptor.rs`. Key columns come first in `fields`, in `primaryKey` order.

Each field has:

- `name`: dotted for nested values, for example `metadata.name`.
- `type`: the wire type, from the column type (below).
- `constraints.required`: false when the column is nullable.
- `format`: `binary` for `blob` columns (base64 strings in rows).
- `categories`: for `code` columns, `[{"value","label"}]`, in code order.
- every `properties` entry of the column, merged into the field.

### Column types

```list column-types
integer
number
bool
text
blob
timestamp
code
interned
ref
frozen
cbor
```

| `ColumnType` | Field `type` | Cell in a row |
|---|---|---|
| `integer` | `integer` | number |
| `number` | `number` | number |
| `bool` | `boolean` | boolean |
| `text` | `string` | string |
| `blob` | `string`, `format: binary` | base64 string |
| `timestamp` | `datetime` | RFC 3339 string |
| `code` | `string` plus `categories` | one of the category values |
| `interned` | `string` | string |
| `ref` | `string` | string, a resource name |
| `frozen` | `object` | decoded value |
| `cbor` | `object` | decoded value |

Cells are absent-as-`null` for nullable columns.

### References and keys

A column with a `reference` produces a `foreignKeys` entry naming the target table. The table lists extra text columns for the target's natural key: `<column>.home`, and `<column>.kind` when the target is any resource (`resource_identity`). Cells never carry store-local ids. Tables of engine kinds are keyed by `resource.home` and `resource.name` ahead of their own key.

### Properties in use

```text
roles
title
sortable
whereOperators
```

- `roles`: an array of strings. In use: `key` (part of the primary key), `ref` (identifies a resource), `state` (a state label), `since` (when a state was entered), `sequence` (log position), `time` (an event time), `transition-from` and `transition-to` (the states an event moved between).
- `title`: a display name.
- `sortable`: whether `sort` accepts the column.
- `whereOperators`: the operators `where` accepts on the column; an empty array means it cannot be filtered. Scalar columns of engine kinds take `==`, `!=`, `<`, `<=`, `>`, `>=`. Resource kinds take `==` on the columns their kind declares queryable.

## Which tables exist

`GET /tables` is the authority. The tables are:

- Resource kinds: one table each, named by the kind's plural, with the shared columns `metadata.*`, `status.phase`, `status.conditions`, `spec` and `status`.
- Engine kinds: a root table per kind, a table per nested collection, and `<root>.log`, the kind's event log projection with the columns `home`, `sequence`, `name`, `cause`, `edge`, `from`, `to`, `at`.

The columns are not listed here; `?format=markdown` on a table describes its own.

## Not built yet

Named queries as tables and a WebSocket carrier (#3249), parent keys on child tables (#3600) and view subscriptions (#3447).
