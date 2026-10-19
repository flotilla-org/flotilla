# Inbox bounded filtered replica

The Message inbox uses store-independent admission order, a Pending-only index,
an owned create hook, borrowed observation snapshots and rebuildable Home values.
The [component workload](../../crates/flotilla-inbox/WORKLOAD.md) owns the size
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
without retaining a separate set of spilled inbox names. A nonempty inbox whose
order has spilled continues reading the store until it empties; recovery may
cache its order again if it fits. Mid-stream spill uses an exclusive admitted_seq
lower bound, so previously emitted rows are never decoded again.

Settled/Dropped values leave Home after the operation. An explicit terminal read
is keyed; historical state is decoded from the source entry, never from today's
terminal body. Reply, supersession and dead-letter existence checks use resolve
without loading those resources. Receipts use the master's declared about index.

Restart streams the home-scoped scan for open values, live inbox counts/clocks,
receipts and interrupted Undeliverable reactions. It rebuilds Pending order from
key-only reads of the declared index, never from scan order or a second body
decode. The store applies the home filter before its page limit. Only values and
inbox-order groups that fit their cache allowances are retained. Bindings remain local observations.
An Unbound clock starts when an empty inbox gains its first open message, and
recovery creates one only for inboxes with open messages. When the last open
message settles or drops, an unbound inbox releases its clock; refilling starts
a fresh period. Bound inbox behavior is unchanged. A missing required clock is
a returned error rather than a panic.

Values are evicted before required live controls. Admission and restart retain
those controls even when a custom target is too small; setup of explicit bindings
and consumer keys has a capacity check. A binding update beyond the
setup ceiling returns a Storage capacity error before mutating the binding; it
does not silently degrade or discard a receiver. This unavoidable live-state
floor is sized against live inbox cardinality, not terminal history. Actual oldest
and newest retained state times describe coverage; no retention horizon is fixed.
Oversized values remain cold.

## Approved engine and storage seams

Home decides admission. Log::create_resource owns the admitted resource,
persists it, and returns exactly the committed resource to Home. Its default
delegates to Log::create for other machines. MessageLog specializes this
hook: it assigns admitted_seq inside the same Store lock/write transaction as
insert and commit. It changes only persistence metadata, not gate-read fields,
identity, state or recorded inputs. The memory source assigns admission under
its own authoritative storage lock.

Observations::with_snapshot borrows one consistent generation. The adapter
uses Table::select to copy only the declared Message read set, including exact
versions and absences; debug/test Source lookups assert membership. A missing snapshot is a
returned error. Home::evict removes only reconstructible values, leaving their authoritative log intact.

## Watches and cursor loss

Watches read the authoritative source through a separate reader handle. They do
not acquire the inbox writer mutex or its worker admission lock. Readers await
one shared handle before submitting blocking work; decoded source pages are not
retained as a journal. Store serialization can still contend with commits, but a
long engine decision does not prevent a source read. Source page limits bound
reads. Memory uses the equivalent authoritative read.

Lag and restart alone do not expire retained cursors. The cursor-loss
policy is deliberately conservative: **any missing home sequence returns
CursorExpired, including a sequence removed by a logged expunge**. Thus one
resource's expunge expires older cursors at that home even when their subscribers
watch unrelated inboxes. Logged expunge is not distinguished from
unexplained loss. Resume after the gap can work; clients needing an older
baseline must resynchronize.

## Expiry

Revisit this shape when cold reads, reader fanout, startup or bind latency miss
the deployment budget, when live controls exceed the workload envelope, or when
new address/body shapes fail allocation calibration. Cache exhaustion itself uses
the declared fallback rather than requiring admission refusal. Large-backlog
cold-bind scheduling remains #3491; a new index requires owner look-over.
The memory backend's authoritative store remains in-memory and is separate from
the filtered replica's sizing. Archive, expunge policy and disk-growth measurement
remain separate work.
