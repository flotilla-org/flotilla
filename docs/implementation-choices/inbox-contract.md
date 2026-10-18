# Inbox bounded filtered replica

The [component workload](../../crates/flotilla-inbox/WORKLOAD.md) contains the
size formula, ceiling arithmetic, measurements and every query's default route.
The public seam remains `InboxBackend`; its operations and streamed results are
unchanged. Foreign backends need not use the engine or these storage reads.

## Resident query cover

`Home<Message>` holds only open typed values that fit its byte budget. The inbox
adapter owns the budget, receipts, maintained pending index and LRU; Home adds
only explicit evict/restore operations. Other engine machines retain their
existing behavior. Settled/Dropped state is read by key and discarded after
use. History stays in the source, never on the resource or in a second journal.

The resident pending index is complete until a budget eviction. At that point it
is dropped and pending reads use `Read::page` on the approved
`pending_by_inbox(inbox, admitted_seq) WHERE Pending`. Coverage cannot silently
become a partial result. This conservative instance-wide cover was chosen over
per-inbox eviction metadata: it needs no unbounded set of negative coverage
entries and no scan to prove that an inbox is complete. Its cost is that cold
pending stays the default after overflow until reopening; improving that cost
requires a measured per-inbox cover design, not an incomplete resident answer.

The home stamps `admitted_seq` with its next create sequence, overwriting caller
input; both logs refuse zero or a sequence mismatch. SQLite verifies inside the
write transaction, so a competing home write causes a clean refusal instead of
persisting the wrong order. Replication preserves the column. The partial index
replaces the unused mixed open index; no speculative Awaiting index is added.
Pending has no self edge and its immutable admission never changes on delivery,
settlement or drop. Source-local sequence uniqueness supplies the order within
one home; neither message names nor replica-local surrogate ids do.

## Budget and ownership

The default dynamic replica allowance is 128 MiB within the root 1 GB daemon
budget. Charges cover open values, Frozen bytes and typed body capacity, reference
collections and strings, receipt fanout, pending/LRU/size-map keys and BTree slack,
local binding observations and registered consumer keys. The implementation
conservatively double-counts shared body Arcs. At roughly 7.7 KB for a measured
2 KB message, a fully resident 500,000-message ceiling would cost about 3.8 GB;
that candidate is rejected rather than hidden behind the word "reference".

Changed values are charged after every operation, terminal values removed, and
oldest-used open values evicted until the budget holds. On a miss, open state and
receipts return through keys/`about`. Larger-than-budget messages are operated on
transiently and immediately become cold again. Local controls are irreducible
observations, so excess new bindings/registrations are refused before mutation
rather than forgotten. Never-bound inboxes use their pending state's entry time
(or reopening time) without allocating a control row. Explicit unbind retains its
actual time, preserving the timeout gate. Empty pending-inbox containers are
removed so distinct settled inboxes cannot leak an index per historical inbox.

Fixed engine definitions/headers, authoritative storage caches and caller-owned
ports/evidence are separate ownership, with estimates and transient limits in the
workload. The memory implementation is an authoritative in-memory store plus the
same bounded replica: its audit storage necessarily grows until the owner
expunges, just as an in-memory SQLite database does. The production budget assumes
a disk Store, not unbounded audit history in a memory backing store.

`residency_blocking` reports the accounted bytes, cover and actual oldest/newest
resident state-entry times. There is no invented retention duration. Startup scans
32 current resources at a time and rebuilds open receipts through declared `about`
reads. It never collects all resources or all receipt history into a recovery Vec.
Dropped Undeliverable sources finish deterministic dead-letter reactions one at a
time; already-existing dead letters are recognized by key, including terminal
ones. Deterministic private names identify their Notify mode without a growing
provenance set of historical names. Ordinary system messages retain caller rules.

## Authoritative watch and gate evidence

A watch establishes a source-log position, then reads `changes_since`. There is
no 65,536-entry retained memory journal, duplicate history or restart generation.
Both backends have the equivalent source read. Lag/restart alone do not expire
cursors. An actual missing sequence, such as history removed by the store's
existing expunge, explicitly returns CursorExpired. The backend adds no trimming.
Message state is decoded from that event's state effect; loading its latest state
would misreport an old Pending/Awaiting event after settlement and was rejected.
Inbox attribution uses the create event by resource key, never a terminal body scan.

Source pages have at most 16 entries, and a subscriber's decoded batch is charged
to a 16 KiB limit. A first oversized change is returned alone, so progress is
possible without retaining a queue of large changes. At the reference 500 clients,
normal decoded buffers therefore cost at most about 8 MiB, independently of log
length. Source decoding runs under the one writer and the Store's page-byte bound.
There is no backlog channel per watcher. Poke-before-pull notifications and a
cursor advance across unrelated entries prevent missed wakes and reread loops.
This source-read route intentionally replaces the earlier zero-read resident
journal; fanout arithmetic and its performance expiry are explicit in WORKLOAD.

Observations provide one consistent borrowed world. The adapter copies only the
message/inbox/condition/authority/supersession/reference read set with exact
versions, then releases the observation owner before deciding/persisting. Engine
`Table` no longer implements Clone, and `MemoryMessageWorld` no longer derives it.
`Table::select` is a deliberate narrow snapshot, not a renamed whole-world clone.
Bindings and own receipts overlay those selected observations for the same gate.

## Single writer and alternatives

One async writer mutex is acquired before submitting one blocking operation;
the state lock spans decision and log commit. No arbitrary 64 permits are reserved,
and SQLite never blocks an async runtime executor. Bind/drain and watch waits
remain outside the writer. Consumer ports must identify the same Store and home,
commit Receive before success, and publication requires a strictly increasing
source sequence. A poisoned writer requires reopening from its log.

Rejected candidates include whole-kind runtime filtering, a full resident world,
per-subscriber history queues, a bounded duplicate watch journal and gate-table
clones. Keeping a mixed open index as well as Pending was rejected by the owner
because it doubles enqueue inbox-index maintenance without a read path. Per-inbox
refill/cover optimization and byte-bounded shared log readers remain possible,
but only where measured cold/fanout cost justifies the extra metadata and seams.

## Evidence and expiry

The shared backend suite and its generated claim interleavings retain their
assertions. The testkit's evidence collaborator supplies borrowed snapshots;
shared suite logic is unchanged. Backend-specific tests now assert that durable
cursors survive lag/restart and that real source gaps expire. New properties and
scenarios cover tiny budgets, oversized bodies, empty/multi-page pending sets,
receipt recovery, keyed terminal reads, no terminal/history reads during steady
delivery, event-state replay and admission stamping. Engine tests prove selective
snapshots preserve versions and evict/restore leaves the log unchanged.

The separate allocator workload keeps its 512 KiB replica at 517,017 charged
bytes / 67 values while source cardinality grows from 2,000 to 6,000. Live Rust
allocations remain roughly flat with small bounded source caches. Cold streaming
uses declared pages/open reads and no terminal/resource history. These diagnostic
results do not claim disk fsync throughput; see WORKLOAD for commands and numbers.

Revisit this choice when cold Pending or source-watch cost exceeds latency/CPU
budgets, controls consume their memory allowance, unusually large metadata or
receipt sets dominate transients, or reference cardinalities change. The former
"before #3317" expiry of the full-resident reference query model is discharged:
this implementation removes it before the Message production switch. The source
of truth, admission column, public trait and streams keep further optimization
possible without changing the public contract or trimming audit data.
