# 67. Tables describe themselves on the wire

**Status:** Accepted, not yet implemented (owner rulings, 2026-10-16 to 2026-10-17); amended 2026-10-17 after binnacle's review (#3427, see decisions 15 and 16)
**Date:** 2026-10-17
**Amends:** [ADR 0012](0012-tables-are-the-consumer-side-view-layer.md): "layout never lives in the store and never crosses the wire" becomes "meaning crosses the wire; layout does not".
**Relates to:**
- [ADR 0063](0063-cbor-is-the-stack-binary-format.md): CBOR is the stack's binary format;
- [ADR 0064](0064-primitives-model-resources-records-gated-machines-inboxes.md): records, gated machines and the named-query surface protocol;
- [ADR 0065](0065-flotillad-serves-one-http-server.md): one HTTP router, the loopback TCP listener and the WebSocket surface channel;
- [ADR 0066](0066-addresses-name-things-by-natural-key.md): natural-key addresses and the condition grammar;
- flotilla-org/binnacle#47 (the Table Source interface in TypeScript), #3319 (the engine and storage-schema stack), #3248 and #3249 (ADR 0065's router and surface protocol).

## Context

While most crews are dummies, the owner wants an easy way to look at the daemon's data to build confidence that it behaves. That needs three things: an HTTP listener, simple routes that serve the underlying data, and a small inspector page. The inspector is meant to grow into a generic viewer, built on binnacle's grid, that replaces much of the TUI and is embedded in Wheelhouse.

A generic viewer needs tables that describe themselves, so no viewer carries code for one particular table. The same form should serve:
- stored data;
- live views of derived things (the named queries);
- historical records (edge history);
- flotilla's view of outside things, such as GitHub issues.

What exists:
- **The resource HTTP routes** (`crates/flotilla-daemon/src/server/resource_http.rs`) serve Kubernetes-shaped JSON lists, gets and NDJSON watches, with label and field selectors. They have no paging, no CBOR and no description of columns.
- **The named queries** (`crates/flotilla-protocol/src/result_set.rs`) send typed Rust rows. Each surface knows the row types and writes its own column specs (`crates/flotilla-tui/src/table_view.rs`).
- **The schema derive** (the #3319 stack) already describes each stored kind as a runtime value in flotilla-schema: `TableSet`, `Table` and `ColumnDef`. It has no SQLite dependency, and its row change log is close to a usable stream of changes. It drops enum variant names, does not say which kind a reference points at, has no place for extra column properties, and still holds the SQL rendering.
- **binnacle's grid** never reads values. Its column names and renderers are compile-time adapter traits. Its server-backed Table Source (binnacle#47) is not designed yet.

## Decision

### 1. Meaning crosses the wire; layout does not (amends ADR 0012)

A table's description carries what its columns mean: names, types, the key, references to other tables, enum variant names and roles. Widths, column order, hidden columns, user labels, colours and themes stay in the viewer. Presentation hints, such as a priority for narrow screens or a preferred view, may come later as hints a viewer is free to ignore. They are not part of the first version. The one exception is a default label: Frictionless's optional `title` may carry one now (decision 16). Units are meaning, not presentation (decision 16). The store never holds presentation.

### 2. The description is Frictionless Table Schema v2; the stream is ours

- **The column description adopts [Frictionless Table Schema v2](https://datapackage.org/standard/table-schema/)**: `fields`, `primaryKey`, `foreignKeys` (whose `reference.resource` names the target table), and `categories` as `{value, label}` for enum codes. Field descriptors allow extra properties, which is where roles go.
- **flotilla uses a profile of it.** Values are native JSON and CBOR values. The properties that describe parsing text, such as `trueValues`, `groupChar` and `decimalChar`, are not used.
- **Column types map directly:** Integer, Bool and Text to integer, boolean and string; Timestamp to datetime; Blob to string with format binary; Frozen and Cbor to object; Code to string with categories (stored codes are short explicit text, #3392 ruling G7); Ref to a foreign key.
- **The stream envelope is flotilla's own** (decision 8), because no standard covers live changes.
- Rejected: the Kubernetes Table (thin types, string-like cells, no keys or references); Grafana data frames (tied to Grafana; flotilla borrows its idea of explicit roles); Arrow (kept for a later bulk export, decision 11).

### 3. One description model: the derive's descriptor

- **The schema derive's descriptor is the single description model.** Every rendering is produced from it: the SQL DDL, Frictionless JSON, the Markdown schema view and mermaid.
- **The SQL moves out** of flotilla-schema into flotilla-schema-sqlite (`ddl.rs`, `Guard::check`, `Condition::sql`). flotilla-schema stays free of storage.
- **The descriptor gains:** enum variant names on Code columns, the target table on each Ref, an open slot for extra field properties, the structural roles (decision 6), and a builder for sources that are not derived kinds.
- **This work is not part of the #3319 stack.** It lands in its own PRs. If the stack has not merged when the work starts, those PRs sit on top of it. Mapping B remains the engine crew's work.
- Rejected: a separate wire-only model with a converter, which would be a second model edited in step with the first.

### 4. One wire table per stored table

- **Each stored table is its own wire table:** the root, each child table, records, and the per-home event `log` (#3392, #3407), which replaced the per-kind history tables. Child rows carry the parent's key as a foreign key.
- **A viewer drills by asking for the child table filtered to the parent's key** (decision 12). This is ADR 0012's navigation stack, and it matches binnacle's flat grid.
- **The listing of tables groups them by kind,** as `TableSet` does, naming the root.
- **A collection change is one row change on its child table,** never a re-send of the parent.
- Rejected: nesting child rows inside parent rows, which falls back to an untyped `object` column and loses the description.

### 5. Store ids never cross the wire

- **An interned column crosses as its text** and is described as a `string`. Interning is a storage choice, not part of a column's meaning.
- **A reference crosses as the target's natural key** (its record name or ADR 0066 address), described by `foreignKeys`.
- **Store-local ids never leave the store.** They mean nothing on another host or after a rebuild.
- WebSocket compression (ADR 0065) handles repeated strings. If a measured stream shows that is not enough, a per-stream dictionary can be added inside the stream envelope as an encoding choice; the description still says `string`.

### 6. Roles come from structure first, then from attributes

- **The first version ships roles the descriptor already knows:** key, ref, state, since, transition from and to, sequence, and time. The derive sets them where it generates those columns (mapping B's state label and `state_since`, the `log` table's edge `from` and `to` columns, keys and references). They travel in the field-property slot.
- **Hand-written role attributes come next,** for example a time or graph-edge marker on a field, followed by presentation hints (decision 1). Both are additive.
- **Roles are never guessed from column names.**

### 7. Named queries describe themselves through the derive

- **Named queries become self-describing tables,** served through the same routes and stream messages as stored tables.
- **The derive gains a describe-only mode.** It runs the same flattening on a struct that is never stored and produces only the descriptor. It is applied to the named queries' existing row structs (`IssueRow`, `CheckoutRow`, `IndependentRow`, `ProjectRepositoriesRow`, `StandingRoleRow`, the Awareness types). The query fills those structs as it does today.
- **The derive's attributes split into shape and storage.** Shape covers child, intern and code; storage covers records, history and frozen. `#[store(...)]` does not read correctly on a struct that is never stored.
- **flotilla-protocol depends on flotilla-schema.** That is acceptable because flotilla-schema stays free of storage (decision 3), and because flotilla-protocol is on its way to becoming HTTP routes and messages.
- **The TUI moves over gradually.** The typed rows remain the in-process Rust type meanwhile.

### 8. One message vocabulary, two carriers

**The messages:**

| Message | Meaning |
|---|---|
| `table` | the Frictionless description, plus the incarnation and sequence it starts from |
| `rows` | a batch of rows; a large snapshot is several batches |
| `synced` | the snapshot is complete, and live changes follow |
| `upsert`, `delete` | one row change, carrying its sequence number |
| `progress` | a heartbeat carrying the current sequence, so a quiet stream proves it is alive |
| `resync` | the server cannot continue from the client's cursor; a fresh `table`, `rows` and `synced` follows |

**The carriers:**
- **The ADR 0065 surface WebSocket,** for live viewers. A client subscribes to a table exactly as it subscribes to a named query, with the same query cursors, incarnation rule and one CBOR map per frame. ADR 0065's "full result set" is `table`, `rows` and `synced`; its "numbered deltas" are `upsert` and `delete`; a stale cursor gets `resync`. This refines #3249.
- **Plain HTTP GET,** for one-shot reads, curl, scripts and agents. A GET returns `table`, `rows` and `synced` as a CBOR sequence (RFC 8742) or JSON Lines, chosen by `Accept`. `?watch` keeps the stream open with the live messages.

Rejected: a separate table-streaming protocol beside the surface channel, which would bring back two ordering domains and two resume schemes.

### 9. Paging is keyset paging with portable cursors

- **Rows are paged by keyset** on the sort columns, then the primary key. The primary key breaks ties, so the order is total and stable.
- **The cursor is an opaque token** encoding the last row's sort values and primary key. It holds natural keys only, so it stays valid across hosts and restarts. A cursor the server cannot honour gets `resync`.
- **Rows that move between pages may be skipped or repeated.** If a row's sort value changes while a client pages, the keyset continues from the old position. This is the normal keyset trade-off and is accepted. A viewer that needs an exact picture subscribes to the stream instead, and a scrolling grid subscribes to a view (decision 15).
- **The derive's store-local `Cursor`** stays as the store's internal pager. The wire cursor is a separate type.
- **binnacle's `u64` Keys are made on the client.** A server-backed Table Source interns each row's primary key through binnacle's `KeyInterner`. Keys never cross the wire.
- **A request can choose its columns** (`?columns=`). The primary key always comes back. The `table` message lists only the columns sent. With no selection, every column comes back. Sorting by an unselected column still works, because the server builds the cursor from the row it read.

### 10. Counts and position jumps are opt-in

- **`?count=estimate`** returns a cheap approximate count from statistics or the store's row-count bookkeeping. **`?count=exact`** returns the real count and may scan.
- **`?offset=N`** serves a scrollbar jump. It is slow deep into a large table, and it returns a normal keyset cursor to continue from.
- **Partial results are always marked,** for example "200 of ~40,000".
- A server-side position index (materialised row numbers per sort order) is the later shape if a measured table makes offset jumps too slow. It would sit behind the same `?offset`, so viewers would not change.

### 11. Renderings are content negotiation on the same routes

Every rendering is chosen by `Accept`, or `?format=` for browsers and curl, and is generated from the description and roles, never from code for one table.

- **The declared diagram:** each kind's state machine as mermaid, from `MachineDef::mermaid`, at the kind's description route.
- **The observed diagram:** a kind's edge entries in the `log` table (the rows carrying the transition roles) rendered as a mermaid `stateDiagram` of the transitions that actually happened, with counts on each edge, for one record or for all of them. An observed edge missing from the declared diagram is a bug.
- **The schema view:** the Markdown view from `render_view`.
- **Arrow** (`application/vnd.apache.arrow.stream`) waits for a real bulk consumer.
- **HTML is not a server rendering.** The inspector is a viewer that reads JSON or CBOR.

### 12. Filters use the ADR 0066 condition grammar

- **Filter:** `?where=<field-path> <operator> <literal>`, the ADR 0066 condition grammar without its address part, because the route names the table. Repeated `where` clauses are ANDed. An unknown column or a type mismatch is refused with 400.
- **Sort:** `?sort=-state_since,name`, ascending unless prefixed with `-`. The primary key is appended as the tie-breaker.
- **Filters are pushed down to the store,** as SQL on SQLite and as index-aware lookups in memory. A clause the store cannot push down is refused with its own error code, distinct from an unknown column or a type mismatch, never answered by listing and filtering.
- **Drilling is a filter** on the parent key (decision 4).
- `labelSelector` stays on today's resource routes. OR, IN and text search are additions to the grammar, not to the routes.

### 13. Build order

0. This ADR.
1. **From main:** ADR 0065's router and loopback TCP listener (#3248), then table routes on plain HTTP GET (decisions 8 to 12). Their first source is today's resources, described through the builder: metadata, labels, phase and conditions as typed columns, and spec and status as `object` columns. This is ADR 0012's generic tier (`Resources { kind }`), now on the wire.
2. **In parallel, its own PRs on top of the #3319 stack or of main:** the descriptor work of decisions 3, 6, 7 and 11. Once both are in, the derived kinds plug into the routes.
3. **The inspector:** a page served by flotillad, built on binnacle's web grid, with a table list, a grid, drilling by foreign key, and a declared-versus-observed diagram panel. binnacle#47's server-backed source is designed against these routes.
4. The WebSocket carrier (#3249) reuses the same message vocabulary.

### 14. Table routes inherit ADR 0065's access rules

- **The table routes and the inspector sit behind ADR 0065's router.** They get its bearer token or browser cookie, its Host and Origin checks, and a TCP listener that binds loopback only and is off unless declared.
- **They serve a caller no more than the resource routes already serve that caller.** Any redaction a kind applies on the resource routes applies to its tables too. Neither column selection nor resolving interned text may get around it.

### 15. Grids subscribe to views, kept in position by the server (amendment, #3427)

binnacle's grid works by Position: it needs the count, the keys for a range of positions, where a given key now sits (Locate), and changes given as positions. Decision 8's live messages name only the row that changed, so a client cannot place a change in a large sorted table without holding the whole ordered key list. Keyset paging's skipped and repeated rows (decision 9) would show in a scrolling grid.

- **A view subscription sits beside the table subscription,** on the ADR 0065 WebSocket. A client subscribes to a view: a table, its `where` clauses, a sort and a window of positions. It moves the window as the user scrolls.
- **The server sends:** the view's count; the rows in the window; changes as positions (inserted at, removed at, moved from and to, and content changed for rows in the window); count changes; and answers to "what is the position of key K?".
- **The server keeps each live view as an in-memory ordered index** of its keys, a B-tree with subtree counts. It is built once from the store's ordered query, which an index can serve, and each change the view sees is one O(log n) update that yields its position. A 100,000-key view is a few megabytes and about 17 comparisons per change. The cost scales with the number of open views, not with the store. Positions are never stored per sort order: that would renumber rows on every insert for orders nobody is watching, and SQLite cannot find a key's position without counting.
- **Append-ordered views,** such as the `log` by sequence, insert at the end, so their index is trivial.
- **This is decision 10's "server-side position index"**, now the basis of live views rather than an optimisation added later. Decision 10's `?offset` and counts still serve GET.
- **The plain table subscription and GET stay** for small tables, curl, scripts, agents and replication-style consumers. A small table can still be fetched whole and sorted by binnacle's in-memory source.

### 16. Smaller points from binnacle's review (amendment, #3427)

1. **Snapshot consistency.** Every GET page and every `rows` batch carries the sequence number it reflects, so a client that mixes paged fetches with a live stream knows which changes to apply.
2. **Capabilities are described.** Field properties say whether a field can be sorted and which `where` operators it supports, so a viewer can disable a control up front rather than learn from a 400.
3. **Ordering is specified.** Nulls sort first, and text sorts by Unicode code point (SQLite's `BINARY` ordering), so a browser sort and a server sort of the same table agree.
4. **Composite keys have one encoding:** the ADR 0063 deterministic CBOR of the key tuple. Equal keys always intern to the same binnacle Key.
5. **A `count` message** carries a changed count, including corrections to an estimate (decision 10).
6. **Units are meaning.** A field property gives a value's unit as a UCUM code (for example `By` for bytes, `s` for seconds), so a generic viewer can align and format values.
7. **Default labels.** Frictionless's optional `title` may carry a column's default label now. A viewer may ignore it, and user settings override it. Priority and preferred view stay deferred (decision 1).
8. **Requests can be cancelled.** Every WebSocket request carries a request id, and a `cancel` message withdraws a superseded window or Locate request.
9. **Room for grouping and pivots.** A row's key is not assumed to be a stored primary key (group and subtotal rows are keyed by group paths), and a view's columns may change while it is live. No grouping or pivot work is planned; the vocabulary only avoids ruling it out.

## Consequences

- ADR 0012's per-surface column registries shrink to layout and actions. What a column means comes from the server.
- ADR 0012's planned generic tier (`QueryId::Resources { kind }`) is delivered as table routes rather than as a new named query.
- The `result_set.rs` module rule that presentation never appears on the wire still holds: only meaning crosses.
- flotilla-schema's role widens from storage to description. Storage renderings live in flotilla-schema-sqlite.
- binnacle#47's server-backed Table Source has a fixed contract to implement: the view subscription (decision 15) for large tables, and decisions 2, 8, 9, 10 and 16 throughout.
- flotilla-protocol gains a dependency on flotilla-schema (decision 7). The build-graph check and the CLAUDE.md crate table, which calls flotilla-protocol serde-only, change with the first PR that adds the dependency.
- Decisions 3, 6, 7 and 11 build on the unmerged #3319 stack's descriptor, and decisions 8 to 10 refine #3249. If either design shifts, this ADR is the place to amend.
- Outside data, such as GitHub issues, can use the builder or the describe-only derive and be served the same way.
