# Recorder/Replayer Split + Sync Barriers Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Separate `ReplaySession` into distinct `Recorder` and `Replayer` types, and add round-based sync barriers with per-channel FIFO queues for resilient concurrent test fixtures.

**Architecture:** `ReplaySession` is replaced by a `Session` enum wrapping `Recorder` or `Replayer`. The `Replayer` organizes interactions into rounds, each containing per-channel FIFO queues keyed by `ChannelLabel`. Within a round, channels are independently consumable; barriers separate rounds. Existing flat YAML fixtures load as a single round.

**Tech Stack:** Rust, serde/serde_yml, std::collections (HashMap, VecDeque), std::sync (Arc, Mutex)

**Spec:** `docs/superpowers/specs/2026-03-10-recorder-replayer-sync-barriers-design.md`

---

## File Structure

| File | Role |
|------|------|
| `crates/flotilla-core/src/providers/replay.rs` | Main file — replace `ReplaySession`/`SessionInner` with `ChannelLabel`, `Round`, `Recorder`, `Replayer`, `Session` enum. Keep adapters (`ReplayRunner`, `RecordingRunner`, etc.) updated to use `Session`. Keep `Masks`, `Interaction`, mask/unmask helpers unchanged. |

All other files are **call-site updates** — changing `&ReplaySession` to `&Session` and `ReplaySession::from_file` to `Session` constructors:

| File | Changes |
|------|---------|
| `crates/flotilla-core/src/providers/vcs/git.rs` | 6 tests: `test_session` return type |
| `crates/flotilla-core/src/providers/vcs/wt.rs` | 3 tests: `test_session` return type |
| `crates/flotilla-core/src/providers/code_review/github.rs` | `build_api_and_runner` signature + body rewrite, 2 tests |
| `crates/flotilla-core/src/providers/issue_tracker/github.rs` | `build_api_and_runner` signature + body rewrite, 1 test |
| `crates/flotilla-core/src/providers/coding_agent/claude.rs` | 7 tests: `test_session`/`test_http_client`, 5 `assert_complete()` → `finish()` |
| `crates/flotilla-core/src/providers/workspace/tmux.rs` | 2 tests: `test_session`/`test_runner` |
| `crates/flotilla-core/src/providers/workspace/zellij.rs` | 2 tests: `test_session`/`test_runner` |

---

## Chunk 1: Core Types and Replayer

### Task 1: Add `ChannelLabel` and `Interaction::channel_label()`

**Files:**
- Modify: `crates/flotilla-core/src/providers/replay.rs:1-50`

- [ ] **Step 1: Write the test for `channel_label()` derivation**

Add to the `#[cfg(test)] mod tests` block:

```rust
#[test]
fn channel_label_from_interaction() {
    let cmd = Interaction::Command {
        cmd: "git".into(),
        args: vec!["status".into()],
        cwd: "/repo".into(),
        stdout: Some("ok\n".into()),
        stderr: None,
        exit_code: 0,
    };
    assert_eq!(cmd.channel_label(), ChannelLabel::Command("git".into()));

    let api = Interaction::GhApi {
        method: "GET".into(),
        endpoint: "repos/owner/repo/pulls".into(),
        status: 200,
        body: "[]".into(),
        headers: HashMap::new(),
    };
    assert_eq!(
        api.channel_label(),
        ChannelLabel::GhApi("repos/owner/repo/pulls".into())
    );

    let http = Interaction::Http {
        method: "GET".into(),
        url: "https://api.claude.ai/v1/sessions".into(),
        request_headers: HashMap::new(),
        request_body: None,
        status: 200,
        response_body: "{}".into(),
        response_headers: HashMap::new(),
    };
    assert_eq!(
        http.channel_label(),
        ChannelLabel::Http("api.claude.ai".into())
    );
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --locked -p flotilla-core channel_label_from_interaction`
Expected: FAIL — `ChannelLabel` type doesn't exist yet

- [ ] **Step 3: Implement `ChannelLabel` and `channel_label()`**

Add after the `Interaction` enum (around line 49), before `InteractionLog`:

```rust
/// Identifies the logical channel an interaction belongs to.
/// Within a replay round, interactions on the same channel are FIFO-ordered,
/// while different channels can be consumed in any order.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ChannelLabel {
    Command(String),
    GhApi(String),
    Http(String),
}

impl Interaction {
    /// Derive the channel label from interaction data.
    /// - Command: executable name (e.g. "git")
    /// - GhApi: endpoint path
    /// - Http: URL host
    pub fn channel_label(&self) -> ChannelLabel {
        match self {
            Interaction::Command { cmd, .. } => ChannelLabel::Command(cmd.clone()),
            Interaction::GhApi { endpoint, .. } => ChannelLabel::GhApi(endpoint.clone()),
            Interaction::Http { url, .. } => {
                // Extract host from URL via simple string parsing (avoids adding `url` crate dep).
                // "https://api.example.com/v1/foo" → "api.example.com"
                let host = url
                    .split("://")
                    .nth(1)
                    .unwrap_or(url)
                    .split('/')
                    .next()
                    .unwrap_or(url)
                    .split(':')
                    .next()
                    .unwrap_or(url)
                    .to_string();
                ChannelLabel::Http(host)
            }
        }
    }
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --locked -p flotilla-core channel_label_from_interaction`
Expected: PASS

- [ ] **Step 5: Commit**

```
git add -A && git commit -m "feat: add ChannelLabel and Interaction::channel_label()"
```

---

### Task 2: Add `Round` type and YAML deserialization

**Files:**
- Modify: `crates/flotilla-core/src/providers/replay.rs:51-55`

- [ ] **Step 1: Write the test for round loading — flat format**

```rust
#[test]
fn load_flat_fixture_as_single_round() {
    let yaml = r#"
interactions:
  - channel: command
    cmd: git
    args: ["status"]
    cwd: "/repo"
    stdout: "ok\n"
    exit_code: 0
  - channel: gh_api
    method: GET
    endpoint: "repos/owner/repo/pulls"
    status: 200
    body: "[]"
"#;
    let rounds = load_rounds_from_str(yaml);
    assert_eq!(rounds.len(), 1);
    assert_eq!(rounds[0].queues.len(), 2); // command + gh_api channels
    assert_eq!(
        rounds[0]
            .queues
            .get(&ChannelLabel::Command("git".into()))
            .expect("git channel")
            .len(),
        1
    );
    assert_eq!(
        rounds[0]
            .queues
            .get(&ChannelLabel::GhApi("repos/owner/repo/pulls".into()))
            .expect("gh_api channel")
            .len(),
        1
    );
}
```

- [ ] **Step 2: Write the test for round loading — multi-round format**

```rust
#[test]
fn load_multi_round_fixture() {
    let yaml = r#"
rounds:
  - interactions:
      - channel: command
        cmd: git
        args: ["status"]
        cwd: "/repo"
        stdout: "ok\n"
        exit_code: 0
      - channel: gh_api
        method: GET
        endpoint: "repos/owner/repo/pulls"
        status: 200
        body: "[]"
  - interactions:
      - channel: command
        cmd: git
        args: ["log"]
        cwd: "/repo"
        stdout: "abc123\n"
        exit_code: 0
"#;
    let rounds = load_rounds_from_str(yaml);
    assert_eq!(rounds.len(), 2);
    // Round 1: git + gh_api
    assert_eq!(rounds[0].queues.len(), 2);
    // Round 2: git only
    assert_eq!(rounds[1].queues.len(), 1);
    assert_eq!(
        rounds[1]
            .queues
            .get(&ChannelLabel::Command("git".into()))
            .expect("git channel")
            .len(),
        1
    );
}
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test --locked -p flotilla-core load_flat_fixture load_multi_round`
Expected: FAIL — `Round` and `load_rounds_from_str` don't exist

- [ ] **Step 4: Implement `Round`, `RoundLog`, and `load_rounds_from_str`**

```rust
use std::collections::VecDeque;

/// A round of interactions grouped by channel. Within a round, interactions
/// on the same channel are FIFO-ordered; different channels are independent.
#[derive(Debug)]
pub(crate) struct Round {
    pub(crate) queues: HashMap<ChannelLabel, VecDeque<Interaction>>,
}

impl Round {
    /// Build a round from a flat list of interactions, grouping by channel label.
    fn from_interactions(interactions: Vec<Interaction>) -> Self {
        let mut queues: HashMap<ChannelLabel, VecDeque<Interaction>> = HashMap::new();
        for interaction in interactions {
            let label = interaction.channel_label();
            queues.entry(label).or_default().push_back(interaction);
        }
        Round { queues }
    }

    /// Returns true if all channel queues are empty.
    fn is_empty(&self) -> bool {
        self.queues.values().all(|q| q.is_empty())
    }
}

/// Multi-round YAML format.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct RoundLog {
    rounds: Vec<InteractionLog>,
}

/// Load interactions from YAML, detecting format (flat `interactions:` or `rounds:`).
/// Returns a vec of `Round`s.
fn load_rounds_from_str(yaml: &str) -> Vec<Round> {
    // Try multi-round format first
    if let Ok(round_log) = serde_yml::from_str::<RoundLog>(yaml) {
        return round_log
            .rounds
            .into_iter()
            .map(|log| Round::from_interactions(log.interactions))
            .collect();
    }
    // Fall back to flat format (single round)
    let log: InteractionLog = serde_yml::from_str(yaml)
        .unwrap_or_else(|e| panic!("Failed to parse fixture YAML: {e}"));
    vec![Round::from_interactions(log.interactions)]
}
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test --locked -p flotilla-core load_flat_fixture load_multi_round`
Expected: PASS

- [ ] **Step 6: Commit**

```
git add -A && git commit -m "feat: add Round type and dual-format YAML loading"
```

---

### Task 3: Implement `Replayer`

**Files:**
- Modify: `crates/flotilla-core/src/providers/replay.rs`

- [ ] **Step 1: Write the test for replayer — basic single-round replay**

```rust
#[test]
fn replayer_serves_by_channel_label() {
    let yaml = r#"
interactions:
  - channel: command
    cmd: git
    args: ["status"]
    cwd: "{repo}"
    stdout: "ok\n"
    exit_code: 0
  - channel: gh_api
    method: GET
    endpoint: "repos/owner/repo/pulls"
    status: 200
    body: "[]"
"#;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.yaml");
    std::fs::write(&path, yaml).unwrap();

    let mut masks = Masks::new();
    masks.add("/real/repo", "{repo}");
    let replayer = Replayer::from_file(&path, masks);

    // Consume in reverse channel order — gh_api before command
    let api_interaction = replayer.next(&ChannelLabel::GhApi("repos/owner/repo/pulls".into()));
    match api_interaction {
        Interaction::GhApi { endpoint, .. } => {
            assert_eq!(endpoint, "repos/owner/repo/pulls");
        }
        _ => panic!("expected gh_api interaction"),
    }

    let cmd_interaction = replayer.next(&ChannelLabel::Command("git".into()));
    match cmd_interaction {
        Interaction::Command { cwd, .. } => {
            assert_eq!(cwd, "/real/repo"); // unmasked
        }
        _ => panic!("expected command interaction"),
    }

    replayer.assert_complete();
}
```

- [ ] **Step 2: Write the test for replayer — multi-round with barrier enforcement**

```rust
#[test]
#[should_panic(expected = "not found in current round")]
fn replayer_enforces_round_boundaries() {
    let yaml = r#"
rounds:
  - interactions:
      - channel: command
        cmd: git
        args: ["status"]
        cwd: "/repo"
        stdout: "ok\n"
        exit_code: 0
  - interactions:
      - channel: gh_api
        method: GET
        endpoint: "repos/owner/repo/pulls"
        status: 200
        body: "[]"
"#;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.yaml");
    std::fs::write(&path, yaml).unwrap();

    let replayer = Replayer::from_file(&path, Masks::new());

    // Try to consume round 2's interaction before round 1 is drained
    replayer.next(&ChannelLabel::GhApi("repos/owner/repo/pulls".into()));
}
```

- [ ] **Step 3: Write the test for replayer — auto-advance between rounds**

```rust
#[test]
fn replayer_auto_advances_rounds() {
    let yaml = r#"
rounds:
  - interactions:
      - channel: command
        cmd: git
        args: ["status"]
        cwd: "/repo"
        stdout: "ok\n"
        exit_code: 0
  - interactions:
      - channel: gh_api
        method: GET
        endpoint: "repos/owner/repo/pulls"
        status: 200
        body: "[]"
"#;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.yaml");
    std::fs::write(&path, yaml).unwrap();

    let replayer = Replayer::from_file(&path, Masks::new());

    // Drain round 1
    replayer.next(&ChannelLabel::Command("git".into()));

    // Should auto-advance to round 2
    let interaction = replayer.next(&ChannelLabel::GhApi("repos/owner/repo/pulls".into()));
    match interaction {
        Interaction::GhApi { endpoint, .. } => {
            assert_eq!(endpoint, "repos/owner/repo/pulls");
        }
        _ => panic!("expected gh_api"),
    }

    replayer.assert_complete();
}
```

- [ ] **Step 4: Run tests to verify they fail**

Run: `cargo test --locked -p flotilla-core replayer_serves replayer_enforces replayer_auto_advances`
Expected: FAIL — `Replayer` doesn't exist

- [ ] **Step 5: Implement `Replayer`**

```rust
/// Replays interactions from a fixture, organized into rounds with per-channel FIFO queues.
#[derive(Clone)]
pub struct Replayer {
    inner: Arc<Mutex<ReplayerInner>>,
}

struct ReplayerInner {
    rounds: VecDeque<Round>,
    masks: Masks,
}

impl Replayer {
    /// Load a replayer from a YAML fixture file.
    pub fn from_file(path: impl AsRef<Path>, masks: Masks) -> Self {
        let content = std::fs::read_to_string(path.as_ref())
            .unwrap_or_else(|e| panic!("Failed to read fixture {}: {e}", path.as_ref().display()));
        let rounds = load_rounds_from_str(&content);
        Self {
            inner: Arc::new(Mutex::new(ReplayerInner {
                rounds: VecDeque::from(rounds),
                masks,
            })),
        }
    }

    /// Consume the next interaction matching the given channel label.
    /// Panics if the channel is not present in the current round.
    pub(crate) fn next(&self, label: &ChannelLabel) -> Interaction {
        let mut inner = self.inner.lock().unwrap();
        let round = inner.rounds.front_mut().unwrap_or_else(|| {
            panic!("Replayer: no more rounds, but expected interaction on channel {label:?}")
        });

        let queue = round.queues.get_mut(label).unwrap_or_else(|| {
            let available: Vec<_> = round
                .queues
                .keys()
                .filter(|k| !round.queues[k].is_empty())
                .collect();
            panic!(
                "Replayer: channel {label:?} not found in current round. \
                 Available channels: {available:?}. Did you miss a barrier?"
            )
        });

        let interaction = queue.pop_front().unwrap_or_else(|| {
            let available: Vec<_> = round
                .queues
                .iter()
                .filter(|(_, q)| !q.is_empty())
                .map(|(k, _)| k)
                .collect();
            panic!(
                "Replayer: channel {label:?} is exhausted in current round. \
                 Available channels: {available:?}. Did you miss a barrier?"
            )
        });

        let unmasked = unmask_interaction(&interaction, &inner.masks);

        // Auto-advance if all queues in the current round are drained
        if round.is_empty() {
            inner.rounds.pop_front();
        }

        unmasked
    }

    /// Panics if any rounds or interactions remain unconsumed.
    pub fn assert_complete(&self) {
        let inner = self.inner.lock().unwrap();
        let non_empty: Vec<_> = inner
            .rounds
            .iter()
            .enumerate()
            .flat_map(|(i, round)| {
                round
                    .queues
                    .iter()
                    .filter(|(_, q)| !q.is_empty())
                    .map(move |(label, q)| format!("round {i}: {label:?} ({})", q.len()))
            })
            .collect();
        if !non_empty.is_empty() {
            panic!(
                "Replayer: {} unconsumed interaction(s) remaining: {}",
                non_empty.len(),
                non_empty.join(", ")
            );
        }
    }
}
```

- [ ] **Step 6: Run tests to verify they pass**

Run: `cargo test --locked -p flotilla-core replayer_serves replayer_enforces replayer_auto_advances`
Expected: PASS

- [ ] **Step 7: Commit**

```
git add -A && git commit -m "feat: implement Replayer with round-based per-channel queues"
```

---

### Task 4: Implement `Recorder`

**Files:**
- Modify: `crates/flotilla-core/src/providers/replay.rs`

- [ ] **Step 1: Write the test for recorder — single round (no barriers)**

```rust
#[test]
fn recorder_saves_single_round() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("recorded.yaml");

    let recorder = Recorder::new(&path, Masks::new());
    recorder.record(Interaction::Command {
        cmd: "git".into(),
        args: vec!["status".into()],
        cwd: "/repo".into(),
        stdout: Some("ok\n".into()),
        stderr: None,
        exit_code: 0,
    });
    recorder.record(Interaction::GhApi {
        method: "GET".into(),
        endpoint: "repos/owner/repo/pulls".into(),
        status: 200,
        body: "[]".into(),
        headers: HashMap::new(),
    });
    recorder.save();

    // Verify the file can be loaded back as rounds
    let content = std::fs::read_to_string(&path).unwrap();
    let rounds = load_rounds_from_str(&content);
    assert_eq!(rounds.len(), 1);
    assert_eq!(rounds[0].queues.len(), 2);
}
```

- [ ] **Step 2: Write the test for recorder — multi-round with barriers**

```rust
#[test]
fn recorder_saves_multi_round_with_barriers() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("recorded.yaml");

    let recorder = Recorder::new(&path, Masks::new());
    recorder.record(Interaction::Command {
        cmd: "git".into(),
        args: vec!["status".into()],
        cwd: "/repo".into(),
        stdout: Some("ok\n".into()),
        stderr: None,
        exit_code: 0,
    });
    recorder.barrier();
    recorder.record(Interaction::GhApi {
        method: "GET".into(),
        endpoint: "repos/owner/repo/pulls".into(),
        status: 200,
        body: "[]".into(),
        headers: HashMap::new(),
    });
    recorder.save();

    let content = std::fs::read_to_string(&path).unwrap();
    let rounds = load_rounds_from_str(&content);
    assert_eq!(rounds.len(), 2);
    assert_eq!(rounds[0].queues.len(), 1); // command only
    assert_eq!(rounds[1].queues.len(), 1); // gh_api only
}
```

- [ ] **Step 3: Write the test for recorder — masking**

```rust
#[test]
fn recorder_applies_masks() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("recorded.yaml");

    let mut masks = Masks::new();
    masks.add("/Users/bob/dev/repo", "{repo}");
    let recorder = Recorder::new(&path, masks);
    recorder.record(Interaction::Command {
        cmd: "git".into(),
        args: vec!["status".into()],
        cwd: "/Users/bob/dev/repo".into(),
        stdout: Some("ok\n".into()),
        stderr: None,
        exit_code: 0,
    });
    recorder.save();

    let content = std::fs::read_to_string(&path).unwrap();
    assert!(content.contains("{repo}"));
    assert!(!content.contains("/Users/bob/dev/repo"));
}
```

- [ ] **Step 4: Run tests to verify they fail**

Run: `cargo test --locked -p flotilla-core recorder_saves recorder_applies`
Expected: FAIL — `Recorder` doesn't exist

- [ ] **Step 5: Implement `Recorder`**

```rust
/// Records interactions into rounds, writing to a YAML fixture on save.
#[derive(Clone)]
pub struct Recorder {
    inner: Arc<Mutex<RecorderInner>>,
}

struct RecorderInner {
    rounds: Vec<Vec<Interaction>>,
    current: Vec<Interaction>,
    masks: Masks,
    file_path: PathBuf,
}

impl Recorder {
    /// Create a new recorder that will write to the given path.
    pub fn new(path: impl AsRef<Path>, masks: Masks) -> Self {
        Self {
            inner: Arc::new(Mutex::new(RecorderInner {
                rounds: Vec::new(),
                current: Vec::new(),
                masks,
                file_path: path.as_ref().to_path_buf(),
            })),
        }
    }

    /// Record a new interaction into the current round.
    pub(crate) fn record(&self, interaction: Interaction) {
        let mut inner = self.inner.lock().unwrap();
        let masked = mask_interaction(&interaction, &inner.masks);
        inner.current.push(masked);
    }

    /// Close the current round and start a new one.
    pub fn barrier(&self) {
        let mut inner = self.inner.lock().unwrap();
        if !inner.current.is_empty() {
            let round = std::mem::take(&mut inner.current);
            inner.rounds.push(round);
        }
    }

    /// Finalize and write all rounds to the YAML file.
    pub fn save(&self) {
        let mut inner = self.inner.lock().unwrap();
        // Finalize current round
        if !inner.current.is_empty() {
            let round = std::mem::take(&mut inner.current);
            inner.rounds.push(round);
        }

        let round_log = RoundLog {
            rounds: inner
                .rounds
                .iter()
                .map(|interactions| {
                    // Sort by channel label for deterministic output
                    let mut sorted = interactions.clone();
                    sorted.sort_by(|a, b| a.channel_label().cmp(&b.channel_label()));
                    InteractionLog {
                        interactions: sorted,
                    }
                })
                .collect(),
        };

        let yaml =
            serde_yml::to_string(&round_log).expect("Failed to serialize interactions");
        if let Some(parent) = inner.file_path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        std::fs::write(&inner.file_path, yaml).unwrap_or_else(|e| {
            panic!(
                "Failed to write fixture {}: {e}",
                inner.file_path.display()
            )
        });
    }
}
```

- [ ] **Step 6: Run tests to verify they pass**

Run: `cargo test --locked -p flotilla-core recorder_saves recorder_applies`
Expected: PASS

- [ ] **Step 7: Commit**

```
git add -A && git commit -m "feat: implement Recorder with round barriers and masking"
```

---

### Task 5: Implement `Session` enum and factory functions

**Files:**
- Modify: `crates/flotilla-core/src/providers/replay.rs`

- [ ] **Step 1: Write the test for Session round-trip (record then replay with channels)**

```rust
#[tokio::test]
async fn session_record_then_replay_round_trip() {
    use crate::providers::testing::MockRunner;

    let dir = tempfile::tempdir().unwrap();
    let fixture_path = dir.path().join("recorded.yaml");

    // Record phase
    {
        let session = Session::recording(&fixture_path, Masks::new());
        let mock = Arc::new(MockRunner::new(vec![
            Ok("hello\n".into()),
            Err("not found".into()),
        ]));
        let recorder = RecordingRunner::new(session.clone(), mock);

        let r1 = recorder.run("echo", &["hello"], Path::new("/tmp")).await;
        assert!(r1.is_ok());

        let r2 = recorder.run("missing", &[], Path::new("/tmp")).await;
        assert!(r2.is_err());

        session.finish();
    }

    // Replay phase — commands are on same channel, so order preserved
    {
        let session = Session::replaying(&fixture_path, Masks::new());
        let runner = ReplayRunner::new(session.clone());

        let r1 = runner.run("echo", &["hello"], Path::new("/tmp")).await;
        assert_eq!(r1.unwrap(), "hello\n");

        let r2 = runner.run("missing", &[], Path::new("/tmp")).await;
        assert!(r2.is_err());

        session.finish();
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --locked -p flotilla-core session_record_then_replay_round_trip`
Expected: FAIL — `Session` doesn't exist

- [ ] **Step 3: Implement `Session` enum**

```rust
/// A test session that is either recording or replaying interactions.
/// Note: The spec shows `Session::Recording(Arc<Mutex<Recorder>>)` but since
/// `Recorder` and `Replayer` already wrap `Arc<Mutex<...>>` internally,
/// we avoid the double-wrapping here.
#[derive(Clone)]
pub enum Session {
    Recording(Recorder),
    Replaying(Replayer),
}

impl Session {
    /// Create a recording session.
    pub fn recording(path: impl AsRef<Path>, masks: Masks) -> Self {
        Session::Recording(Recorder::new(path, masks))
    }

    /// Create a replaying session from a fixture file.
    pub fn replaying(path: impl AsRef<Path>, masks: Masks) -> Self {
        Session::Replaying(Replayer::from_file(path, masks))
    }

    /// Consume the next interaction matching the given channel label (replay only).
    pub(crate) fn next(&self, label: &ChannelLabel) -> Interaction {
        match self {
            Session::Replaying(r) => r.next(label),
            Session::Recording(_) => panic!("next() called in recording mode"),
        }
    }

    /// Record a new interaction (recording only).
    pub(crate) fn record(&self, interaction: Interaction) {
        match self {
            Session::Recording(r) => r.record(interaction),
            Session::Replaying(_) => panic!("record() called in replay mode"),
        }
    }

    /// Insert a barrier between rounds.
    /// In recording mode, closes the current round.
    /// In replay mode, this is a no-op (barriers are structural in the fixture).
    pub fn barrier(&self) {
        if let Session::Recording(r) = self {
            r.barrier();
        }
    }

    /// Returns true if this session is in recording mode.
    pub fn is_recording(&self) -> bool {
        matches!(self, Session::Recording(_))
    }

    /// Panics if any interactions remain unconsumed (replay only).
    pub fn assert_complete(&self) {
        match self {
            Session::Replaying(r) => r.assert_complete(),
            Session::Recording(_) => {}
        }
    }

    /// Save if recording, assert_complete if replaying.
    pub fn finish(&self) {
        match self {
            Session::Recording(r) => r.save(),
            Session::Replaying(r) => r.assert_complete(),
        }
    }
}
```

- [ ] **Step 4: Update adapter field types and factory functions together**

**Important:** Steps 4 and 5 are interdependent — the factory functions pass `Session` to adapter constructors, and the adapters must accept `Session`. Do both in one pass. First update all adapter struct fields from `session: ReplaySession` to `session: Session` (Step 5), then update the factory functions below.

Replace `test_session`, `test_runner`, `test_gh_api`, `test_http_client`:


```rust
/// Create a `Session` that either records or replays.
pub fn test_session(fixture_path: &str, masks: Masks) -> Session {
    if is_recording() {
        Session::recording(fixture_path, masks)
    } else {
        Session::replaying(fixture_path, masks)
    }
}

/// Create a `CommandRunner` for a test session.
pub fn test_runner(session: &Session) -> Arc<dyn CommandRunner> {
    match session {
        Session::Recording(_) => Arc::new(RecordingRunner::new(
            session.clone(),
            Arc::new(super::ProcessCommandRunner),
        )),
        Session::Replaying(_) => Arc::new(ReplayRunner::new(session.clone())),
    }
}

/// Create a `GhApi` for a test session.
pub fn test_gh_api(session: &Session, runner: &Arc<dyn CommandRunner>) -> Arc<dyn GhApi> {
    match session {
        Session::Recording(_) => {
            let real_api = Arc::new(super::github_api::GhApiClient::new(Arc::clone(runner)));
            Arc::new(RecordingGhApi::new(session.clone(), real_api))
        }
        Session::Replaying(_) => Arc::new(ReplayGhApi::new(session.clone())),
    }
}

/// Create an `HttpClient` for a test session.
pub fn test_http_client(session: &Session) -> Arc<dyn super::HttpClient> {
    match session {
        Session::Recording(_) => {
            let real_client = Arc::new(super::ReqwestHttpClient::new());
            Arc::new(RecordingHttpClient::new(session.clone(), real_client))
        }
        Session::Replaying(_) => Arc::new(ReplayHttpClient::new(session.clone())),
    }
}
```

- [ ] **Step 5: Update adapters to hold `Session` instead of `ReplaySession`**

Change field types in `ReplayRunner`, `ReplayGhApi`, `ReplayHttpClient`, `RecordingRunner`, `RecordingGhApi`, `RecordingHttpClient` from `session: ReplaySession` to `session: Session`.

Update `ReplayRunner` to call `self.session.next(&ChannelLabel::Command(cmd.to_string()))` instead of `self.session.next("command")`.

Update `ReplayGhApi` to call `self.session.next(&ChannelLabel::GhApi(endpoint.to_string()))` instead of `self.session.next("gh_api")`.

Update `ReplayHttpClient` to derive the label from the request URL host and call `self.session.next(&ChannelLabel::Http(host))`.

Update `RecordingRunner`, `RecordingGhApi`, `RecordingHttpClient` to call `self.session.record(...)` (unchanged API, just different type).

- [ ] **Step 6: Remove old `ReplaySession` and `SessionInner`**

Delete the `ReplaySession` struct, `SessionInner` struct, and all their methods. The `Session` enum now provides all functionality.

Keep `InteractionLog` (still needed for flat YAML format compat and for `RoundLog`).

- [ ] **Step 7: Run test to verify it passes**

Run: `cargo test --locked -p flotilla-core session_record_then_replay_round_trip`
Expected: PASS

- [ ] **Step 8: Run all replay module tests**

Run: `cargo test --locked -p flotilla-core replay`
Expected: Several older tests will fail because they still reference `ReplaySession::from_file`. These will be fixed in the next task.

- [ ] **Step 9a: Update session/runner tests**

Update `replay_session_serves_in_order`, `replay_runner_with_git_vcs`, `record_then_replay`:
- `ReplaySession::from_file(&path, masks)` → `Session::replaying(&path, masks)`
- `session.command_runner()` → `ReplayRunner::new(session.clone())`
- `ReplaySession::recording(...)` → `Session::recording(...)`
- `session.next("command")` → `session.next(&ChannelLabel::Command("git".into()))`

- [ ] **Step 9b: Update GhApi tests**

Update `replay_gh_api_get`, `replay_gh_api_get_non_2xx_returns_err`, `replay_gh_api_get_with_headers`, `replay_gh_api_get_with_headers_no_pagination`:
- `ReplaySession::from_file(...)` → `Session::replaying(...)`
- `session.gh_api()` → `ReplayGhApi::new(session.clone())`

- [ ] **Step 9c: Update HTTP and YAML tests**

Update `replay_http_client_round_trip`, `yaml_round_trip`:
- `ReplaySession::from_file(...)` → `Session::replaying(...)`
- `session.http_client()` → `ReplayHttpClient::new(session.clone())`
- `yaml_round_trip` may need no changes if it only tests `InteractionLog` serialization

- [ ] **Step 10: Run all replay module tests**

Run: `cargo test --locked -p flotilla-core replay`
Expected: ALL PASS

- [ ] **Step 11: Commit**

```
git add -A && git commit -m "feat: implement Session enum, update adapters and factory functions"
```

---

## Chunk 2: Provider Call-Site Updates and Verification

### Task 6: Update provider test call sites

**Files:**
- Modify: `crates/flotilla-core/src/providers/vcs/git.rs` (lines ~297-459)
- Modify: `crates/flotilla-core/src/providers/vcs/wt.rs` (lines ~331-466)
- Modify: `crates/flotilla-core/src/providers/code_review/github.rs` (lines ~249-315)
- Modify: `crates/flotilla-core/src/providers/issue_tracker/github.rs` (lines ~285-322)
- Modify: `crates/flotilla-core/src/providers/coding_agent/claude.rs` (lines ~496-703)
- Modify: `crates/flotilla-core/src/providers/workspace/tmux.rs` (lines ~550-648)
- Modify: `crates/flotilla-core/src/providers/workspace/zellij.rs` (lines ~573-709)

Most call sites (`git.rs`, `wt.rs`, `tmux.rs`, `zellij.rs`, `claude.rs`) use `test_session`/`test_runner`/`test_http_client` without naming the type, so they work unchanged. The `build_api_and_runner` helpers need full rewrites because their bodies call `session.gh_api()` and `session.command_runner()` which no longer exist on `Session`.

- [ ] **Step 1: Rewrite `build_api_and_runner` in `code_review/github.rs`**

Change signature from `session: &replay::ReplaySession` to `session: &replay::Session`. Replace the function body to use factory functions instead of calling methods on session:

```rust
fn build_api_and_runner(
    session: &replay::Session,
) -> (
    Arc<dyn crate::providers::github_api::GhApi>,
    Arc<dyn crate::providers::CommandRunner>,
) {
    let runner = replay::test_runner(session);
    let api = replay::test_gh_api(session, &runner);
    (api, runner)
}
```

Update callers to remove the `recording` parameter (it's no longer needed — the factory functions handle the mode internally).

- [ ] **Step 2: Rewrite `build_api_and_runner` in `issue_tracker/github.rs`**

Same change as Step 1.

- [ ] **Step 3: Update `assert_complete()` calls in `coding_agent/claude.rs`**

Change the 5 `session.assert_complete()` calls (around lines 504, 529, 553, 667, 688) to `session.finish()`. The `Session` enum exposes both `assert_complete()` and `finish()`, but `finish()` is the standard way to end a session (it handles both recording and replaying).

- [ ] **Step 4: Run all tests**

Run: `cargo test --locked`
Expected: ALL PASS.

- [ ] **Step 4: Run clippy**

Run: `cargo clippy --all-targets --locked -- -D warnings`
Expected: PASS with no warnings

- [ ] **Step 5: Run fmt**

Run: `cargo fmt`

- [ ] **Step 6: Commit**

```
git add -A && git commit -m "refactor: update provider test call sites for Session type"
```

---

### Task 7: Clean up — remove dead code, update doc comments

**Files:**
- Modify: `crates/flotilla-core/src/providers/replay.rs`

- [ ] **Step 1: Remove any leftover `ReplaySession` references in doc comments**

Search the **entire codebase** (not just `replay.rs`) for remaining references to `ReplaySession` in comments, doc strings, or documentation files (including `AGENTS.md`). Update them to reference `Session`, `Recorder`, or `Replayer` as appropriate.

- [ ] **Step 2: Verify no dead code warnings**

Run: `cargo clippy --all-targets --locked -- -D warnings`
Expected: PASS

- [ ] **Step 3: Final full test run**

Run: `cargo test --locked`
Expected: ALL PASS

- [ ] **Step 4: Commit**

```
git add -A && git commit -m "chore: clean up doc comments and dead code from replay refactor"
```

---

### Task 8: Add integration test for multi-channel round reordering

This validates the key behavioral change — that channels within a round can be consumed in any order.

**Files:**
- Modify: `crates/flotilla-core/src/providers/replay.rs` (test section)

- [ ] **Step 1: Write the integration test**

```rust
#[test]
fn multi_channel_round_allows_any_consumption_order() {
    // Fixture has command and http in same round
    let yaml = r#"
interactions:
  - channel: command
    cmd: git
    args: ["status"]
    cwd: "/repo"
    stdout: "ok\n"
    exit_code: 0
  - channel: http
    method: GET
    url: "https://api.test/v1/sessions"
    status: 200
    response_body: '{"data":[]}'
  - channel: command
    cmd: git
    args: ["log"]
    cwd: "/repo"
    stdout: "abc\n"
    exit_code: 0
"#;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.yaml");
    std::fs::write(&path, yaml).unwrap();

    let session = Session::replaying(&path, Masks::new());

    // Consume http FIRST (before any commands) — this is the key test.
    // With the old linear cursor this would have panicked.
    let http = session.next(&ChannelLabel::Http("api.test".into()));
    assert!(matches!(http, Interaction::Http { .. }));

    // Now consume commands in order
    let cmd1 = session.next(&ChannelLabel::Command("git".into()));
    match cmd1 {
        Interaction::Command { args, .. } => assert_eq!(args[0], "status"),
        _ => panic!("expected command"),
    }

    let cmd2 = session.next(&ChannelLabel::Command("git".into()));
    match cmd2 {
        Interaction::Command { args, .. } => assert_eq!(args[0], "log"),
        _ => panic!("expected command"),
    }

    session.finish();
}
```

- [ ] **Step 2: Run the test**

Run: `cargo test --locked -p flotilla-core multi_channel_round_allows`
Expected: PASS

- [ ] **Step 3: Commit**

```
git add -A && git commit -m "test: add integration test for multi-channel round reordering"
```

---

### Task 9: Final verification

- [ ] **Step 1: Run full test suite**

Run: `cargo test --locked`
Expected: ALL PASS

- [ ] **Step 2: Run clippy**

Run: `cargo clippy --all-targets --locked -- -D warnings`
Expected: PASS

- [ ] **Step 3: Run fmt**

Run: `cargo fmt --check`
Expected: No changes needed

- [ ] **Step 4: Verify existing fixtures load correctly**

Run: `cargo test --locked -p flotilla-core record_replay`
Expected: ALL PASS — all existing fixture-based tests work with the new round-based replayer treating flat fixtures as a single round.
