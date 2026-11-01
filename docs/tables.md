# Tables

flotillad serves its resource kinds and engine kinds as self-describing tables. A client reads a table's description, then a stream of row messages.

Fenced blocks tagged `list <name>` below (routes, messages, column types, `where` operators) are checked against the code by a test.

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

- `where=<condition>`, repeatable; every condition must hold. A condition is an expression in the condition language ([Conditions](#conditions)), URL-encoded, such as `where=inbox.under("project:flotilla/convoy:fix-3774")`.
- `sort=a,-b`: comma-separated column names, `-` for descending. Columns must be `sortable`. The table's key columns are appended in ascending order, so the order is total. Null sorts before every value ascending, and after every value descending.
- `columns=a,b`: the columns to return. Key columns are always included, and a `reference` column brings its `.kind` and `.home` companions.
- `limit`: rows per page; 200 when absent, otherwise 1 to 10000.
- `offset`: rows to skip (default 0).
- `cursor`: the `cursor` of the previous `rows` message when it was not null. It is opaque, and bound to the table, `namespace`, `sort` and the canonical text of `where` of the query that produced it, so another spelling of the same conditions continues it; a cursor used with a different query, or one issued before conditions took this language, yields a `resync` message instead of rows. Paging is keyset: rows strictly after the cursor's sort values are returned, so inserts and deletes between pages do not shift it.
- `count=exact|estimate`: adds a `count` message.
- `namespace` (default `default`): the scope read. Flotilla's own records live in the `flotilla` namespace, so a client that omits `namespace` gets empty tables and no error; pass `namespace=flotilla`. For engine kinds, a namespace other than `default` names the home.
- `watch` (bare, or `true`): keep the stream open and send changes.

`.log` tables accept no `watch`, `cursor`, `offset`, `count` or custom `sort`; they return a bounded snapshot in `home, sequence` order.

## Conditions

`where` speaks one condition language, a strict subset of [CEL](https://github.com/google/cel-spec) implemented by `rigging-condition`. Its grammar, canonical text, three-valued evaluation and SQL translation are defined there: see the crate's [README](https://github.com/flotilla-org/rigging/blob/main/crates/rigging-condition/README.md) and its conformance corpus, [`corpus/corpus.json`](https://github.com/flotilla-org/rigging/blob/main/crates/rigging-condition/corpus/corpus.json), which every implementation runs. The design is [the condition language note](research/2026-10-31-condition-expression-language.md).

```text
inbox.under("project:flotilla/convoy:fix-3774")
phase in ["Running", "Landing"] && !has(`closed-at`)
`admitted_seq` > 10 || state == "Settled"
"kind:bug" in metadata.labels
```

- **Fields** are the table's columns. A column's field path is its name: `metadata.name`, with a segment that is not an identifier backticked (`` `closed-at` ``). Where another filterable column's name is a dotted prefix of it, the whole name is one backticked segment: a reference's `` `reference.home` `` beside `reference`.
- **Literals** are typed by the column: strings are double-quoted (`"Landed"`, never a bare word), integers and floats are plain, booleans are `true`/`false`, and timestamps are `timestamp("2026-10-01T00:00:00Z")` with at most six fractional digits. An enum column takes one of its `constraints.enum` names; an address column takes canonical ADR 0066 address text.
- **A missing value** (a null cell) makes every comparison that reads it unknown, so `!=` does not keep it; test presence with `has(field)`. `&&`, `||` and `!` are three-valued, and a row is kept only when its condition is true.
- **`under`** holds when the address equals the literal or lies below it at a segment boundary: `…/convoy:demo2` is not under `…/convoy:demo`, and an encoded `/` (`repo:org%2Frepo`) is no boundary. On a stored address the store searches its interned strings by range.
- Any CEL spelling of a condition is accepted (whitespace, single quotes, comments); the canonical print is what a cursor binds. The former spelling, `.field op literal`, is refused.

Each field's `whereOperators` lists what its type allows; an empty array means the column cannot be filtered (`cbor`, `frozen` and `blob` columns).

```list where-operators
integer, number, string, datetime: == != < <= > >= in has
boolean: == != has
enum: == != in has
address: == != in under has
map: in
```

- `in` takes a non-empty list of up to 32 literals: `state in ["Pending", "Awaiting"]`. On a map it is `"key" in field`.
- A bare boolean field is a condition: `done`, `!done`.
- On resource kinds, `metadata.labels` is a map: `"k" in metadata.labels` tests a label's presence and `metadata.labels["k"] == "v"` its value; its `additionalProperties` carry the string operators. Label and indexed field equalities are read through the store's indexes, and the rest of the condition is evaluated over the rows the store returns.

Errors: a condition that does not parse, exceeds a limit (32 terms in one `&&` or `||` run, 32 list elements, 12 nested parentheses or `!`, 4,096 bytes), or names a bare word where a literal belongs, is `invalid_filter`; a field the table does not have is `unknown_column`, and one it cannot filter is `unsupported_filter`; a literal of the wrong type, an enum name outside the set, a non-canonical address, or ordering (`<`, `>`, …) on a boolean, enum or address is `type_mismatch`.

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
- `constraints`: the Frictionless constraints below.
- `format`: `binary` for `blob` columns (base64 strings in rows).
- every `properties` entry of the column, merged into the field.

### Field types

```list field-types
integer
number
boolean
string
datetime
object
address
```

All but `address` are Frictionless types. `address` is a flotilla extension: the column holds canonical ADR 0066 addresses (`crates/flotilla-address`), as text. Address columns are the `interned` columns whose values are addresses; a condition shares their storage but is a `string`.

### Constraints

```list constraints
required
enum
```

- `required`: false when the column is nullable.
- `enum`: on an enum (`code`) column, the closed set of values a cell may hold: the readable names (`Pending`, `Landed`), in code order. A null cell is allowed only when `required` is false. Rows never carry the short codes that storage and SQL use (ADR 0047: the spelling is wire only).

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
| `code` | `string` plus `constraints.enum` | one of the enum's names |
| `interned` | `string`, or `address` for an address | string |
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
- `whereOperators`: the operators `where` accepts on the column, by its type ([Conditions](#conditions)); an empty array means it cannot be filtered.

## Which tables exist

`GET /tables` is the authority. The tables are:

- Resource kinds: one table each, named by the kind's plural, with the shared columns `metadata.*`, `status.phase`, `status.conditions`, `spec` and `status`.
- Engine kinds: a root table per kind, a table per nested collection, and `<root>.log`, the kind's event log projection with the columns `home`, `sequence`, `name`, `cause`, `edge`, `from`, `to`, `at`. `cause` (`Create`, `Edit`, `Edge`, `Delete`, `Expunge`), `edge` (the kind's edge names) and `from` and `to` (its state's labels) are enum columns.

The columns are not listed here; `?format=markdown` on a table describes its own.

## Reading a table from the CLI

`flotilla table <name>` renders a table through [binnacle](https://github.com/flotilla-org/binnacle), reading `/tables/<name>/rows?watch&namespace=flotilla` over this host's daemon socket (`--namespace` reads another). `flotilla table` with no name lists the tables.

- Piped, or with `--no-interactive`, it prints aligned text (`--format text`, the default), `tsv`, `json` or `jsonl`.
- In a terminal it opens binnacle's grid inline below the prompt, live: sort, filter, find and column choice are binnacle's. `q` or Esc leaves; Enter leaves and prints the chosen row's key. The rows shown stay in the scrollback.
- `--columns`, `--sort`, `--where`, `--wide` and `--view` adjust the view. `--where` filters the rows already read, not the server's read.

A table's built-in view is a binnacle Layout document (`version: 2`) under `views/`, named `<kind>.<name>.json`; `views/convoy.default.json` is the first. The binary includes these files, and a table without one shows binnacle's default view: the nested columns hidden and the key and state columns first.

## Not built yet

Named queries as tables and a WebSocket carrier (#3249), parent keys on child tables (#3600) and view subscriptions (#3447).
