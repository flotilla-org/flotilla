# Inbox bounded filtered replica

Implements #3466 before the Message switch (#3317). The owner approved the
stored admission column and Pending-only index, then approved the owned create
hook, borrowed observation snapshot and Home eviction seams. The
[component workload](../../crates/flotilla-inbox/WORKLOAD.md) owns the size
formula, measurements, ceiling arithmetic and access-cost numbers.

## Resident state and recovery

The adapter maintains a complete per-inbox Pending index, local inbox
bindings/clocks, and a recency-evicted cache of typed open Message values and
receipts. The Pending index has a separate budget: value eviction never drops
its keys or changes query coverage. Pending walks that index in home admission
order and loads each absent value once by resource key. There is no store-page
candidate decode followed by a second load, nor a cross-home candidate query.
The approved stored Pending index remains available to other store clients.

Settled/Dropped values leave Home after the operation. An explicit terminal read
is keyed; historical state is decoded from the source entry, never from today's
terminal body. Reply, supersession and dead-letter existence checks use resolve
without loading those resources. Receipts use the master's declared about index.

Restart streams the store's home-scoped scan, restores the Pending index and
inbox clocks, recovers open receipts, and retains only values that fit. It also
finishes interrupted Undeliverable reactions idempotently. Bindings are local
observations, as in the reference backend: recovery initializes one Unbound clock
per inbox at open. During a backend lifetime, the first successful enqueue starts
that inbox's clock; subsequent messages and value eviction do not reset it.
Unbinding the last consumer starts a new unbound period. The Quint specs and
Message gate semantics are unchanged.

Pending keys and irreducible controls are never silently evicted. Admission or
setup refuses new state before mutation if their budgets cannot represent it;
recovery refuses an over-budget rebuild. Values are evicted to respect the
remaining cache share. The reported oldest/newest state times describe actual
resident cover, not a configured time horizon. Oversized values remain cold.

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
copies only the declared Message read set, including exact versions and
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

## Acceptance and remaining expiry

InboxBackend operations and signatures, shared testkit-suite assertions and
Quint specs are unchanged. Two backend-specific contract.rs assertions were
changed for retained lag/restart cursors, and the large lag test is replaced by
small lag and restart cases. Recovery/receipt, claim, race and generated
conformance cases exercise both backends. Read auditing lives in test-side
wrapper logs, not production StoreLog. Charge calibration uses a separate live
Rust allocator target, including the full ceiling Pending index. Exhaustive
MessageSpec destructuring makes a new field fail to compile in accounting.

The previous full-resident workaround has expired and is removed: no copied
watch journal, worker-permit pool or engine-table Clone. Revisit this shape when
measured cold reads, reader fanout, startup or bind latency miss the deployment
budget, when key/control cardinality or address shape exceeds the workload, or
when the accounting calibration fails. Large-backlog cold-bind scheduling remains
tracked in #3491; a new index would require another owner look-over. The memory
backend's authoritative store is intentionally in-memory; its retained storage is
separate from the filtered replica and is not the daemon's SQLite sizing model.
