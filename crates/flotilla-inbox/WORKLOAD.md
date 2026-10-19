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
| Complete per-inbox Pending index | per entry: name bytes + twice `(u64, String)`; per inbox: twice Address charge + BTreeMap header | 128 MiB independent cap; never evicted with values; refuse admission before writing if full |
| Open values, receipts, LRU and charge metadata | `resource()` below, plus owned receipt rows | shares 128 MiB with controls; evict least-recently-used values only |
| Inbox bindings/unbound clocks and consumer keys | Address + binding header + 128 B outer-map slack; nonempty consumer set adds 512 B node allowance + twice receiver charge; registered key adds receiver charge + 64 B | controls capped at 96 MiB within the shared 128 MiB; never silently forgotten |
| Temporary touched/read sets | keys touched by the current operation; only declared evidence keys copied with versions | released after operation; source/port observations themselves are caller-owned |

Total charged long-lived replica cap is **256 MiB**, not 128 MiB: complete
Pending keys have their own 128 MiB alongside the 128 MiB value/control share.
Control state reduces the value allowance; it does not force index eviction.
Recovery refuses a source whose required keys/controls exceed these caps.

`Address` charge is `size_of(Address) + 4 * canonical_len + 2 * segment_slice_bytes`.
The previous length multiplier 8 and fixed 128 B/segment are gone. Parsed String
capacity matters: multiplier 2 undercounted the long-address calibration, so 4
is retained. Segment slots allow Vec spare capacity. `receiver` includes its
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
| Open resource, empty text, Home + LRU + metadata | 2,142 | 2,426 |
| Same, distinct 2 KiB text | 6,238 | 6,524 |
| Same, distinct 8 KiB text | 18,526 | 18,812 |
| Convoy request body, one promise and quota | 3,785 | 4,289 |
| 2 KiB text plus one receipt and receipt maps | 6,999 | 7,487 |
| Unbound clock with outer map | 354 | 467 |
| One-consumer binding with outer map | 991 | 1,293 |

The one-element BTreeSet was measured, not treated as just a Receiver: omitting
its node allowance undercounted (624 B charged against 991 B allocated).
These measurements calibrate the tested address/body/binding shapes; larger
fanout, reference/request shapes and different address envelopes must continue
to satisfy accounting. The formula includes their owned capacities explicitly.

**Full ceiling Pending measurement:** 500,000 names of 91 bytes distributed
across 46,000 inboxes allocated **86,677,530 B** and charged **108,691,120 B**,
below the 134,217,728 B cap. Both keys and outer inbox nodes were measured.
Using 80-byte inbox addresses instead gives a conservative charge of about
127.6 MB, still below the cap; longer key/address shapes are an expiry input.
At 46,000 one-consumer bindings and 36,000 consumer keys, the measured-name
charge is about 67.4 MB; with 80-byte inboxes it is about 77.2 MB, below 96 MiB.
That leaves about 57.0 MB of the value/control share for open values. With
80-byte sender/inbox addresses and 2 KiB bodies (about 6,980 charged B/value),
roughly 8,200 values fit while **all 500,000 Pending keys remain resident**.
Actual coverage time is reported from the retained values' state timestamps;
no days/seconds of retention are promised.

Reserve about 16 KiB of fixed replica headers/minimum BTree-node slack outside
the dynamic charges; sparse singleton trees cannot amortize node slots like the
calibration's populated maps. Source ownership is separate: SQLite rows/history remain on disk; bounded
string/identity caches at their default 65,536 entries in two generations can
use roughly 100 MB for short names. Reserve up to about 128 MiB for startup,
source-page and gate transients: startup holds 32 bodies, frozen documents can
be up to 1 MiB each with typed plus canonical allocations, and the source log's
existing encoded page budget is 32 MiB. A shared reader decodes one source page
at a time. Each subscriber retains at most 16 decoded metadata changes (no
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
| pending | complete resident per-inbox ordered index | one keyed value load per absent item + indexed receipt read; no store Pending-page decode or other-home filtering |
| receive/deliver/settle/drop | typed cached value and receipts/evidence | keyed `resolve` + `load`, receipt `about` if needed; no Settled/Dropped or source-history read for steady-state delivery |
| bind | resident Pending index; inspect candidates until a receipt hit | one keyed load + receipt query per absent candidate; linear in backlog while writer is held, then receipt wait outside lock (#3491) |
| unbind | resident binding | no store read |
| explicit terminal get | key | resolve + typed load, then discard |
| watch | separate source reader, authoritative `changes_since` | at most 16 entries/store page under existing byte cap; non-create attribution resolves original create, reused within page |
| restart | home-scoped `Read::scan` | 32 current resources/page; open receipts via declared about index; reconstruct keys/clocks and repair reactions |

The unused store Pending-page path is removed. The approved durable Pending
index still exists and its schema/order tests remain; the inbox runtime's
Pending source is its resident index. This avoids decoding candidates twice
and excludes foreign-home rows before any value load, including colliding
names/admission numbers at a shared inbox address.

## Workload probes and expiry

With a 512 KiB value/control allowance, 128-entry source caches and 2 KiB distinct
bodies, 2,000/4,000/6,000 stored open messages retained 79 values and charged
518,317 B for values/controls. Pending charges grew 189,528/379,528/569,528 B
while remaining complete. Total live Rust bytes were 929,171/1,106,323/1,330,435:
expected index growth, not a full-body leak. Enqueue 6,000 took 3,673 ms; cold
pull 6,000 took 1,623 ms, with **6,000** value loads (formerly 12,000), 6,000
receipt queries and zero terminal loads/scans/watch requests.

Cold bind with 1,024 Pending values used 1,024 loads and 1,024 receipt queries
and about 195 ms; there is no store-index page decode. Startup over 2,048
terminal messages took about 88 ms and 64 scan pages, no receipt queries and no
retained terminal values. These are warm in-memory SQLite development timings,
not fsync latency or a 500,000-row startup latency claim. Startup I/O still grows
with history, even though retained state is budgeted.

The test-side read-count guard delivers replies to a settled parent after a
Settled/Dropped backlog: warm delivery reads no values/receipt history; forced
cold delivery reads open keys/receipts only; both perform resolve-only parent
checks with zero terminal loads/scans/watch requests. Shared suite and race
replay remain the gate boundary. Watches may contend for the Store mutex but
never the inbox writer lock; an intentionally paused decision tests this.

At 500 watches and a 10,000/s all-kind burst, direct log reads can examine up to
5 million source entries/s. That fanout is not benchmarked as sustainable.
Revisit on CPU/latency misses, large cold bind/startup costs, failed allocation
calibration, longer addresses/keys, or exhausted key/control budgets. Archive,
expunge policy and disk-growth measurement remain separate work.
