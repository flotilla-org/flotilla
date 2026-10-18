# Workload model: inbox contract

Inherits maturity, deployment tiers, message, running_convoys, change_events,
retention and budgets from the root [workload model](../../WORKLOAD.md).

## Operations and locality

This is the isolated engine-dependent contract from #3316, with no production
callers until #3317. Operations are local to one home; replication and remote
consumer adapters are outside this issue.

- Enqueue and edges: one engine decision and one committed log write. SQLite
  also reads its home sequence for the watch cursor. Consumer Receive is one
  write through the consumer's injected resource port.
- Gates: keyed resident message, binding and receipt lookups, plus an injected
  snapshot of observed evidence. No durable-store read serves a gate. The
  observation adapter must keep its snapshot resident.
- Pending: an inbox index ordered by numeric admission sequence, then per-consumer receipt lookup and rule evaluation.
  Results are a pull stream, materializing at most one message at a time.
  Notify and Mute write their receipt when that item is read; Turn is explicitly
  acknowledged after transport. Reading does not accept the Message Deliver edge.
- Watch: a shared resident delta journal, indexed by inbox and cursor, globally
  bounded to 65,536 changes. Each watcher holds one cursor and one item; there
  is no queue per watcher and no store read per watcher/event. Lagging readers
  receive CursorExpired and must obtain a fresh pending baseline.
- Startup: SQLite scans resources and logs in pages of 256, recovering messages,
  consumer receipts and journal positions. Bindings are local observations: the
  composition root rebinds consumers after restart; unbound time starts at
  recovery, never retroactively before it.

## Ownership, budgets and bounds

One writer lock spans decision and persistence. Blocking work runs on Tokio's
blocking pool behind 64 permits: callers wait before entering that pool. One
operation executes at a time. A binding waits outside that writer until observed
receipts of pending messages have their Deliver. Watches register notification
before reading to avoid lost wakeups. Cancellation drops a watch; there is no
server-side watcher registration to clean up.

At the root ceiling of 10,000 changes/s, the journal bridges about 6.5 seconds
if every change belongs to this backend. The 65,536 entries are shared: at 500
clients, memory is O(retained changes + clients), not O(changes * clients).
Pending and watch advance through BTree ranges rather than rescanning history
per item. Terminal messages leave the pending index immediately.

Home and MemoryLog retain resources/history for the lifetime of this backend.
This is a deliberately smaller **query model: full resident reference backend**,
not the daemon's retention implementation. At 50 messages per convoy this is
50,000 / 500,000 resources at target / ceiling. With the root's 2 KB message and
2x typed-memory factor that is about 200 MB / 2 GB before indexes, exceeding the
daemon's 1 GB budget at the ceiling. Its expiry is **before #3317 installs a
production caller**: use a compact open-message view, indexed cold reads for
terminal records, and the root archive policy. Neither public result needs to
change: pending and watch already stream, and cursor expiry is explicit. The
build-graph guard enforces the no-production-caller assumption.

## Implementation choices

A whole-kind scan per wake was rejected: at the ceiling it would inspect
500,000 messages per wake. A queue per watcher was rejected: 500 slow clients
would multiply the backlog. Re-querying SQLite per watcher/event was rejected
against the root zero-read delivery target. The shared inbox index and bounded
cursor journal serve those operations from resident state instead.

Foreign implementations lower operations themselves through InboxBackend,
without inheriting these storage/index choices. Cursors are home-local exclusive
positions; retention expires them explicitly. Rules are caller values and
create no stored schema. Message and consumer records retain their existing
schema and N-to-N+1 rules.

## Local measurement

A local development-profile sample on 2026-10-18 enqueued 1,000 distinct 2 KB
bodies into SQLite's in-memory database in 595 ms, then streamed their pending
items in 16 ms. This includes the blocking-worker hop and does not measure
filesystem/fsync latency. It supports the indexed pending path and worker-queue
cost; it does not claim the full resident reference model fits the daemon's
ceiling. The retention expiry above remains explicit.

## Consumer log scope and diagnostics

SQLite Receive ports must identify the exact Store and home shared by the inbox.
That scope and the receiver home are checked before committing Receive; publication
also refuses any non-increasing cursor. Foreign-store ports need a replication
boundary, which is outside this local backend. Memory assigns its own journal
positions and does not require its consumer MemoryLogs to share a sequence.

Construction is fallible through open. Synchronous register/get are setup and
diagnostic operations, used outside concurrent runtime work; runtime callers use
register_async/get_async through the bounded blocking worker. A poisoned writer
is fatal: reopen from durable state instead of resuming a possibly interrupted
consumer or persistence operation.
