# Delta Snapshots Design

Issue: Step 3 of backend daemon plan | Prereq for: #33 (multi-host)

## Motivation

Full snapshots are broadcast on every refresh cycle. This works over a Unix socket but won't scale to TCP/multi-host. Delta snapshots reduce wire traffic, enable event-sourced replication (natural fit for raft/consensus), and let the TUI skip unnecessary table rebuilds when nothing meaningful changed.

Priority order: multi-host foundation, architectural completeness, bandwidth, UX.

## Core Concept: Event-Sourced Delta Log

Deltas are the primary representation. Snapshots are materialized views of accumulated deltas. The daemon maintains a bounded delta log per repo; clients subscribe from a position in the log and receive replayed deltas or a full snapshot to catch up.

This extends naturally to multi-host: the delta log is the replication unit.

## Delta Types (in `flotilla-protocol`)

### EntryOp — generic operation on a keyed collection entry

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op")]
pub enum EntryOp<T> {
    Added(T),
    Updated(T),
    Removed,
}
```

### Change — a single mutation, Kafka-style (key in envelope, not in value)

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Change {
    Checkout { key: PathBuf, op: EntryOp<Checkout> },
    ChangeRequest { key: String, op: EntryOp<ChangeRequest> },
    Issue { key: String, op: EntryOp<Issue> },
    Session { key: String, op: EntryOp<CloudAgentSession> },
    Workspace { key: String, op: EntryOp<Workspace> },
    Branch { key: String, op: EntryOp<Branch> },
    WorkItem { identity: WorkItemIdentity, op: EntryOp<WorkItem> },
    ProviderHealth { provider: String, op: EntryOp<bool> },
    ErrorsChanged(Vec<ProviderError>),
}
```

### SnapshotEvent — replaces current `DaemonEvent::Snapshot`

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SnapshotEvent {
    Full { seq: u64, snapshot: Snapshot },
    Delta { seq: u64, prev_seq: u64, changes: Vec<Change> },
}
```

## Key-Free Value Types

Current protocol types embed their own keys redundantly (e.g., `Checkout` has `path`, `ChangeRequest` has `id`). The delta envelope carries the key, so value types become pure payloads:

| Type | Key removed | Key location |
|------|------------|--------------|
| `Checkout` | `path: PathBuf` | `IndexMap<PathBuf, Checkout>` key / `Change::Checkout { key }` |
| `ChangeRequest` | `id: String` | `IndexMap<String, ChangeRequest>` key / `Change::ChangeRequest { key }` |
| `Issue` | `id: String` | `IndexMap<String, Issue>` key / `Change::Issue { key }` |
| `CloudAgentSession` | `id: String` | `IndexMap<String, CloudAgentSession>` key / `Change::Session { key }` |
| `Workspace` | `ws_ref: String` | `IndexMap<String, Workspace>` key / `Change::Workspace { key }` |

This ripples through the codebase — code that reads `checkout.path` or `issue.id` from the value must get the key from the map or envelope instead. Pure refactor, no behavior change.

## Branch Unification

Replace two unkeyed lists:

```rust
// Before
pub remote_branches: Vec<String>,
pub merged_branches: Vec<String>,

// After
pub branches: IndexMap<String, Branch>,
```

```rust
pub struct Branch {
    pub status: BranchStatus,
}

pub enum BranchStatus {
    Remote,
    Merged,
}
```

Keyed by branch name. Fits the `Change::Branch { key, op }` pattern. Status can grow (e.g., `Stale`, `Protected`) without structural changes.

## DeltaSource Trait — Per-Provider Streams

Each provider category gets a `DeltaSource` that produces deltas for its collections. Per-provider streams are merged into a unified delta log by the daemon.

```rust
/// Produces deltas for a single keyed collection.
trait DeltaSource<K, V> {
    fn compute_deltas(
        &self,
        prev: &IndexMap<K, V>,
        curr: &IndexMap<K, V>,
    ) -> Vec<(K, EntryOp<V>)>;
}
```

Default implementation diffs two IndexMaps:
- Keys in `curr` but not `prev` → `Added`
- Keys in `prev` but not `curr` → `Removed`
- Keys in both, values differ → `Updated`

Provider category to collection mapping:

| Provider Category | Collections | Key Type |
|---|---|---|
| Vcs | `checkouts`, `branches` | `PathBuf`, `String` |
| CodeReview | `change_requests` | `String` |
| IssueTracker | `issues` | `String` |
| CodingAgent | `sessions` | `String` |
| WorkspaceManager | `workspaces` | `String` |

A provider that natively emits deltas implements `DeltaSource` to return its accumulated deltas directly, ignoring the `prev`/`curr` inputs.

Work item deltas are derived: when raw provider data changes, re-correlate affected groups and diff the resulting work items.

## Delta Log & Materialized State

Per-repo state in the daemon:

```rust
struct RepoState {
    materialized: Snapshot,
    previous: Option<Snapshot>,       // for diff-based DeltaSource
    delta_log: VecDeque<DeltaEntry>,  // bounded, ~16 entries
    seq: u64,
}

struct DeltaEntry {
    seq: u64,
    prev_seq: u64,
    changes: Vec<Change>,
}
```

### On refresh

1. Each `DeltaSource` produces deltas against `previous` vs new provider data
2. Re-correlate affected work items, diff to produce work item deltas
3. Combine all into `Vec<Change>`
4. Append `DeltaEntry { seq+1, seq, changes }` to `delta_log`
5. Apply changes to `materialized`
6. Rotate `materialized` → `previous`
7. Broadcast `SnapshotEvent::Delta { seq, prev_seq, changes }`
8. Trim `delta_log` to capacity

### Delta vs full decision

The daemon computes the delta. If the delta is larger than a full snapshot (e.g., bulk refresh replaced everything), it broadcasts a full snapshot instead. Clients handle both.

### On `get_state` request

Returns current `materialized` snapshot — unchanged from today.

## Protocol & Wire Changes

### DaemonEvent evolves

```rust
enum DaemonEvent {
    SnapshotFull { seq: u64, repo: PathBuf, snapshot: Box<Snapshot> },
    SnapshotDelta { seq: u64, prev_seq: u64, repo: PathBuf, changes: Vec<Change> },
    RepoAdded(Box<RepoInfo>),
    RepoRemoved { path: PathBuf },
    CommandResult { repo: PathBuf, result: CommandResult },
}
```

### Subscribe gains seq map

```rust
async fn subscribe(
    &self,
    last_seen: HashMap<PathBuf, u64>,
) -> broadcast::Receiver<DaemonEvent>;
```

On subscribe, the daemon checks each repo's delta log and sends replayed deltas or a full snapshot to catch the client up, then the live broadcast takes over.

### Broadcast model

Eager/single delta: daemon computes one delta (against `seq - 1`), broadcasts it to all clients. Clients that missed events request full re-sync. Matches the current `tokio::broadcast` architecture.

## Client-Side Materialization

### SocketDaemon

Maintains a local materialized `Snapshot` per repo:

```rust
struct ClientRepoState {
    snapshot: Snapshot,
    seq: u64,
}
```

- `SnapshotDelta` → apply changes to local snapshot, bump seq
- `SnapshotFull` → replace wholesale
- `get_state` → return local copy, no round-trip

### TUI

`App::handle_daemon_event`:

- `SnapshotFull` → same as today, full `apply_snapshot`
- `SnapshotDelta` → apply changes to repo's `ProviderData` and `work_items`, rebuild table view

Change-detection badge becomes trivial: any non-empty delta on an inactive tab sets `has_unseen_changes`.

Future optimization: skip table rebuild when delta contains no work item changes.

### InProcessDaemon

Already owns the materialized state. Gains the delta log and emits `SnapshotDelta` events instead of `SnapshotFull`. The `DaemonHandle` trait methods are unchanged.

## Implementation Strategy

### PR 1: Structural refactor (no behavior change)

- Extract keys from value types (`Checkout`, `ChangeRequest`, `Issue`, `CloudAgentSession`, `Workspace`)
- Unify `remote_branches` / `merged_branches` into `branches: IndexMap<String, Branch>`
- Add `EntryOp<T>`, `Change`, `SnapshotEvent` types to protocol
- Update all code that accesses embedded keys to use map keys instead
- All tests pass, identical runtime behavior

### PR 2: DeltaSource trait and diff logic

- Implement `DeltaSource` trait with default IndexMap diff
- Wire up per-provider delta sources in daemon
- Compute deltas on refresh, but still broadcast full snapshots
- Verify deltas are correct by asserting `apply(prev, deltas) == curr` in tests

### PR 3: Delta log and broadcast

- Add `DeltaEntry`, `VecDeque` delta log to `RepoState`
- Switch broadcast from `SnapshotFull` to `SnapshotDelta`
- Delta-vs-full size decision
- Update `DaemonEvent` enum

### PR 4: Client-side materialization

- `SocketDaemon` applies deltas to local state
- Subscribe with `last_seen` seq map
- Delta log replay on reconnect
- `InProcessDaemon` updated to emit deltas

### PR 5: TUI integration

- `handle_daemon_event` handles both `SnapshotFull` and `SnapshotDelta`
- Incremental table view updates
- Simplified change-detection badge

## Key Design Decisions

- **Deltas are primary, snapshots are materialized views.** Event-sourced model that extends to raft/consensus for multi-host.
- **Per-provider delta streams merged into unified log.** Each provider category produces deltas independently. Native delta providers can replace diff-based ones incrementally.
- **Key in envelope, not in value.** Kafka-style: `Change::Checkout { key, op }` — value types are pure payloads.
- **Diff-based DeltaSource first, native later.** Design for native delta emission, implement via snapshot diffing initially.
- **Bounded delta log (~16 entries), retain only previous snapshot for diffing.** Seq gap > log window triggers full re-sync.
- **Eager single delta broadcast.** One delta computed, broadcast to all clients. Clients that lag request full re-sync.
