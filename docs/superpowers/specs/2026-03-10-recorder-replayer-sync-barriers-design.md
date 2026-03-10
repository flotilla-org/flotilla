# Replay Framework: Separate Recorder/Replayer + Sync Barriers

Issue: #183

## Problem

The replay framework has two structural limitations:

1. The old `Session` struct handled both recording and replaying in a single struct gated by a `recording: bool` flag, coupling two distinct concerns.
2. Interactions must be consumed in exact fixture order. This is fragile when providers make concurrent requests via `tokio::join!` — the order between independent calls is non-deterministic.

## Design

### Channel Labels

Channel labels identify the logical channel an interaction belongs to, derived automatically from interaction data:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Hash, Ord, PartialOrd)]
pub enum ChannelLabel {
    Command(String),    // executable name: "git", "tmux"
    GhApi(String),      // endpoint path
    Http(String),       // URL host
}
```

`Interaction` gets a `channel_label(&self) -> ChannelLabel` method that extracts the label from its data (command name, endpoint, URL host).

This means multiple calls to the same executable (e.g. several `git` commands) share a channel and stay FIFO-ordered within a round. This is intentional — sequential calls to the same tool typically depend on each other. Similarly, two GhApi calls to the same endpoint (e.g. paginated requests) stay ordered, while calls to different endpoints are independently reorderable. `Ord` is derived for deterministic serialization ordering when the recorder writes YAML.

### Round Model

A fixture consists of a sequence of rounds. Within a round, interactions on the same channel are ordered (FIFO), but interactions on different channels can be consumed in any order. Barriers separate rounds — all interactions in round N must complete before round N+1 begins.

### Recorder

```rust
pub struct Recorder {
    rounds: Vec<Vec<Interaction>>,  // completed rounds
    current: Vec<Interaction>,       // in-progress round
    masks: Masks,
    file_path: PathBuf,
}
```

- `record(interaction)` — masks and appends to `current`.
- `barrier()` — pushes `current` into `rounds`, starts a new vec.
- `save()` — finalizes current round, serializes all rounds to YAML. Sorts interactions within each round by channel label for deterministic diffs.
- Wrapped in `Arc<Mutex<>>`.

### Replayer

```rust
pub struct Replayer {
    rounds: VecDeque<Round>,
    masks: Masks,
}

struct Round {
    queues: HashMap<ChannelLabel, VecDeque<Interaction>>,
}
```

- `next(label)` — pops front of the matching channel queue in the current round. Unmaskes placeholders. Panics with diagnostics if label not found.
- After each `next()`, checks if all queues in the current round are empty and auto-advances to the next round.
- `assert_complete()` — panics if any rounds or interactions remain unconsumed.
- Wrapped in `Arc<Mutex<>>`.

### Session Enum

```rust
pub enum Session {
    Recording(Arc<Mutex<Recorder>>),
    Replaying(Arc<Mutex<Replayer>>),
}
```

- `next(label)` — delegates to `Replayer::next()`. Panics if called in recording mode.
- `record(interaction)` — delegates to `Recorder::record()`. Panics if called in replay mode.
- `barrier()` — records a barrier (recording mode), no-op in replay mode (barriers are structural in the YAML).
- `finish()` — delegates to `save()` or `assert_complete()`.
- `is_recording()` — returns true if in recording mode.

`test_session()` returns `Session`. Factory functions (`test_runner`, `test_gh_api`, `test_http_client`) match on the enum to create the appropriate adapter.

Uses `std::sync::Mutex` (not `tokio::sync::Mutex`) — the lock is held only briefly for queue operations and round-advance checks, and async-awareness is unnecessary.

**Example test with barriers:**
```rust
#[tokio::test]
async fn concurrent_providers_with_barrier() {
    let session = replay::test_session(&fixture("multi_provider.yaml"), masks);
    let runner = replay::test_runner(&session);
    let http = replay::test_http_client(&session);

    // Round 1: git and http happen concurrently
    let (branches, sessions) = tokio::join!(
        vcs.list_local_branches(&repo),
        agent.list_sessions(),
    );

    session.barrier(); // no-op in replay, marks round boundary in recording

    // Round 2: depends on round 1 results
    let details = agent.get_session(&sessions[0].id).await;

    session.finish();
}
```

### Adapter Changes

Adapters remain split as today (`ReplayRunner`/`RecordingRunner`, etc.). The change is:

- Replay adapters call `session.next(ChannelLabel::Command("git".into()))` instead of `session.next("command")`. The label is derived from the request being made.
- Recording adapters call `session.record(interaction)` as before. The recorder derives the label from the interaction when organizing into rounds.
- Factory functions accept `&Session`.

### YAML Format

**Existing format (single implicit round):**
```yaml
interactions:
- channel: command
  cmd: git
  ...
- channel: gh_api
  endpoint: repos/owner/repo/pulls
  ...
```

**New multi-round format:**
```yaml
rounds:
- interactions:
  - channel: command
    cmd: git
    ...
  - channel: http
    url: https://api.claude.ai/v1/sessions
    ...
- interactions:
  - channel: gh_api
    endpoint: repos/owner/repo/pulls
    ...
```

The loader detects which format by checking for `rounds:` vs `interactions:` at the top level. Old format deserializes into a single round.

### Error Diagnostics

- **Channel not found in round:** "Expected interaction on channel `Command("git")` but round 2 only has channels: `GhApi("repos/owner/repo/pulls")`, `Http("api.claude.ai")`. Did you miss a barrier?"
- **Unconsumed at finish:** "Replay incomplete: round 3 has 2 remaining interactions on `Command("git")`"

### Migration

- Existing fixtures work unchanged — flat `interactions:` is treated as a single round.
- Existing test code works unchanged — no `barrier()` calls means single-round behavior, which is more permissive than today (cross-channel reordering allowed).
- Factory function call sites accept `&Session`.
- Test helper functions like `build_api_and_runner` (in `code_review/github.rs` and `issue_tracker/github.rs`) also need signature updates.
- Re-recording an existing test (`RECORD=1`) will produce the new `rounds:` format (single round) rather than the old flat `interactions:` format. Both are supported on load.

## Out of Scope

- Explicit channel label overrides / `IntoChannelLabel` trait (future).
- Automatic barrier detection from async boundaries.
- Migration tool to convert flat fixtures to round-based format.
