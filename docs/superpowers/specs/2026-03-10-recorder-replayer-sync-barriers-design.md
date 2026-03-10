# Replay Framework: Separate Recorder/Replayer + Sync Barriers

Issue: #183

## Problem

The replay framework has two structural limitations:

1. `ReplaySession` handles both recording and replaying in a single struct gated by a `recording: bool` flag, coupling two distinct concerns.
2. Interactions must be consumed in exact fixture order. This is fragile when providers make concurrent requests via `tokio::join!` — the order between independent calls is non-deterministic.

## Design

### Channel Labels

Channel labels identify the logical channel an interaction belongs to, derived automatically from interaction data:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ChannelLabel {
    Command(String),    // executable name: "git", "tmux"
    GhApi(String),      // endpoint path
    Http(String),       // URL host
}
```

`Interaction` gets a `channel_label(&self) -> ChannelLabel` method that extracts the label from its data (command name, endpoint, URL host).

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
- `save()` — finalizes current round, serializes all rounds to YAML.
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

- `barrier()` — records a barrier (recording mode), no-op in replay mode (barriers are structural in the YAML).
- `finish()` — delegates to `save()` or `assert_complete()`.

`test_session()` returns `Session`. Factory functions (`test_runner`, `test_gh_api`, `test_http_client`) match on the enum to create the appropriate adapter.

### Adapter Changes

Adapters remain split as today (`ReplayRunner`/`RecordingRunner`, etc.). The change is:

- Replay adapters call `session.next(ChannelLabel::Command("git".into()))` instead of `session.next("command")`. The label is derived from the request being made.
- Recording adapters call `session.record(interaction)` as before. The recorder derives the label from the interaction when organizing into rounds.
- Factory functions change signature from `&ReplaySession` to `&Session`.

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
- Factory function call sites need mechanical update from `&ReplaySession` to `&Session`.

## Out of Scope

- Explicit channel label overrides / `IntoChannelLabel` trait (future).
- Automatic barrier detection from async boundaries.
- Migration tool to convert flat fixtures to round-based format.
