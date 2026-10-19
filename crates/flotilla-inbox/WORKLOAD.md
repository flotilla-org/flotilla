# Workload model: inbox bounded replica

Inherits the root [workload](../../WORKLOAD.md): a large daemon has 10,000
running convoys, about 36,000 vessels, 500,000 messages at 2 KiB mean bodies,
500 view clients and a 10,000 all-kind changes/s burst. Model 46,000 inboxes
(one per vessel plus convoy), one bound consumer per inbox and 36,000 registered
consumer keys. This is pre-production until #3317. Design decisions and cursor
semantics live in the [implementation record](../../docs/implementation-choices/inbox-contract.md).

## Ownership and budgets

| Resident structure | Size model | Budget / eviction |
|---|---|---|
| Per-inbox Pending-order cache | per entry: name bytes + twice `(u64, String)`; per inbox: twice Address charge + BTreeMap header | 128 MiB independent cap; spill whole inbox groups to the declared store index |
| Open values, receipts, LRU and charge metadata | `resource()` below, plus owned receipt rows | shares 128 MiB with controls; evict least-recently-used values only |
| Inbox bindings/unbound clocks and consumer keys | Address + binding header + 128 B outer-map slack; nonempty consumer set adds 512 B node allowance + twice receiver charge; registered key adds receiver charge + 64 B | 96 MiB setup ceiling within shared 128 MiB; required live clocks retained, values evicted first |
| Temporary touched/read sets | keys touched by the current operation; only declared evidence keys copied with versions | released after operation; source/port observations themselves are caller-owned |

The normal dynamic allowance is **256 MiB**: 128 MiB for Pending-order keys
and 128 MiB shared by values and controls. The explicit control setup ceiling
is 96 MiB within that share. Required live-control charges can exceed a smaller
custom target. Historical-only unbound inboxes charge zero. Residency reports
value/control charges, effective setup ceiling and Pending cache usage/cap.

`Address` charge is `size_of(Address) + 4 * canonical_len + 2 * segment_slice_bytes`.
Parsed String
capacity and segment Vec spare capacity are included in that charge. `receiver` includes its
header, id/host capacity and node slack. Resource accounting lists every
MessageSpec field explicitly. Fixed resource/map headers are counted once,
with two BTree slots per live row for Home, LRU and size metadata; add three
owned key strings, address heap allocations, frozen canonical bytes and typed
body/Arc storage, reference capacities/addresses, optional refs/conditions and
owned receipt/state payloads. Adding a spec field fails compilation here.

## Measured calibration and ceiling

2026-10-19, development profile, System allocator requested live Rust bytes.
Calibration uses 1,000 independent items per shape; distinct body allocations,
actual BTree maps and metadata are included. Native SQLite memory is excluded.
The allocation target asserts charge >= measured allocation, rather than just
asserting that accounting counters stay below a cap.

| Item | Allocated B/item | Charged B/item |
|---|---:|---:|
| Parsed Address, canonical length 23 | 241 | 292 |
| Cloned Address, length 23 | 168 | 292 |
| Parsed Address, length 142 | 584 | 768 |
| Cloned Address, length 142 | 406 | 768 |
| Parsed / cloned Address, length 2,062 | 6,344 / 4,246 | 8,448 |
| Parsed / cloned three-segment Address, length 790 | 1,992 / 1,726 | 3,424 |
| Parsed / cloned artifact Address with escaped subject, length 1,101 | 2,366 / 2,360 | 4,732 |
| Open resource, empty text, Home + LRU + metadata | 2,142 | 2,426 |
| Same, distinct 2 KiB text | 6,238 | 6,524 |
| Same, distinct 8 KiB text | 18,526 | 18,812 |
| Convoy request body, one promise and quota | 3,785 | 4,289 |
| 2 KiB text plus one receipt and receipt maps | 6,999 | 7,487 |
| Unbound clock with outer map | 368 | 483 |
| One-consumer binding with outer map | 1,005 | 1,309 |

The one-consumer binding measurement includes the BTreeSet node allocation.
These measurements calibrate the tested address/body/binding shapes; larger
fanout, reference/request shapes and different address envelopes must continue
to satisfy accounting. The formula includes their owned capacities explicitly.

**Full ceiling Pending measurement:** 500,000 names of 91 bytes distributed
across 46,000 inboxes allocated **86,677,530 B** and charged **108,691,120 B**,
below the 134,217,728 B cap. Both keys and outer inbox nodes were measured.
Using 80-byte inbox addresses instead gives a conservative charge of about
127.6 MB, still below the cap; longer key/address shapes are an expiry input.
At 46,000 one-consumer bindings and 36,000 consumer keys, the measured-name
charge is about 68.1 MB; with 80-byte inboxes it is about 77.9 MB, below 96 MiB.
That leaves about 56.3 MB of the value/control share for open values. With
80-byte sender/inbox addresses and 2 KiB bodies (about 6,980 charged B/value),
roughly 8,000 values fit while all 500,000 Pending keys fit this measured shape.
With all 46,000 inboxes live but unbound, clocks charge about 22.2 MB
for measured addresses, or 32.0 MB for 80-byte addresses; empty unbound inboxes
charge zero. The open count is included in every binding measurement.
Actual coverage time is reported from the retained values' state timestamps;
no days/seconds of retention are promised.

Reserve about 16 KiB of fixed replica headers/minimum BTree-node slack outside
the dynamic charges; sparse singleton trees cannot amortize node slots like the
calibration's populated maps. Source ownership is separate: SQLite rows/history remain on disk; bounded
string/identity caches at their default 65,536 entries in two generations can
use roughly 100 MB for short names. Reserve up to about 128 MiB for startup,
source-page and gate transients: startup holds 32 bodies, frozen documents can
be up to 1 MiB each with typed plus canonical allocations, and the source log's
existing encoded page budget is 32 MiB. Each watcher reads its own pages through one shared reader handle, which
serializes source reads; pages are not shared between subscribers. Each subscriber retains at most 16 decoded metadata changes (no
bodies); at 2 KiB/change, 500 subscribers hold about 16 MiB. Large metadata or
first-oversized source events are transient outliers and a sizing expiry, not a
second retained journal. These allowances plus the replica are about 500 MiB
of the daemon's 1 GiB ceiling; other machines/ports/evidence need the remainder.
This is an ownership model, not a measured whole-process RSS guarantee.

## Access defaults and costs

| Query | Deliberate route | Cold cost |
|---|---|---|
| enqueue | resident binding/clock and read set; transaction-assigned admission | source sequence for opaque name; one create transaction; no pre-read for admission |
| reply/supersede/dead-letter existence | resident identity or source resolve | resolve only, never terminal body/history |
| pending | complete cached inbox order, or declared `pending_by_inbox` fallback | cache: one keyed load per absent item; fallback: one home-scoped indexed resource above the last admission per pull; indexed receipts for cold values |
| receive/deliver/settle/drop | typed cached value and receipts/evidence | keyed `resolve` + `load`, receipt `about` if needed; no Settled/Dropped or source-history read for steady-state delivery |
| bind | same Pending cache/fallback; inspect candidates until receipt hit | one value read + receipt query per absent candidate; linear backlog under writer, then receipt wait outside lock (#3491) |
| unbind | resident binding | no store read |
| explicit terminal get | key | resolve + typed load, then discard |
| watch | separate source reader, authoritative `changes_since` | at most 16 entries/store page under existing byte cap; non-create attribution resolves original create, reused within page |
| restart | home-scoped `Read::scan` for open values/clocks; declared Pending index for order | 32 resources/scan page plus key-only Pending-index pages of 32 (no second body decode); recover open receipts and repair reactions |

Key-only index reads project id and admitted_seq, with the home predicate applied
before the limit. Fallback bodies decode once per returned candidate; already
emitted admissions and foreign-home rows incur no body loads.

## Small read-count probes

A 32 KiB value/control allowance, eight-entry source caches and distinct 2 KiB
bodies retain four values at 32, 64 and 96 stored open messages. Value/control
charge stays 27,643 B; Pending charges are 3,600 / 6,576 / 9,552 B. Live Rust
bytes are 210,633 / 213,433 / 217,129. Pulling 96 messages makes 96 value loads,
96 receipt queries and no terminal loads, scans or watch reads.
Cold bind over three messages makes three loads and three receipt queries.
Restart over three terminal messages uses one scan page, no receipts and no
retained values or unbound clocks. These are read-count probes, not throughput
or fsync benchmarks; startup I/O still grows with history.

At 500 watches and a 10,000/s all-kind burst, independent log reads can examine
up to five million source entries/s. That fanout is not benchmarked as sustainable.
Expiry conditions and policy decisions live in the implementation record.
