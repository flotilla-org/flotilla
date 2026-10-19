# Inbox bounded filtered replica

Implements #3466 before the Message switch (#3317). The owner approved the
stored admission column and Pending-only index, then approved the owned create
hook, borrowed observation snapshot and Home eviction seams. The
[component workload](../../crates/flotilla-inbox/WORKLOAD.md) owns the size
formula, measurements, ceiling arithmetic and access-cost numbers.

## Resident state and recovery

The adapter maintains a budgeted cache of complete per-inbox Pending order,
local inbox bindings/live clocks, and a recency-evicted cache of typed open
Message values and receipts. Within budget, pending uses cached admission keys
and loads an absent value once. If a complete inbox order cannot fit, that inbox
uses the declared pending_by_inbox index by admission; a returned resource is
restored without another body load. Index identity metadata excludes other homes
before decoding bodies. Value eviction does not affect the independent key cache.
Cache pressure never refuses admission or recovery. Groups are cached whole,
with no unbounded set of spilled inboxes; a spilled group stays store-backed until
it empties or recovery considers it again. Mid-stream spill resumes after the
last emitted admission number.

Settled/Dropped values leave Home after the operation. An explicit terminal read
is keyed; historical state is decoded from the source entry, never from today's
terminal body. Reply, supersession and dead-letter existence checks use resolve
without loading those resources. Receipts use the master's declared about index.

Restart streams the home-scoped scan for open values, live inbox counts/clocks,
receipts and interrupted Undeliverable reactions. It rebuilds Pending order from
the declared index, never from scan order. Only values and inbox-order groups
that fit their cache allowances are retained. Bindings remain local observations.
An Unbound clock starts when an empty inbox gains its first open message, and
recovery creates one only for inboxes with open messages. When the last open
message settles or drops, an unbound inbox releases its clock; refilling starts
a fresh period. Bound inbox behavior is unchanged. A missing required clock is
a returned error rather than a panic. The Quint specs remain unchanged.

Values are evicted before required live controls. Admission and restart retain
those controls even when a custom target is too small; setup of explicit bindings
and consumer keys keeps its existing capacity check. A binding update beyond the
setup ceiling returns a Storage capacity error before mutating the binding; it
does not silently degrade or discard a receiver. This unavoidable live-state
floor is sized against live inbox cardinality, not terminal history. Actual oldest
and newest retained state times describe coverage; no retention horizon is fixed.
Oversized values remain cold.

## Approved engine and storage seams

Home still decides admission. Log::create_resource owns the admitted resource,
persists it, and returns exactly the committed resource to Home. Its default
preserves the old create behavior for other machines. MessageLog specializes this
hook: it assigns admitted_seq inside the same Store lock/write transaction as
insert and commit. It changes only persistence metadata, not gate-read fields,
identity, state or recorded inputs. There is no read-then-retry path, MessageSpec
downcast, or extra static bound in the shared consumer StoreLog. The memory
source assigns admission under its own authoritative storage lock.

Observations::with_snapshot borrows one consistent generation. The adapter
uses Table::select to copy only the declared Message read set, including exact versions and
absences; debug/test Source lookups assert membership. A missing snapshot is a
returned error, not a poisoned writer. Engine tables and MemoryMessageWorld no
longer implement whole-world Clone. Home::evict removes only reconstructible
values, leaving their authoritative log intact.

## Watches and cursor loss

Watches read the authoritative source through a separate reader handle. They do
not acquire the inbox writer mutex or its worker admission lock. Readers await
one shared handle before submitting blocking work; decoded source pages are not
retained as a journal. Store serialization can still contend with commits, but a
long engine decision does not prevent a source read. Source page limits apply;
there is no second decoded-byte budget. Memory uses the equivalent authoritative
read and stores no unused copies of accepted edges or admission input vectors.

Lag and restart alone do not expire retained cursors. The current cursor-loss
policy is deliberately conservative: **any missing home sequence returns
CursorExpired, including a sequence removed by a logged expunge**. Thus one
resource's expunge expires older cursors at that home even when their subscribers
watch unrelated inboxes. Logged expunge is not currently distinguished from
unexplained loss. Resume after the gap can work; clients needing an older
baseline must resynchronize. This issue adds no trimming, archive or expunge
policy. The home-wide consequence is tested explicitly.

## Expiry

Revisit this shape when cold reads, reader fanout, startup or bind latency miss
the deployment budget, when live controls exceed the workload envelope, or when
new address/body shapes fail allocation calibration. Cache exhaustion itself uses
the declared fallback rather than requiring admission refusal. Large-backlog
cold-bind scheduling remains #3491; a new index requires owner look-over.
The memory backend's authoritative store remains in-memory and is separate from
the filtered replica's sizing. Archive, expunge policy and disk-growth measurement
remain separate work.
