# Convoy event cost and read-pressure recovery (#3607)

## Workload

The [root workload model](../../WORKLOAD.md) supplies the scale: approximately
193 replicated Convoys in the reported incident; 1,000 running Convoys at the
target and 10,000 at the ceiling, across up to 500 hosts. Most records on a
host can be replicas. Convoys average 21 KB, with a 100 KB tail; the local
measurement uses 320 replicas carrying 30,000 bytes of instruction each.
Events arrive continuously: 1,000/s sustained and 10,000/s burst. Fleet-health
clients poll every five seconds, with up to 500 clients at the ceiling.

These are same-process read models over a local durable SQLite store. Replication
also crosses the peer boundary. No persisted resource shape or schema changes.
The existing full-replication inventory is retained; partitioning and compacting
that inventory are outside this stability fix.

## Operations and costs

Let N be fleet Convoys, L local Convoys, B the changed body size, K the number
of distinct changed Convoy keys in a coalescing interval, and D the changed
Convoy's child/subject evidence. Counts below exclude producing the incoming
event itself, which necessarily carries the changed body.

| Path | Previous work | New steady-state work | Store reads |
|---|---|---|---|
| Leaf, replica Convoy event | Sort/materialize N owned bodies, then filter for Local; rebuild all local rows | The view applies the event once; no local invalidation, local inventory list, or owned inventory copy | 0 after bootstrap |
| Leaf, local Convoy event | Materialize N bodies; derive rows for every local Convoy | Share the changed body; update its key and derive its rows and indexed child/CR selection, O(B + D) | 0 after bootstrap |
| Leaf, checkout/CR/vessel event | Derive rows for all L Convoys | Reverse dependencies and parent labels select affected parents; query only their child/subject evidence | 0 after bootstrap |
| Aggregator, ordinary Convoy event | Multiple N-body maps plus all-row summaries and salience | Two source-precedence lookups; retain previous/current Arc handles; update one source key; summarize only that Convoy with indexed child evidence | 0 |
| Aggregator, delivery | Whole inventory rebuild per event (some delivery already coalesced) | First-deadline 25 ms coalescing, K row updates; one salience pass per flush | 0 |
| Fleet health | Request snapshot copies Convoys, then fleet rows list them again | Use the Aggregator's event-maintained rows for addresses, placement and surface state; no Convoy store query or body materialization | 0 Convoy reads |
| Busy read | Return deadline failure into controller/replicator restart path | Retry that idempotent read in the same consumer: 25 ms exponential backoff window, capped at 1 s, with per-consumer upper-half jitter; cancellation drops the retry | One per attempt |
| Slow/deadline read log | A WARN/file write per occurrence | Shared per-backend counts, at most one summary per 30 s | n/a |

A whole Convoy-map diff remains necessary at initial snapshot/recovery and when
origin-to-host discovery changes which buffered replicas are projectable. A newly
seen unresolved origin is remembered rather than re-resolved on every event.
Secondary-input changes retain their existing coalesced full-projection boundary;
that boundary rebuilds indexes of child keys, not child-body copies, for subsequent
Convoy events. Salience joins sessions to indexed Convoy names instead of copying
or scanning Convoy bodies. Checkout orphan detection uses indexed parent membership.

The leaf view keeps a separate authoritative-object index of Arc handles. Replica
updates do not invalidate its local generation. Key notifications have a 1,024-entry
bound: overflow or a replacement snapshot explicitly requests a resync. A replica
burst does not publish local key notifications. Local resync selects local handles,
not replica bodies. Forge-definition changes can affect all local address resolutions
and therefore intentionally resync all local rows.

Fleet health still computes host-level health and freshness at request time and
reads other resource kinds. It does not cache time-sensitive answers indefinitely.
Fleet-list/scoped inventory operations retain their existing selection contracts.
Presentation result-set/scoped delivery costs, the SQLite JSON write triggers, and
other consumers' projection costs are not claimed to be solved here.

## Candidate walk and resource budget

Filtering an owned list rejects replicas too late: at the ceiling one leaf event
can copy 210 MB at the root mean, or 1 GB at the tail. With even 1,000 events/s
that is 210 GB/s of body copying before any judgement. Caching that list by a
kind-wide generation still invalidates on every unrelated replica event.

A second owned local inventory avoids replica copies but duplicates workflow and
instruction bodies. A local index of immutable Arc handles gives the same selection
without those copies and without a second store watch. A bounded key stream plus
reverse dependencies avoids re-deriving unrelated local rows.

The Aggregator already owns source maps and precedence. Sharing individual source
objects and updating their key avoids a second authoritative cache and preserves
precedence and soft-deletion semantics. Child evidence indexes contain keys only;
their cost follows child cardinality. Name/parent indexes add O(N + children) small
keys/handles, rather than O(N * body size) additional resident bodies. An Arc handle
is a machine word; 10,000 handles alone are about 80 KB on the reference 64-bit
hosts, excluding map/key allocation. The existing full inventories and their
memory ceiling are unchanged; this is not a claim that their total resident
representation meets the root's compact-form budget.

Busy retry is attached to continuous consumers, not interactive read APIs or
writes. Interactive reads still return a bounded busy refusal. Only the store's
specific busy-read message is classified as pressure, including across the existing
HTTP error boundary. SQLite producers and classification share a single message
constant/factory, tested using the real SQLite deadline error; the interactive
request deadline is deliberately a different category. Independently seeded
per-consumer jitter samples the upper half of each backoff window (initially
12.5–25 ms, eventually 0.5–1 s), avoiding synchronized retries. Invalid cursors, ended streams, missing access and real
storage/transport failures retain their existing error/recovery paths. Retry has
one outstanding read per consumer and a capped rate during prolonged pressure.

Cancellation ownership was audited by call-site family:

| Read-retry caller | Owner/cancellation boundary |
|---|---|
| Controller primary/secondary watches and row reads | Controller loop owns current watch tasks, including respawns, and aborts them on drop; row reads are awaited in the loop |
| WatchView and local/replica source recovery | Final view-handle drop aborts the worker; setup/recovery awaits the retry future directly |
| Direct/routed replication and digest reads | Peer-generation cancellation is selected against the entire supervised attempt, including read backoff; supervisor drop cancels its generations |
| Registry watch setup | Awaited by the caller; routed ResourceWatch commands select their cancellation token during setup as well as stream consumption |

The controller cancellation guard failed against the detached-watch implementation
and passes with owned tasks. A cancelled routed-watch setup guard also failed
before setup was included in the token select. Cancellation stops future read
attempts; a SQLite operation already executing retains its existing worker lifetime.

Assumptions: watch snapshot/replay handoff remains race-free; source precedence
is the existing fixed two-source order; a resource event replaces one complete
body; child evidence changes use the existing full-projection invalidation boundary.
These are rebuildable in-process indexes. The seams are WatchView key/Arc reads,
Aggregator source keys, and the shared read-retry helper. Revisit this choice if
those assumptions change or the root scale increases. No new API limits or stored
compatibility burden are introduced.

## Verification and measurement

The deterministic fixtures run in-process with 320 replica Convoys, each carrying
30 KB of instruction, and 20 successive events to one key. Setup is outside the
timed interval. Owned-body counters are at WatchView materialization and Aggregator
whole-map copying; a separate row-summary counter checks exactly one summary per
event. Leaf read counters cover the consumer's backend after bootstrap. Aggregator
source and host lookup collaborators are resident; the event path has no store.
Wall times are secondary, single unoptimized runs on this vessel; they include
fixture event submission and are not a CPU-throughput or fleet-host claim.

| Consumer | Bodies copied/event before → after | Store reads/event before → after | Wall time/event before → after |
|---|---|---|---|
| Leaf | 320 → 0 | 0 → 0 | 1.384 ms → 0.391 ms |
| Aggregator | 1,600 → 0 | 0 → 0 | 19.354 ms → 0.061 ms |

The final Aggregator guard additionally requires exactly one row summary per event.
Full output and measurement context are recorded in the PR evidence. The old Aggregator functions
were temporarily restored for the baseline, retaining the new counters and fixture;
the baseline failed the zero-copy assertion. Restoring the leaf materializing read
also failed its guard. Disabling busy-read retry failed the real SQLite writer
scenario. All temporary mutations were reverted.

Commands:

```sh
cargo test -p flotilla-crew --locked --lib replica_event_work_measurement -- --nocapture
cargo test -p flotilla-projections --locked --lib replica_convoy_event_work_measurement -- --nocapture
cargo test -p flotilla-core --locked --lib fleet_health_does_not_materialize_convoy_inventory
cargo test -p flotilla-store-sqlite --locked --lib busy_writer_does_not_restart -- --nocapture
```

The operator owns the post-roll b5 dummy-fleet load run, per the corrected brief.

Raw registry watch setup preserves SQLite's bounded read deadline. The routed
continuous watch consumer owns busy retry under its cancellation token; failed
setup drops its partial stream before retrying. Subscribe-before-list handoffs
use bounded `WatchChannel` rings (256 events per local/replica stream), and lag
returns `WatchExpired` rather than allocating an unbounded queue. A real SQLite
writer-pressure test failed before this separation and now checks the raw API
returns store-busy within three seconds. Prolonged retry observability is tracked
separately in #3615.
