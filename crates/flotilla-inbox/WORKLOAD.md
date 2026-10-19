# Workload model: inbox bounded replica

> Owner review 2026-10-19: the current resident-index, clock and ceiling model is rejected. The existing numbers/choices below are historical implementation evidence pending the requested corrections; they are not an accepted sizing or switch-readiness claim.

Inherits deployment tiers, messages, convoys, changes and budgets from the root
[workload](../../WORKLOAD.md). The sizing case is one large daemon: 10,000
running convoys, about 500,000 messages (50/convoy), 2 KB mean bodies, 500 view
clients and 10,000 burst changes/s. The daemon's total resident budget is 1 GB.
This crate still has no production callers until #3317.

## Query cover and operations

`Home<Message>` is a **bounded filtered replica**, not the store. A single writer
pushes accepted changes into resident open values and their indexes. Terminal
values leave immediately. Missing open values are fetched by key. Pending-index
coverage starts complete; the first budget eviction invalidates that coverage,
drops the resident index, and routes pending queries to the declared store index
for the rest of that backend instance. This conservative cover check cannot
return an incomplete list as complete. This is a permanent warm-to-cold
throughput cliff for that instance even after the backlog drains; coverage is
not automatically rebuilt. Restart rebuilds cover if it fits.

| Access | Default | Cold path / cost |
|---|---|---|
| enqueue | resident admission/evidence; one committed create | sequence read to stamp `admitted_seq`; SQLite rechecks inside the write transaction |
| receive/deliver/settle/drop | keyed resident open value, local bindings/receipts, selective observed evidence | `resolve` + typed `load`; open receipts through paged `about`; no scan or unrelated resource history |
| pending | per-inbox resident BTree range in immutable admission order | `page(pending_by_inbox)` in 32-id pages; load at most 32 candidates one at a time for names, then keyed load/receipts for each consumed item |
| bind | same pending index/route; inspect observed receipts until a match | cold path uses declared Pending pages and keyed open loads; waits outside writer until Deliver |
| unbind | local binding observation | no store read |
| get diagnostic | resident key | current terminal/open value by `resolve` + `load`; terminal value discarded afterwards |
| watch | source log, `changes_since` | up to 16 source entries/page, 16 KiB decoded buffer; Message edges/receipts attribute inbox using resolve + the first `changes` entry, never the current terminal body |
| restart | store startup scan, 32 resources/page | load each current resource; retain only open values; recover their receipts with `about`; finish Undeliverable reactions one source at a time |

Admission order is the create's home sequence on the row, not the mutable last
version or a store-local id. `pending_by_inbox(inbox, admitted_seq) WHERE Pending`
replaces `open_by_inbox`; no mixed or Awaiting index is maintained. A log refuses
zero/mismatched admission values. The writer overwrites any caller value.
Other consumer writers may advance the same home sequence between stamping and
create; a refused create leaves Home unchanged and the adapter retries once
with the new sequence. Sustained contention after that bounded retry returns
an error; it never persists an incorrect order.

For a cold Pending page, the API-level cost is 1 index read + up to 32 typed
candidate loads, then resolve/load and receipt `about` pages for each item not
already resident. The low-budget 6,000-item measurement below performs 12,000
typed loads, 188 index pages and 6,000 empty receipt pages, with no terminal loads
or resource-history reads. Indexed cold reads deliberately trade CPU/latency for
a fixed resident footprint; they are not advertised as zero-read delivery.

Cold bind is O(inbox Pending backlog) in the no-receipt case: it loads each
candidate and its receipt page under the writer lock/one worker, so unrelated
operations wait for that pass. A receipt hit exits early and the subsequent wait
releases the worker/lock. Receipts also come from external observation evidence,
so a new SQLite receipt index alone cannot replace that gate lookup. No new
schema/index is introduced; reducing this cold-bind latency needs a separately
justified read shape and, if necessary, owner schema look-over.

Warm delivery reads no resource/receipt history. `delivery_reads_no_terminal_history`
checks both warm and forced-cold delivery after 96 Settled/Dropped messages;
it refuses terminal loads, scans, resource-history reads and watch reads.
The warm case additionally refuses resource and receipt reads entirely.

## Resident size model and budget

`resident.rs` implements conservative charges for owned data, BTree node slack
and duplicate index keys. Frozen bodies are charged per resident handle even
when their Arcs share storage. The budget is **128 MiB** for the dynamic replica,
including local binding observations and registered consumer keys. This is a
share of, not a replacement for, the root daemon budget. Fixed engine definition,
mutex/Arc/type headers are constant (allow 16 KiB); backing-store caches, injected
observation owners and consumer-port implementations have their own ownership.
The engine neither copies nor retains those collaborators' worlds/history.

| Structure | Charge / lifetime |
|---|---|
| open `Resource<Message>` | Rust header + 1,024 B map/node/key allowance + 8×name length, sender address and 2×inbox address; body canonical bytes + typed text capacity (or typed request maps/strings); reference Vec capacity, revision capacities, cross-home reference strings, condition addresses and receiver |
| address | Rust header + 8×canonical length + 128 B per segment (canonical/decoded text, Vec slack and artifact subject) |
| receipt map | 128 B header/node allowance + each receiver's header, id/host capacities and 96 B node slack; evicted with its open message |
| pending index, LRU, charge map | accounted conservatively in each resource's key/node allowance; empty inbox entries are removed; all pending-index keys are dropped on cover invalidation |
| local binding | address + binding header + 128 B node allowance + receiver charges; default never-bound inboxes need no entry |
| registered consumer key | receiver charge + 64 B for map/port handle; port internals are caller-owned dependencies |
| gate evidence | selective snapshot of at most the named message/inbox/condition/authority/supersession/reference keys; versions preserved; released after that operation, no whole-table Clone |

For the measured distinct 2 KB messages, the charge is about 7.7 KB each including
metadata; holding all 500,000 would be about 3.8 GB and is rejected. At the ceiling,
charged dynamic resident bytes are **at most 134,217,728**, regardless of retained
terminal/history counts. With the root's roughly 36,000 live vessels, allocating
one binding per vessel plus one per convoy gives about 46,000 bindings. At an
illustrative 2 KB/binding and 256 B/consumer key, controls use about 101 MB,
leaving roughly 33 MB for about 4,000 such open records. Actual address lengths,
receipt fanout and typed requests change that split; these are explicit estimates,
not extra limits on resource cardinality. Larger records simply stay cold.

Every access reconciles changed resources and evicts least-recently-used open
values until charges fit. Terminal values and their receipts are removed even
when the cache has room. Irreducible local controls cannot be forgotten safely:
a new binding or consumer registration that alone exceeds the budget is refused
before mutation (`Storage`; setup-only blocking registration fails fast).
Existing observations remain intact. No resource or log entry is deleted.

`residency_blocking` reports budget, accounted bytes, open count, pending cover,
and **oldest/newest resident state-entry timestamps**. The covered time span is
measured, not fixed at seven days or a chosen number of seconds. There is no
resident history window and no history trimming.

## Store and transient ownership

MemoryInboxLog is the memory implementation's authoritative storage, retaining
rows and its one event log with admission/pending/about indexes. Its storage
bytes grow with retained data, like an in-memory SQLite database; this test
backend is not a way to fit unbounded durable history inside daemon RAM.
The bounded read replica has the same budget in both backends. Production
composition uses a disk Store; native SQLite pages/log data are not Rust replica
allocations. Archive/expunge policy and disk growth remain outside #3466.

SQLite's existing bounded two-generation caches (65,536 entries/generation each
for strings, identities and reverse ids, 512 prepared statements) are unchanged.
At illustrative 128 B/entry, the three two-generation caches cost about 48 MiB
plus string payloads; budget roughly 100 MiB for normal short names. This is
separate source-layer memory, not a second full decoded resource replica.

Startup holds at most 32 current resources before trimming each value. With a
1 MiB frozen-body limit and canonical + decoded text, that is about 64 MiB of
body transient space, plus metadata. Runtime cold Pending pages retain only 32
names, not 32 bodies. A gate holds only one resource's evidence/read set, and a
watch holds one cursor and at most 16 KiB of decoded changes (a first oversized
change is returned alone). At 500 clients, decoded buffers cost at most about
8 MiB plus small cursors/handles; delivered items belong to their callers. Unusually large metadata/evidence fanout must be budgeted by its
composition owner rather than hidden behind a whole-world clone.

## Watch and concurrency choices

There is no copied 65,536-entry journal. Both implementations read their source
log. SQLite decodes a Message edge's recorded state effect, so old cursors report
the state at that event rather than the resource's latest terminal state.
Lag and restart alone do not expire a cursor. A real missing source sequence
(e.g. the store's existing expunge) returns CursorExpired; ahead returns
CursorAhead. This issue adds no trimming and stores no extra watch history.

Log-backed watch intentionally crosses into storage. With 500 clients and
10,000 source changes/s, a fully caught-up client set could examine 5 million
source entries/s; this replaces the earlier resident-journal zero-read choice
at the issue owner's explicit direction. Pages of at most 16 entries and a 16 KiB decoded-byte cap bound subscriber
memory; sparse inboxes advance their source cursor past unrelated entries. This
amortizes log calls for dense histories, while key/create-event attribution is
performed once per distinct message in a page (zero lookups when its create is
already in that page); the decoded change buffer itself supplies this page-local
cache, so there is no retained name-to-inbox map. Any future batched/shared-tail optimization must
preserve log authority and a byte budget, not reinstate a second retained journal.
The source log's explicit cursor gaps are the recovery backstop.

An async writer mutex serializes admission **before** one blocking worker is
submitted; the standard state mutex protects that decision and persistence.
There is no 64-permit worker pool or permits-per-durable-rate arithmetic.
Concurrent callers wait in their own futures, not in an unbounded blocking-pool
queue. Bind waits and watch notification waits occur outside the writer.
Notification is enabled before reading so concurrent commits cannot lose a wake.
Consumer ports commit to this exact Store/home before publication. Observed
evidence supplies one consistent borrowed snapshot from which only named values
are copied, preserving recorded versions; engine tables no longer implement Clone.

## Measurements and expiry

2026-10-18, development profile, in-memory SQLite, one blocking writer, distinct
2 KB text bodies, 512 KiB replica budget and 128-entry/generation source caches
to isolate replica allocations (`resident_allocations`, separate process allocator):

- 2,000 / 4,000 / 6,000 open messages: 517,017 accounted bytes, 67 resident values.
- Total live Rust allocations: 675,150 / 664,174 / 698,302 B. The source rows/log
  are in SQLite native memory and excluded from this Rust allocation count.
- Enqueue 6,000: 3,661 ms; stream all 6,000 cold pending items: 2,218 ms.
- Cold reads: 12,000 typed loads, 188 Pending pages, 6,000 receipt pages,
  **zero terminal loads, startup scans or resource-history pages**.

A cold bind with 1,024 pending messages and a 2 KiB budget took 235 ms:
32 Pending pages, 2,048 typed loads (index-page decoding plus keyed candidates),
1,024 empty receipt pages, and zero terminal/history/scan reads. Its regression
guard fixes these counts to expose the linear cost. Restart over 2,048 terminal
messages took 87 ms, 64 startup scan pages (each decoding up to 32 bodies), zero
receipt pages, and retained zero messages/bytes. These use development-profile
in-memory SQLite and warm source caches; real disk I/O and a ceiling-sized
500,000-message restart are not measured by these numbers. Startup cost still
grows with stored history even though retained memory does not.

A dense watch replay of one create/receipt/edge lifecycle, repeated 1,000 times
(3,000 changes), took 295 ms with page-local attribution reuse and zero resource
history reads. Disabling reuse took 625 ms and 2,000 resource-history reads
(each also requires a resolve); the new read-count guard fails in that case.
This is development-profile in-memory SQLite, a warm three-event source page,
not a sparse/mixed-inbox or 500-subscriber benchmark. A page without a message's
create still needs one resolve + create-history read per distinct message; the
worst case remains 16 pairs per page. Fanout therefore remains an explicit
performance expiry above, rather than a demonstrated 10k/s watch throughput.

The allocation workload is the calibration check for the conservative charge
heuristics, not a proof for every possible accepted value shape. Extend its
body/address/reference/receiver coverage when those types change.

These are diagnostic CPU/worker measurements, not fsync latency or a promise of
10,000 durable writes/s. The root's disk write budget is a different constraint.

Revisit this shape when cold Pending throughput or log-backed watch fanout misses
the deployment's latency/CPU budget, when control observations approach their
128 MiB share, when root cardinalities/addresses grow, or when one resource's
metadata/evidence dominates transient memory. Eviction already handles a larger
open set without a contract migration. A persistent per-inbox cover or shared
bounded log reader may improve cold cost, but needs measured justification.
#3317 is no longer the expiry of a full-resident reference query model: that
model and its copied watch journal have been removed before the production switch.
