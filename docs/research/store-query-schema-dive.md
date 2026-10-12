# Resource store deep dive: schema, query paths, steady-state callers, live evidence

Research for flotilla-org/flotilla#2953. It was framed against the owner's target model (correction 2026-10-12):

- The daemon loads the vast majority of state at startup into typed, indexed in-memory structures.
- It keeps that state current from its own writes and from watch or replication events.
- It writes through to SQLite for durability.
- It queries the database only for cold data: history and archived records.

Under this model, a table scan followed by a filter on a steady-state path is a defect whether the filter runs in SQL or in Rust. #2953-style pushdown applies only to the cold set.

Sources:
- Code: `github/main` at `ca3bc781` (2026-10-12). Paths are relative to the repository root.
- Live evidence: an online `.backup` of kiwi's `~/.local/state/flotilla/resources.sqlite`, taken 2026-10-12 22:31, analysed read-only on the copy.
- Write-amplification probes ran on a second, scratch copy.

---

## 0. Summary

| # | Finding | Kind | Evidence |
|---|---------|------|----------|
| 1 | **Statements are never cached.** Every `execute`/`query_row`/`prepare` re-prepares, and each prepare of an upsert on `resource_objects`/`replica_objects` compiles every trigger on the table: about 93 value tables × 3 TEMP triggers, plus labels, owners and digest. | naive shape | Measured: an uncached replica upsert costs **2.16 ms**; the same statement cached costs **0.061 ms** (**35×**). Without the TEMP triggers it costs 0.28 ms. The single writer tops out at about 450 replica events/s on prepare alone. |
| 2 | **JSON is decoded on the reader and writer threads, about 5 passes per row.** The passes are `from_str`→`Value`, a throwaway typed decode to validate, `from_value` into `SerializedResource`, then `transcode` (`to_value`+`from_value`) into `T`. The 3 reader workers spend their time in serde, not SQL. | naive shape | Parse 2.2 ms/MB plus about 2.6 ms/MB per round trip. One `including_replicas::<Convoy>().list()` on kiwi is 4.9 MB, about 36 ms of CPU and about 26 MB of transient `Value` heap (heap/text = **5.4×**). |
| 3 | **Steady-state readers list and decode whole kinds per pass or per call, including history.** 94% of replica Convoys are Landed or Abandoned; 96% of Artifacts belong to finished or deleted convoys. Event delivery also reads the DB: `ReplicaReadResolver::watch` does a `get` per Definitions event and per self-origin replica event, and watch replay runs on the **writer**. | naive shape (resident-model defect) | Section 3. At the ceiling, a Convoy list is 23k–230k rows (0.5–4.8 GB). |
| 4 | **Write amplification per object write.** About 30 statements and 9–35 rows change per write, and the full body is stored twice (object plus event). The body is also re-parsed about 15 times inside SQLite (Convoy) (`flotilla_declared_values` runs a full serde parse per declared field), and 47 dead legacy expression indexes per table are still maintained. | naive shape | Measured WAL per write: Convoy replica **309 KB** (body 21 KB, 14×), local Convoy status **260 KB**, Host heartbeat **91 KB** (body 5.3 KB, 17×). At about 13 heartbeats/min that is about **1.7 GB/day** of WAL for one Host object. |
| 5 | **The event log is 70% of the file.** It keeps 1,024 full-body events per kind per namespace, whatever the object count, so the single live local Convoy carries 26 MB of history. Replica tombstones are never collected (6,415 since 2026-08-11). | size | `resource_events` = 61.9 MB of 88.6 MB. Live objects are 8.4 MB, and the resident subset is about 2.8 MB. |

**The resident set is small.** Loading every live row on kiwi (2,673 rows, 8.4 MB) takes **16 ms** of SQL and **19 ms** of `serde_json` parse. The resident working set, excluding finished convoys and their artifacts, is about 2.8 MB of JSON text. As `serde_json::Value` it would be about 15 MB; as typed structs, about 4–6 MB. At the ceiling (10× hosts), resident is about 34 MB of text, about 50 MB typed, and 0.1–0.25 s to load (parse alone, then the full decode chain). **The cold set is where the bytes are:** history (finished convoys, their artifacts, the event log and tombstones) is 5.6 MB of the 8.4 MB of live rows plus all 62 MB of events today. At 100× history it reaches 0.5–5 GB, which belongs on an archive host. Pushdown (#2953) applies only there, and only to a handful of query shapes.

**Recommended split:**

- **Quick wins (days):**
  - `prepare_cached` on every fixed SQL string, with the cache sized for the generated set.
  - Drop the 94 dead legacy expression indexes.
  - Stop the double typed decode on reads.
  - Move watch replay off the writer onto a reader.
  - Coalesce Host heartbeat writes.
  - Garbage-collect replica tombstones.
  - Make event retention per object, time-bounded, rather than 1,024 per kind.
- **Resident-state redesign (the main work; extends #3120's rpds shared generations):**
  - One typed in-memory store per kind, loaded at startup, keyed `(origin, name)`, with maintained indexes for the declared fields, labels and owners.
  - It is updated from the writer's own commits and from replication applies, and the DB becomes write-through.
  - Every steady-state caller in section 3 reads it; watch delivery and resync never touch SQLite.
  - The 190 value tables and the label/owner trigger tables then become redundant for resident kinds. Drop them, and the write amplification goes with them.
- **#2953 (cold-set pushdown and archive split):**
  - Retention moves finished convoys, their artifacts, old events and tombstones into history tables, and later to an archive host.
  - Declared SQL queries with proven indexes serve history (convoy by subject or time window, artifacts by convoy, message audit). `MessageQuery` is already the exemplar.
  - Narrow #2953's contract to this.

---

## 1. Schema

All schema is created in code in `crates/flotilla-store-sqlite/src/sqlite.rs` (`initialize_connection` at 395, `initialize_query_indexes` at 637) and `crates/flotilla-store-sqlite/src/sqlite/digest.rs` (`initialize_digests` at 40). There is no migration framework: `resource_store_migrations` holds name markers.

### 1.1 Tables

| Table | Key | Payload | kiwi rows | dbstat size |
|---|---|---|---|---|
| `resource_objects` (sqlite.rs:415) | (group, version, kind, ns, name) | `resource_version`, **`body_json TEXT`** (whole k8s object) | 426 | 0.97 MB + 52 KB autoindex |
| `replica_objects` (477) | (origin_root, group, version, kind, ns, name) | **`body_json TEXT`**, `last_synced_at TEXT` | 2,247 | 9.3 MB + 364 KB autoindex |
| `resource_events` (426) | (g, v, kind, ns, event_version) | `event_type`, **full `body_json` per event** | 12,627 | **61.9 MB** + 672 KB autoindex |
| `resource_tombstones` (525) | (g, v, kind, ns, name) | `body_json` | 28 | – |
| `replica_tombstones` (514) | (origin, g, v, kind, ns, name) | `last_synced_at` | **6,415** (since 2026-08-11, never collected) | 1.37 MB + 1.0 MB autoindex |
| `replica_cursors` (502) | (origin, g, v, kind, ns) | rv, generation, complete_prefix | 58 | – |
| `resource_sequences` / `resource_event_compaction` | (g, v, kind, ns) | counters | 28 / 12 | – |
| `digest_entries` / `digest_nodes` (digest.rs:44) | (origin, …, bucket[, name]) | rv / bucket hash (256 buckets) | 2,673 / 2,950 | 0.9 MB / 0.64 MB incl. autoindexes |
| `{resource,replica}_objects_labels` / `_owners` (sqlite.rs:656) | (g, v, kind, ns, name, origin, key) | label value / owner key | 221 + 1,969 / 165 + 1,537 | about 1.7 MB incl. lookups |
| **190 `*_{scalar_v3,collection_v2,…}_values` tables** (sqlite.rs:712) | (g, v, kind, ns, name, origin, value) | one row per declared field value | about 21k cells | 5.7 MB incl. indexes |
| decode quarantines, `field_ownership_violations` | | | 0 | |

Total file: 22,176 pages × 4 KiB = 90.8 MB (292 free pages). **Events are 70% of it.** All live object bodies together are 8.4 MB.

Every identity column repeats `group_name='flotilla.work'`, `version='v1'`, `kind`, and `namespace='flotilla'` as TEXT, in every table and every index. A value-table row is about 80 bytes of key for a value of a few bytes.

### 1.2 Indexes

- **498 indexes and 190 value tables**, created per declared queryable field (`ResourceKind::queryable_fields`), each **twice**: once for local objects, once for replicas.
- **Value tables** (`*_values`) hold a covering index `(g, v, kind, ns, value, name, origin)`. They are maintained by **TEMP triggers** that call the Rust UDF `flotilla_declared_values(body_json, path)` (sqlite.rs:375-393, 720-731). The UDF does `serde_json::from_str::<Value>(&body)` on the **whole body, once per declared field per write**. Convoy has 12 declared fields per table, TerminalSession 10, Checkout and CredentialGrant 6 each.
- **Legacy expression indexes.** There are 47 per table: `*_metadata_name` for every kind, plus `*_status_phase` for Checkout, Convoy, Message, TerminalSession and Vessel. They take the form `ON …(…, json_extract(body_json,'$.metadata.name'), name) WHERE kind='X' AND json_valid(body_json)` (`sqlite_master` on the copy; created by an earlier migration and never dropped, see the comment at sqlite.rs:747). Generated queries use the value tables with `INDEXED BY` (sqlite.rs:874), so **these 94 indexes are dead**. Every write of a matching kind still pays `json_valid` and `json_extract` on the body to maintain them.
- **The Message exemplar.** Eight partial expression indexes over `json_extract` for the inbox, active, reply, batch and audit-age queries (sqlite.rs:492-501). `MessageQuery` uses them with `INDEXED BY`.
- **Labels and owners.** Persistent triggers re-derive them from `json_each(body_json,'$.metadata.labels' / '$.metadata.ownerReferences')` with DELETE then INSERT (sqlite.rs:660-669).
- **Digest.** TEMP triggers upsert `digest_entries` per object write. A second TEMP trigger recomputes the bucket hash with `digest_hash(json_group_array(...))` over every entry in the bucket (digest.rs:56-94).

On every native open, `initialize_query_indexes` truncates and **rebuilds every value table from all bodies** (sqlite.rs:732-735: "Repair on every native open"). With 95 kinds × fields × 2 tables, that is one full JSON parse per (row, declared field) at startup.

### 1.3 Write amplification per object write

These are static counts from the code, with the WAL measured on the kiwi copy by `wal_probe.py`. The probe emulates the TEMP value-table and digest triggers with Python UDFs.

**Replica event apply** (`apply_replica_values`, sqlite.rs:1971-2084), per mutation:

| Step | Statements | Notes |
|---|---|---|
| Rust: SELECT existing body+synced (1989), SELECT tombstone (2001), UPSERT (2031), DELETE tombstone (2043) | 4 | each re-prepared; the existing 21 KB body is read only to compare strings |
| Persistent labels/owners UPDATE trigger | 4 (2 DELETE + 2 INSERT … `json_each`) | 2× `json_valid`, 2× `json_each` parse |
| TEMP value-table triggers (Convoy: 12) | 24 (DELETE + INSERT each) | **12 full serde parses of the body** inside the UDF |
| Legacy expression indexes (Convoy: 2) | – | 2× `json_valid` + `json_extract` |
| Digest: entry upsert, then bucket rehash trigger | 2 + aggregate over bucket | `json_valid`/`json_extract` for rv |
| **Total (Convoy)** | **about 34 statements, 29 rows changed** | **about 15 JSON parses of a 21 KB body** |

Encoding before the writer: the typed object goes through `encode_object` → `Value` (store.rs:98), then `serde_json::to_string` (sqlite.rs:1987). Batching: `apply_replica_batch_raw` chunks events into 64 per transaction (sqlite.rs:3093, `REPLICA_BATCH_SIZE` in crates/flotilla-store/src/replica.rs:2). That saves commits, but not per-row work.

**Local status or spec update** (`update_status_raw` 2481, `update_typed_with_merge` 2411):

- SELECT and decode the existing object (`select_existing` 2747: `from_str`, then typed validate, then `from_value`).
- Validate by transcoding to `T` (`validate_status_update`, `validate_spec_update`, `validate_spec`, each one a `to_value`+`from_value`).
- **For `Forge` and `Project` (`VALIDATE_NAMESPACE_SPEC`), also SELECT and decode every sibling in the namespace on every write** (`validate_namespace_spec` 2720).
- `clone_through_serde(status)`, `encode_object`, `to_string`.
- Statements: allocate version (SELECT + UPSERT `resource_sequences`), UPSERT `resource_objects` (fires labels/owners, value-table, digest and legacy-index maintenance), DELETE quarantine, INSERT `resource_events` with the **full body again**, then `compact_events_to_limit`, which issues three statements per write once a stream exceeds 1,024 (DELETE events ≤ floor, DELETE event quarantine, UPSERT compaction; sqlite.rs:1429-1466).
- About 10 Rust statements plus trigger work. The probe measured 32 rows changed for a local Convoy.

**Measured WAL bytes per single-object write** (scratch copy; TEMP triggers emulated):

| Write | Body | WAL bytes | Rows changed | Amplification |
|---|---|---|---|---|
| replica Convoy upsert | 21 KB | **309,032** | 29 | 14.5× |
| replica TerminalSession upsert | 21 KB | 214,272 | 35 | 10× |
| replica Checkout upsert | 2 KB | 135,992 | 22 | 68× |
| replica Artifact upsert | 0.8 KB | 103,032 | 9 | 126× |
| local Convoy status write (+event, +compaction) | 16 KB | 259,592 | 32 | 16× |
| local Host status write (heartbeat) | 5.3 KB | 90,672 | 8 | 17× |

Without the TEMP-trigger emulation, the same Convoy replica upsert wrote 74 KB. About three quarters of the WAL is the derived value and digest tables.

**Write rates on kiwi.**

- `Host` has 344,147 versions since 2026-07-28. The retained window, 1,024 events, spans 20:14–21:31: **13.4 writes/min** of a 5.3 KB body, each about 91 KB of WAL, which is about **1.7 GB of WAL per day** for one heartbeat object.
- `Regard` has 239,124 versions.
- `Event` writes 1,024 events in 5.8 h.

### 1.4 Event log growth and compaction

- Retention is a **count per (kind, namespace) stream: 1,024 events** (`DEFAULT_MAX_EVENTS_PER_RESOURCE_STREAM`, crates/flotilla-resources/src/retention.rs:15). The `Observations` class keeps 1 (crates/flotilla-resources/src/replica.rs:25). It is enforced inline on every write (sqlite.rs:1411) and on startup (`compact_existing_events` 1468).
- Events carry the **full object body**. Total events on kiwi are 12,627 rows and 54.9 MB of JSON. The largest streams:

| Stream | Events | Bytes | Live local objects |
|---|---|---|---|
| Convoy | 1,024 | **26.0 MB** (avg 25 KB) | **1** |
| TerminalSession | 1,024 | 7.8 MB | 0 |
| Host | 1,024 | 5.5 MB (76 min of heartbeats) | 1 |
| WorkflowTemplate | 580 | 2.6 MB | 28 |
| ManifestRoot | 310 | 2.3 MB | 1 |
| ConvoyEnsure | 785 | 2.0 MB | 5 |
| Checkout, Repository, Vessel, Event, Presentation, Demand, Regard | about 1,000 each | 0.6–1.8 MB each | – |

- Growth is bounded at (kinds × namespaces × 1,024 × body size), so this is a **size** finding, not unbounded growth. The resume window it buys is arbitrary: 76 minutes for Host and days for Convoy. Count-based retention keeps 26 MB to resume a stream that has one object.
- **Replay** (`replay_events`, sqlite.rs:2805) runs inside `watch_raw` → `self.call(...)`, **on the single writer connection** (sqlite.rs:2681). It is there so that subscription and replay are atomic with commits (comment at sqlite.rs:80-81). The replay parses every event body with `from_str`, then **decodes it again to validate** (`decode_event`, 2864). A replay from the Convoy floor is 26 MB × (2.2 + 2.6 ms/MB), about **125 ms of writer time**, during which no write commits. #3123's "watch resource replay" slow operations (93 in three minutes) are this.
- **Replica tombstones have no GC.** 6,415 rows date from 2026-08-11 to now: Event 3,740, Checkout 866, TerminalSession 668. They grow with history and are scanned by nothing hot, but they are never reclaimed.

---

## 2. Query paths

### 2.1 Connection model (after #3126)

`SqliteBackend` (sqlite.rs:71) has one writer `tokio_rusqlite::Connection` and `READER_COUNT = 4` read-only WAL connections (sqlite.rs:56). Three serve resource reads; one is reserved for discovery inventory (sqlite.rs:280-282, 1070-1076).

- `read_call` applies a 2 s deadline and sheds reads that expire or whose caller has gone (sqlite.rs:1092-1103). This fixes the metastable queue in #3123.
- Each worker is one OS thread. **JSON decoding runs inside the worker closure** (for example, list_local_query 2215-2223 and list_replicas_raw 1578-1601), so a reader worker is busy for decode time, not SQL time. On the kiwi copy, SQL for any list here is under 5 ms.
- **The writer queue also carries watch replay** (2681), `diagnostics` (1178: `COUNT(*)` over `resource_events` plus quarantine scans), and cleanup tasks.

### 2.2 Statement preparation

There are **zero `prepare_cached` calls** in the workspace (`grep -rn prepare_cached crates` returns nothing). `sqlite.rs` has 22 `.prepare(` sites and `digest.rs` 2. Every `execute` and `query_row` also prepares, because rusqlite's `Connection::execute` and `query_row` call `prepare` internally. Hot sites:

| Site | Path | Per |
|---|---|---|
| sqlite.rs:1989, 2001, 2031, 2043 | replica apply | replica event |
| 1350, 1371, 1381 (sequence), 2778 (upsert), 2792, 1403 (event insert), 1439/1447/1455 (compaction), 2753 (select_existing) | local write | object write |
| 2147 (`get_raw`) | point read | `get` (922 slow in three minutes in #3123) |
| 2205 (list), 2946 (query_raw), 1558 (list replicas), 1669 (get replicas) | reads | call |
| 2133 (replica cursor) | replication | watch restart and step (510 slow in #3123) |
| 2834 (replay) | watch start | watch |

**Measured cost** on the kiwi schema (`prep_probe.py`):

| Statement | Cost |
|---|---|
| Replica upsert, uncached, persistent schema only | 0.278 ms |
| **Replica upsert, uncached, with the 279 TEMP value-table triggers the daemon installs** | **2.16 ms** |
| Same upsert, cached statement | **0.061 ms** |
| Point SELECT, uncached / cached | 0.023 / 0.013 ms |

Preparing an INSERT or UPDATE compiles every trigger on the table into the program; the `WHEN NEW.kind='X'` filter applies only at run time. **Every write to `resource_objects` or `replica_objects` therefore pays about 2 ms of compilation before doing any work.** For a burst of replica events, that alone caps the writer near 450 events/s.

### 2.3 Query builder

`declared_sql` and `declared_sql_with_names` (sqlite.rs:799-1005) generate SQL from `StoreQuery`:

- Labels, label prefixes, fields, field value sets, prefixes, presence, absence, owner and related-kind (`field_matches_query`) predicates become `name IN (SELECT … FROM <values table> INDEXED BY …)` subqueries.
- `any_of` becomes a UNION of identity subqueries.
- `all_sources_for_matches` builds the name set over both sources, then hydrates.
- Self-origin suppression is a correlated `NOT EXISTS` against `resource_objects` (975).

On the kiwi copy, `EXPLAIN QUERY PLAN` shows every generated shape using the intended indexes: a covering index on the value or label table, then a PK probe into the objects table, plus a temp B-tree for `ORDER BY name` on replicas.

**The builder is not the problem.** Pushdown exists and is planned correctly. The problems are:

1. Few hot callers use it.
2. Each call re-prepares.
3. The output is whole bodies, decoded about five times.

### 2.4 Per-path table

Notation: N_k is the number of objects of the kind, local plus every replica origin. Decode means the full chain: `from_str` → `Value` → typed validate (`ResourceKind::decode`, crates/flotilla-store/src/store.rs:65-75) → `from_value` → later `transcode` to `T` (store.rs:32, `convert_read_list`).

| Path | SQL | Predicates | Decode | Txn | Notes |
|---|---|---|---|---|---|
| `list` → `list_local_query` (2189) | 1 indexed range on PK | kind/ns only (default `StoreQuery`) | every row, about 5 passes | read txn | quarantine cleanup spawned on writer |
| `list_replicas_raw` (1549) | 1 range, temp B-tree sort | none | every row (on the worker) plus RFC 3339 parse | none | |
| `ReplicaReadResolver::list` (crates/flotilla-store/src/backend.rs:194) | **2 calls**: local `list()` + `list_replicas_typed` | none | all N_k | 2 | self-origin shadowing filtered in Rust (230-246) |
| `list_matching_labels` (backend.rs:248) | `query_declared`, label subquery | **pushed down** | matched rows | 1 | #2953's claim that it is not pushed down is **out of date** on main |
| `query_raw` (2928) | 1–2 generated statements in one snapshot | pushed down | matched rows; `merge_query_definitions` / `sort_query_items` in Rust | 1 | |
| `query_values_raw` (2983) | value table only | pushed down | none | 1 | the right shape for projections |
| `query_messages` (1501) | `INDEXED BY` partial expression index | pushed down | matched | 1 | exemplar |
| `DefinitionResolver::list` (crates/flotilla-store/src/definition.rs:83) | local list + replicas list | none | all sources, then causal merge in Rust | 2 | `apply` lists **all siblings on every apply** (definition.rs:164) |
| `DefinitionResolver::query_declared` (64) | hydrating query, then **rebuilds a `QueryIndex` in Rust** over the merged results (72-79) | pushed down, then re-filtered | matched | 1 | |
| `get` / `get_raw` (2147) | PK | – | 1 row | none | |
| watch replay (`replay_events` 2805) | range on events PK | `event_version > ?` | every event twice (validate, then keep `Value`) | **writer** | |
| live watch delivery | none | – | `Value` shared in `WatchPayload::Shared` | – | **good**: live events come from memory (`notify_watchers` 1485) |
| `ReplicaReadResolver::watch` (backend.rs:296-386) | **one `get` per event**: per Definitions event (369) and per self-origin replica event (333) | – | 1 row per event | per event | **delivery reads the DB** |
| `namespace_inventory` (183) | full covering-index scan of both object tables, UNION | – | none | discovery reader | O(N) per discovery call; should be a maintained set |
| `diagnostics` (1181) | `COUNT(*)` over events and objects plus quarantine scans | – | – | **writer** | |

---

## 3. Steady-state callers

The caller survey was traced from code at `ca3bc781`; file paths are under `crates/`. The cost columns use the section 4.2 decode rate (about **7.4 ms of CPU per MB** for the full read chain) and kiwi's cardinalities:

| Set | kiwi rows | Size |
|---|---|---|
| Convoy* (local + replica) | 231 | 4.93 MB |
| TerminalSession* | 13 | 0.27 MB |
| ConvoyEnsure* | 9 | 27 KB |
| Vessel* | 13 | 60 KB |
| Checkout* | 38 | 76 KB |
| Project definitions | 24 | 57 KB |

The ceiling, from §5.1, is **Convoy\* at 23k–230k rows (0.5–4.8 GB)** if history stays in the live tables, and about 2.3k active rows if history is archived. Every other kind is ×10.

**Timers.** Measured from code:

| Timer | Interval | Source |
|---|---|---|
| Controller resync | 60 s | `flotilla-daemon/src/runtime.rs:97` |
| Heartbeat | 30 s | `runtime.rs:96` |
| Leaf stall tick | 1 s per namespace | `flotilla-core/src/leaf_engine/wake.rs:248` |
| Regard expiry | 1 s | `in_process.rs:2058` |
| Replication digest | 60 s per (peer, kind) | `replicator.rs:21` |
| ImageBuild projection | 1 s | `runtime.rs:932` |
| Owner-GC backstop | 60 s | `runtime.rs:745` |
| `LANDING_EVIDENCE_TTL` (shorter than resync, so every resync re-inspects) | 30 s | `checkout_integration.rs:24` |

**Already resident (good):**

- The **leaf engine** reads stall judgement and `sync_rows` from 11 `WatchView`s (`leaf_engine/wake.rs:67-96`), with no SQLite on the tick.
- The **Aggregator**'s `flush_local_projection`, `summarize` and `rebuild_*` run over maps fed by 16 list+watch views. They make no store reads, although they recompute in full per event.
- **Live watch delivery** in the SQLite backend comes from memory.

These pieces are the seed of the resident model. The defect is that each consumer builds its own copy (#3120), and the paths below bypass them.

### 3.1 Callers that read the store per pass or per event

| # | Caller | Trigger and frequency | Store reads per pass | Now (kiwi) | Ceiling | Resident structure to read instead | Cold need |
|---|---|---|---|---|---|---|---|
| 1 | **Message controller** (`flotilla-daemon/src/runtime.rs:825-843`, `runtime/message.rs:40-153`, `flotilla-store/src/message_delivery.rs:82,98,288,735,772`) | **Every event** on 9 dependency kinds (Message, TerminalSession, Convoy, ConvoyEnsure, ChangeRequest, Issue, Artifact, Usage, Vessel) while any message is active. No debounce beyond name dedup. | `query(active)` ×3–4; `MessageReceiverSnapshot::load` (`message_inbox.rs:290-306`) = **Convoy\* + TerminalSession\* + ConvoyEnsure\* full lists** + Project and CrewDefaults definitions; all local Demands (`:735`); a TS `get` per holder group | about 5.3 MB, about **39 ms per event** | **3.6–36 s per event**: saturates | Receiver index: role address → holder `(origin, session)`; `(convoy, vessel, role)` → session; `(project, role)` → ensure → convoy. Plus the existing in-memory `MessageIndex` (`message_query.rs:58`) for active messages, and Demand gates by owning session. Maintained from TS, Convoy, ConvoyEnsure and Message events and own writes. | none (audit retention only) |
| 2 | **Pending supervisor turn task** (`flotilla-daemon/src/tasks.rs:522-639` → `crew_ops.rs:3197-3399`) | Every batch of Convoy, Message or TS events (`ready_chunks(64)`), plus every 60 s | Convoy\* query; `MessageReceiverSnapshot::load`; local Convoy query; **a `get` per local convoy** | about 39 ms per batch | 3.6–36 s per batch | convoy → {pending follow-up, supervisor turn, brief}, maintained from the Convoy watch | none |
| 3 | **Convoy controller** (`flotilla-store/src/convoy.rs:177-319`) | Every local convoy every 60 s, and on every labelled Vessel, Checkout or TS event | Full Vessel\* and Checkout\* lists, filtered by convoy in Rust; Forge definitions; Host\*, ChangeRequest\* and Artifact\* by phase. **N×M** | about 0.2 MB, about 1.5 ms per convoy | 150 convoys × (Vessel\* + Checkout\* ≈ 1.5 MB) ≈ **1.7 s/min** from resync alone; ×event rate | convoy label → `{(origin, name)}` for Vessel and Checkout; Forge by name (merged); ChangeRequest by `(service, scope, number)`; Artifact by name; Host by id | none |
| 4 | **Replication apply** (`flotilla-store/src/backend.rs:483-505`; `sqlite.rs:1971,2101,2123`; `replicator.rs:434-439,542,744,966-972`) | Per replicated event. Routed traffic is **one record per envelope** (`resource_explain.rs:125-131`). | Cursor SELECT on a reader (the cursor is already in memory); write transaction: 2 point SELECTs + upsert + tombstone delete (**about 2.2 ms prepare each**, §2.2); a second write transaction to advance the cursor. **Relay echo**: third-origin rows are re-sent by every peer, decoded, then filtered in Rust. The conflict rule (`sqlite.rs:2013-2018`) treats a newer `synced_at` with an identical body as a change, so it is rewritten (traced from code, not measured). **Discarded whole-kind lists** at `replicator.rs:744` (every routed restart) and `:542`. | about 3 round trips + about 309 KB WAL per Convoy event; up to N−2 = 1 echo rewrite | up to **28 echo rewrites × 309 KB = 8.6 MB of WAL per Convoy write per host**; a 10k catch-up burst = 22 s of prepare alone | `(kind, ns, origin)` → cursor; `(kind, ns, origin, name)` → (rv, synced_at, body hash) to drop echoes before any DB work; tombstone map. Cursor written through in the same transaction as the apply. **Delete the discarded lists.** | durable cursors and tombstones (read from memory after startup) |
| 5 | **`ReplicaReadResolver::watch`** (`flotilla-store/src/backend.rs:296-386`) | **Every event × every watcher** (`tasks.rs:483,533-535,994-995`, `runtime/message.rs:38`, `leaf_engine/routing.rs:139`, `forge_observation.rs:369`, `vessel_placement.rs:50`, `registry.rs:1342`) | Definitions kinds: `definitions().get` = local get + `get_replicas_typed` + merge, **per event** (`:355,:369`). Self-origin replica events: local `get` (`:333`). Plus `ReplicaConvoyCheckoutWatch` runs `list_matching_labels(Checkout)` per Convoy event (`controller/mod.rs:379`, "acceptable at current scale"); `LabelJoinWatch<Checkout,Vessel>` runs it per Checkout event (`:440`). | 2 reads × about 8 watchers per definition event | ×10 events × watchers | One shared merged-definitions map `(kind, ns, name)` → {origin → object}; a set of local names per `(kind, ns)` for self-origin suppression; the label indexes from the resident store | none |
| 6 | **Leaf engine: charter repair** (`flotilla-core/src/leaf_engine/stalls.rs:273` → `role_routing.rs:413,470`) | **Every 1 s tick**, for each local holder whose `brief-charter-revision` annotation (written only at launch, `vessel.rs:1373`) differs from the project charter | `inbox.messages.get(charter-<hash>)`; if missing: `ResolvedCascade::load` (Project and CrewDefaults lists) + `query(Active{receiver})` + `accept_locked` | 1 get/s per stale holder, **forever** after any charter change | ×10 holders | Message index by name, or a resident set of `(receiver, revision)` already notified; or re-stamp the annotation | none |
| 7 | **Leaf engine: routing reloads and turn delivery** (`leaf_engine/routing.rs:144-166`, `subscriptions.rs:222-259`, `turn_delivery.rs:87,286-293`, `supervision.rs:165-167`) | Per routing event on 6 kinds; per turn firing | The watch delivers a full typed object but the code keeps only the name, then re-reads each subject with `including_replicas().get` / `get_all`. Every convoy status patch reloads all of that convoy's row subjects. Turn delivery: Convoy get, subject get, TS `list_matching_labels`, **Checkout\* list then a ChangeRequest get per candidate** | small | ×10 | Use the event payload or `LeafViews` keyed `(kind, name)` → origin copies; Checkout by convoy label | none |
| 8 | **Change-request and issue observers** (`flotilla-core/src/change_request_observer.rs:110,165,181,448,496,692`, `issue_observer.rs:303`, `forge_observation.rs:158-334`, `in_process.rs:459,469,589,603`) | Convoy, Project, Repository, Host and Forge events (Host heartbeats wake it every 30 s per host); per refresh tick (10 s/15 s/90 s per active subject); per forge read | `reconcile_convoy_demands`: Convoy\* list + `source_owner` per source. `source_owner`: Forge definitions + Project sources + Project definitions + Host sources + a Repository get per member, **once per source and once per tick per subject**. Observation queries: Repository\* + Convoy\* / Forge + Convoy\*. `ChangeRequest*` full list for retained subjects; all local Convoys per history publish. | about 5 MB per Host-heartbeat wake ≈ 37 ms every 30 s per host | 30 hosts × (3.6–36 s) every 30 s: **saturates** | Resident source → owner election, recomputed only on Project/Repository/Forge/Host-readiness change; Repository by forge `(service, scope)`; Convoy by subject ref | retained-terminal ChangeRequests = cold |
| 9 | **Aggregator resolvers** (`flotilla-aggregator/src/aggregator.rs:1003,1804`; `attach.rs:348-372,530`; `in_process.rs:3544`) | Attach-capability refresh on every TS, Environment or peer/host event (abort and respawn, no debounce); branch-subject discovery inline on the loop, at most once per convoy per 5 min; issue materializer every 30 s | Convoy and TS queries + **an Environment `get` per session** (the aggregator discards Environment watch items at `:1003`); Checkout\* + Forge lists | about 5 MB per TS event when the Convoy query is unselective | saturates | The aggregator's own maps plus an Environment map it already watches | none |
| 10 | **Controller resync** (`flotilla-store/src/controller/mod.rs:691,763`) | Every 60 s per controller | Lists the whole primary kind, **keeps only names**, then re-reads each item with `get` (2N reads) | small (local kinds) | O(local N) per controller per minute; with retained local history ×100 | Resync from the resident map: names are already there | none |
| 11 | **Convoy-ensure** (`flotilla-controllers/src/reconcilers/convoy_ensure.rs:178,370,385,571,1197`) | Any event in 10 kinds (including Host heartbeats), 200 ms coalesce, + 60 s | ConvoyEnsure definitions; **per driver ensure: Convoy\* once or twice + Host\***. O(ensures × convoys), acknowledged in a comment at `:370` | 9 × 5 MB ≈ **330 ms per pass**, every Host heartbeat | 90 × 0.5–4.8 GB: saturates | `(project, role)` → ensure; ensure → convoy (by the `ensured_from` annotation); Host by id | none |
| 12 | **Checkout / Repository / Vessel placement / projected vessels and sessions / ImageBuild** (`checkout.rs:375,402-405`, `checkout/runtime.rs:72`, `repository.rs:99,127`, `vessel_placement.rs:53-160`, runtime ImageBuild projection) | Per reconcile / per event batch / ImageBuild **every 1 s** | Pending checkout: all Checkouts, then `owning_convoy` **per sibling inside the loop**; ChangeRequest\* to find one record on every resync (because of the TTL); Convoy\*, Vessel\* and PlacementPolicy\* per reconcile; ImageBuild\* + a remote get per build every 1 s | small to medium | N² checkouts; Convoy\* per reconcile saturates | Checkout by convoy label; ChangeRequest by key; Convoy by name; vessel placement already holds its inputs in memory but uses them only to detect change | none |
| 13 | **Expiry scans** (`regard_lifecycle.rs:148-150`, `demand_lifecycle.rs:21`, `event.rs:116`, `health.rs:252`, owner-GC backstop `runtime.rs:745`) | Regard 1 s; Demand, Event 30 s; health tick; owner GC 60 s over all 35 kinds | Whole-kind lists to find expired items; owner GC lists every kind even though a resident owner → children index already exists for the event path | small now | O(N) per tick per kind | A deadline heap per kind (expiry → names), maintained by writes and events; owner GC from the existing index | none |

**Whole-kind list call sites.**

- There are 146 non-test whole-kind list call sites, about 120 of them in caller code. The largest counts are `runtime/credentials.rs` 7, `forge_observation.rs` 7, `dispatch_reconciler.rs` 6, `in_process.rs` 6 and `crew_ops.rs` 6.
- **Lists inside per-item loops:**
  - `checkout.rs:402-405`
  - `convoy_ensure.rs:385/1197` (once per ensure)
  - `forge_observation.rs:158` (once per source, and once per subject per tick)
  - `role_routing.rs:120` (`ResolvedCascade::load` once per ancestor project)
  - `turn_delivery.rs:290-293`
  - `convoy.rs:177-319`
  - `crew_ops.rs:3399`
  - `message_retention.rs:57-58` (hourly; this one is legitimately cold)

**Status of #2953's original claims on main.**

- `resolve_message_receiver` is fixed. It now uses narrow `query_declared` lookups (`message_inbox.rs:207-287,486`).
- `crew_message_views` is now `#[cfg(test)]` (`read_projections.rs:2101`).
- `list_matching_labels` is pushed down (§2.4).
- **The whole-kind `MessageReceiverSnapshot::load` is still live** in `message_delivery.rs:98,288,772`, `crew_ops.rs:3220`, and `role_routing.rs:99,177,241,330`. `crew_address_book` is called from the Vessel controller (`vessel.rs:1350`) and from admission.

**Cold need.** Only message audit retention, replication snapshots, cursors and tombstones, and retained terminal ChangeRequests need data the daemon would not hold resident. Cursors and tombstones must be durable, but they can be read from memory after startup. **Every other steady-state path above can read a resident index.**

---

## 4. Live evidence (kiwi, 2026-10-12 22:31)

### 4.1 Objects per kind

| Source | Kind | Rows | Bytes | Avg | Max |
|---|---|---|---|---|---|
| replica | Convoy | 230 | 4,910,393 | 21,350 | 47,567 |
| replica | Artifact | 1,299 | 1,066,369 | 821 | 1,317 |
| replica | WorkflowTemplate | 112 | 540,724 | 4,828 | 9,378 |
| replica | ForgeRead | 148 | 471,400 | 3,185 | 101,279 |
| replica | TerminalSession | 13 | 269,870 | 20,759 | 26,857 |
| local | ForgeRead | 70 | 206,085 | 2,944 | 35,193 |
| replica | Event | 174 | 187,218 | 1,076 | 1,280 |
| local | WorkflowTemplate | 28 | 134,899 | 4,818 | 7,944 |
| local | Artifact | 134 | 107,896 | 805 | 1,041 |
| replica | Checkout | 38 | 76,196 | 2,005 | 2,863 |
| replica | ForgeReadHeartbeat | 148 | 66,241 | 448 | 494 |
| replica | Vessel | 13 | 60,401 | 4,646 | 5,127 |
| local | Project / ForgeReadHeartbeat / Event | 17 / 70 / 31 | 35.6 K / 31.5 K / 31.2 K | | |
| others | 25 small kinds | under 25 KB each | | | |

Replica origins: feta (`9ad3…`) 1,962 rows; udder (`1c8d…`) 285 rows.

**What is in the big bodies:**

- Convoy (avg 21 KB): `status.workflow_snapshot` 5.2 KB, `status.work` 3.3 KB, `placement_decision` 2.4 KB, `turn_deliveries` 2.0 KB, `lifecycle_mutations` 1.7 KB, `metadata.annotations` 3.0 KB.
  - The 5.2 KB workflow snapshot is copied into every convoy, even though only 140 templates exist. It is a candidate for content addressing or interning.
- TerminalSession (avg 21 KB): `status.launch_command` 17.4 KB.

**History share:**

- Replica Convoys: Landed 213 (4.45 MB), Abandoned 3, Active 14 (0.38 MB). **94% of rows and 92% of bytes are finished.**
- Artifacts: 688 belong to convoys that no longer exist, 684 to Landed or Abandoned convoys, and **50 to Active convoys (3.5%)**.

### 4.2 Startup-load and decode cost (measured on the copy)

`bench/` is a standalone Rust program using `serde_json` 1.0.154, release build, with a counting allocator.

| Set | Rows | Text | SQL scan | Parse to `Value` | `Value` heap (×text) | One extra `to_value`/`from_value` round trip |
|---|---|---|---|---|---|---|
| local objects | 426 | 0.68 MB | <16 ms (both) | 2.1 ms | 4.5 MB (6.7×) | 2.7 ms |
| replica objects | 2,247 | 7.76 MB | (above) | 17.2 ms | 42.0 MB (5.4×) | 20.4 ms |
| events | 12,627 | 54.9 MB | 52 ms | 104 ms | 295 MB (5.4×) | 139 ms |

The rule of thumb is 2.2 ms/MB to parse, 2.6 ms/MB per extra round trip, and **5.4× heap overhead for `Value`**.

- Typed structs are substantially smaller. Strings dominate, so typed is about 1.5–2× text against `Value`'s 5.4×. That is why "typed resident state, not JSON blobs" matters for memory.
- The read path does about one parse plus two round trips, roughly **7.4 ms/MB**.

### 4.3 Event log

See section 1.4. Compaction floors on kiwi:

| Stream | Compacted through |
|---|---|
| Host | 343,123 |
| Regard | 238,100 |
| ForgeReadHeartbeat | 127,937 |
| ForgeRead | 89,053 |
| TerminalSession | 57,359 |
| Event | 53,686 |
| Checkout | 32,097 |
| Repository | 25,817 |
| Demand | 19,798 |
| Convoy | 10,466 |

### 4.4 Query plans

`EXPLAIN QUERY PLAN` was run on the copy for queries reconstructed from `declared_sql`:

- local Convoy list: `SEARCH o USING INDEX sqlite_autoindex_resource_objects_1 (group_name=? AND version=? AND kind=? AND namespace=?)`
- replica Convoy list: `SEARCH … replica_objects_by_resource_name (g, v, kind, ns)`, then `USE TEMP B-TREE FOR ORDER BY`
- replica label query plus self-origin suppression: `LIST SUBQUERY` on `replica_objects_labels_lookup` (covering, with a bloom filter), a PK probe, and a correlated `sqlite_autoindex_resource_objects_1` covering probe
- field query (`Convoy status.phase`): covering `replica_objects_Convoy_status_phase_scalar_v3`, then a PK probe
- watch replay: `SEARCH resource_events USING INDEX sqlite_autoindex_resource_events_1 (… event_version>?)`
- namespace inventory: **`SCAN`** of both covering indexes plus a temp B-tree (O(N) per discovery call)
- digest bucket snapshot: covering digest index, then a PK probe

Every predicate is indexed. The planner and SQL are not the cost.

---

## 5. Verdict

### 5.1 Workload note and ceiling

There is no `WORKLOAD.md`, so the ceiling is sized at the largest plausible deployment, and the assumptions are stated here. This gap is itself a finding: #2953's contract should create `WORKLOAD.md`.

| Driver | Now (kiwi) | Ceiling | Assumption |
|---|---|---|---|
| Hosts / replica origins | 3 / 2 | 30 / 29 | 10× hosts |
| Active convoys (fleet) | 15 | 150 | scales with hosts |
| Finished convoys retained (fleet) | 216 | 21,600 (100× history), or 216,000 if every host replicates every host's history | 100× history |
| Live objects (all kinds) | 2,673 rows, 8.4 MB | resident about 34 MB text; cold 0.5–5 GB | the resident share (about 2.8 MB now) scales with hosts; history goes to cold |
| Object writes | Host heartbeat 13/min per host; replication catch-up bursts of thousands of events (#3123: 536 slow applies in 3 min) | 30× heartbeats; bursts 10× | |
| Event log | 62 MB | about 1,024 × kinds × body, roughly 100 MB with today's count-based retention | bounded but wasteful |

### 5.2 Classification: resident and cold

| Data | Class | Now | Ceiling |
|---|---|---|---|
| Active Convoys (+ ConvoyEnsure, Vessel, TerminalSession, Checkout of active work) | **resident** | 15 + 9 + 13 + 13 + 38 rows, about 0.75 MB | ×10, about 8 MB text |
| Definitions (Project, Repository, WorkflowTemplate, Credential*, PlacementPolicy, FulfilmentKind, Forge, CrewDefaults, CrewImageBaseline, FleetDesignation, DispatchHold, ImageLayer) | **resident** | about 260 rows, about 0.85 MB | grows with projects, not hosts; under 10 MB |
| Host, Environment, Clone, ManifestRoot, Demand | **resident** | about 25 rows | ×10 |
| Observations (ForgeRead, ForgeReadHeartbeat, ChangeRequest, Event, DispatchObservation, Issue, Usage) | **resident** (bounded by TTL / demand) | about 600 rows, about 1 MB | ×10, about 10 MB |
| Messages (active inbox) | **resident** | 0 on kiwi (feta: 431) | bounded by active receivers |
| Artifacts of active convoys | **resident** | 50 rows | ×10 |
| replica_cursors, digest_nodes | **resident** (tiny) | 58 / 2,950 | |
| **Finished Convoys** (Landed, Abandoned) | **cold → archive** | 216 rows, 4.5 MB | 21.6k–216k rows, 0.45–4.5 GB |
| **Artifacts of finished or deleted convoys** | **cold → archive** (orphans: delete) | 1,372 rows, 1.1 MB | 137k rows, about 110 MB+ |
| **resource_events** beyond a short resume window | **cold** (retain minutes, per object) | 12,627 rows, 62 MB | |
| **replica_tombstones** older than the resume window | **cold → GC** | 6,415 | grows with history |
| Message audit (satisfied, expired, …) | **cold** | – | |
| quarantines, field-ownership violations | cold, diagnostics only | 0 | |

**The resident set is about 2.8 MB of text today, about 34 MB at the ceiling.** As typed structs that is about 4–6 MB now and about 50–70 MB at the ceiling. Startup load is under 50 ms now and under about 0.3 s at the ceiling at 7.4 ms/MB.

**The cold set is the only place pushdown belongs**, and it has few query shapes:

- finished convoys by subject, repository or project, by time window;
- artifacts by convoy;
- message audit by age (already indexed: `message_audit_age`);
- events by (kind, name, version) for resume.

### 5.3 Ranked naive shapes (cost at ceiling = ops × cardinality × bytes per decode)

| Rank | Shape | Ceiling arithmetic | Production shape | Where |
|---|---|---|---|---|
| 1 | Steady-state whole-kind list-and-decode, including history (section 3), and per-item lists inside loops | Convoy list: 23k–230k rows × 21 KB × 7.4 ms/MB = **3.6–36 s of CPU per call**, 2.6–26 GB of transient `Value`. Per-message receiver resolution multiplies that by the number of messages | Typed resident store per kind; maintained indexes (by subject, receiver, convoy label, phase, owner); history excluded by construction | **resident redesign** |
| 2 | Uncached statements compiling about 280 triggers per write | Catch-up burst of 10k events × 2.16 ms = **21.6 s of writer time** versus 0.6 s cached; every point read pays prepare too | `prepare_cached` everywhere, with a cache sized for the declared shapes; then delete the TEMP-trigger value tables for resident kinds | **quick win**, then resident redesign |
| 3 | Event delivery reading the DB: watch replay on the writer, a `get` per Definitions or self-origin event, resync relists | Replay from the floor: 26 MB parse on the writer, about 125 ms of stall per watcher start; ×N watchers after restart and ×29 peers resuming | Delivery from the resident store's ordered change log (in-memory ring per kind, sized by time); DB replay only for a cold resume, on a reader | **resident redesign** (replay-off-writer as a quick win) |
| 4 | Write amplification (about 30 statements, about 15 body parses, 14–126× WAL per write) | 30 hosts × 13 heartbeats/min × 91 KB = **51 GB/day WAL** fleet-wide from heartbeats alone; Convoy replica 309 KB × catch-up bursts | Write-through of a typed object: one row upsert plus one compact change-log row. No derived JSON index tables for resident kinds (indexes live in memory); drop 94 dead indexes now; coalesce heartbeats (or make them ephemeral observation state, not durable) | quick win (dead indexes, heartbeats), then resident redesign |
| 5 | About 5 decode passes per row (validate-decode thrown away, `Value` round trips) | ×5 on every byte decoded above | Decode once, straight to `T` (`from_str::<K8sResourceObject<T>>`); keep raw bytes for pass-through; never `transcode` | quick win |
| 6 | Startup repair rebuilds all 190 value tables from bodies on every open | one parse per (row × declared field) per open; at ceiling (about 300k rows incl. history × about 5 fields) about 1.5M parses | Gone with resident indexes; until then make the repair conditional (schema hash or unknown-writer marker) | quick win |
| 7 | `namespace_inventory` and `stored_namespaces` scan both object tables per discovery call | O(N) per call; 300k rows at ceiling | Maintained (kind → namespaces) set in memory | resident redesign |
| 8 | `validate_namespace_spec` and `DefinitionResolver::apply` decode every sibling on every write | O(N_kind) per write (Projects, Forges, definitions) | Validate against the resident map | resident redesign |

### 5.4 Size findings (separate from the shapes)

| Finding | Now | Fix |
|---|---|---|
| Event log is 70% of file; count retention keeps 26 MB for one live Convoy | 62 MB | per-object, time-bounded retention (resume window); store deltas or references instead of full bodies; archive beyond window |
| Replica tombstones never collected | 6,415 rows, 2.4 MB with index | GC beyond the resume window or the digest-proven prefix |
| Workflow snapshot copied into every convoy (5.2 KB × 230) | about 1.2 MB | content-addressed snapshot reference (needs the N→N+1 decode shim) |
| TerminalSession `launch_command` 17 KB per session | 0.27 MB | store by reference |
| Orphan Artifacts (convoy deleted) | 688 rows | owner GC (`owner_gc.rs` exists; check why these escape) |
| Identity strings repeated in every row of 190 value tables and 498 indexes | about 6 MB of the derived tables | disappears with resident indexes; otherwise intern (kind_id INTEGER) |
| `Value` resident representation is 5.4× text | (#3120 measured three O(N) copies at about 5 MB each, N=224) | typed `Arc<T>` in rpds maps (#3120) |

### 5.5 Recommended split

**Quick wins.** Each is independent, has no schema-format change, and takes days:

1. `prepare_cached` on every fixed SQL string in `sqlite.rs` and `digest.rs`, and `set_prepared_statement_cache_capacity` sized for the generated shapes. Expect about 35× on writes to the object tables.
2. Drop the 94 dead legacy `*_metadata_name` / `*_status_phase` expression indexes. Verify they are unused with an EXPLAIN test that already exists in shape (`declared_query_plans_use_indexes`, sqlite.rs:3815).
3. Decode once per row: drop the throwaway typed validate plus the `from_value` plus the `transcode` chain.
4. Move watch replay off the writer. Snapshot `current_version` and register the subscriber under the watcher lock, then replay on a reader up to that version.
5. Make the startup value-table repair conditional.
6. Garbage-collect replica tombstones; delete orphan Artifacts.
7. Make the Host heartbeat non-durable, or coalesce it. 13 writes/min × 91 KB WAL is not durability anyone needs.

**Resident-state redesign** (the main contract; it extends #3120's rpds generations into the source of truth):

- **One `KindStore<T>` per resource kind.** It holds `rpds::HashTrieMap<(origin, name), Arc<ResourceObject<T>>>`, loaded at startup (under 50 ms on kiwi). It has maintained secondary indexes for declared fields, labels and owner references, and the hot derived maps section 3 names. The writer task owns it.
- **Mutations** validate against the resident map, update the map and indexes, write through to SQLite (one upsert plus one change row, prepared and cached), and publish the change from memory.
- **Replication applies** update the same map.
- **Watches and resync** read the map plus an in-memory change ring. SQLite is never on the delivery path.
- **SQLite** keeps durable rows and is the source for the cold set only.
- The 190 value tables, the labels and owners tables and their triggers become redundant for resident kinds, and go. That removes most of the write amplification measured above.

**#2953, narrowed to the cold set:**

- A retention and archive split: finished convoys and their artifacts, old events and tombstones move to history tables, later to an archive host.
- A small declared query vocabulary over history, with `EXPLAIN QUERY PLAN` tests: the `MessageQuery` pattern, which already works.
- Guard rails: read-count assertions (`ReadObserver` exists, store.rs) on interactive and reconcile paths, so that steady-state paths assert **zero** DB reads.
- `WORKLOAD.md` with the numbers above.

---

## Appendix: method and reproduction

- Copy: `sqlite3 ~/.local/state/flotilla/resources.sqlite ".backup '…/store-dive/resources.sqlite'"`. The live DB was not written.
- Row counts, sizes and plans: `sqlite3` 3.51.0 on the copy; `dbstat` is available.
- Decode and heap: `store-dive/bench` (Rust, `serde_json` 1.0.154, counting global allocator), over bodies exported one per line.
- WAL per write: `store-dive/wal_probe.py` on a scratch copy. TEMP value-table triggers and digest triggers are emulated with Python UDFs (one value row per table). The figures are about right; the real UDF parses the body once per field, so its CPU is higher, not its WAL.
- Prepare cost: `store-dive/prep_probe.py`. Python `sqlite3` with `cached_statements=0` against a cached connection, 200 iterations inside a rolled-back transaction, with the 279 TEMP triggers recreated as the daemon defines them.
