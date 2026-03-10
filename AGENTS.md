# Agent Notes

## Test Command Defaults

- Normal environment: `cargo test --workspace --locked`
- Restricted Codex sandbox (socket bind/listen blocked): `cargo test --workspace --locked --features flotilla-daemon/skip-no-sandbox-tests`

Use the sandbox-safe command when `CODEX_SANDBOX` is set or socket-bind tests are expected to fail with `Operation not permitted`.

## Cursor Cloud specific instructions

### Overview

Flotilla is a single Rust TUI binary (no databases, Docker, or background services). All development commands are in `CLAUDE.md`.

### Toolchain

The VM ships with Rust 1.83 by default, but dependencies require edition 2024 (Rust ≥ 1.85). The update script handles upgrading to `stable` automatically via `rustup default stable && rustup update stable`.

### Running the app

```bash
cargo run -- --repo-root /workspace   # launches TUI against this repo
```

The app auto-detects git, GitHub (`gh` CLI), Claude, and terminal multiplexers from the environment. Only `git` is required; everything else degrades gracefully when absent.

### Key commands

| Task | Command |
|------|---------|
| Build | `cargo build --locked` |
| Lint (format) | `cargo fmt --check` |
| Lint (clippy) | `cargo clippy --all-targets --locked -- -D warnings` |
| Test | `cargo test --workspace --locked` |
| Run | `cargo run -- --repo-root /workspace` |

### Gotchas

- `cargo build` without `--locked` may update `Cargo.lock`; use `--locked` for reproducible builds.
- The TUI needs a real terminal (TTY). Use `cargo run` inside a terminal emulator, not piped.

## Testing Providers with Record/Replay

Flotilla providers (git, GitHub, Claude, etc.) integrate with external systems. The `replay` module captures and replays interactions to enable deterministic, offline testing.

### Pattern

Tests follow a 5-step workflow:

```rust
fn fixture(name: &str) -> String {
    format!("{}/src/providers/<category>/fixtures/{}",
        env!("CARGO_MANIFEST_DIR"), name)
}

#[tokio::test]
async fn test_my_provider() {
    let session = replay::test_session(&fixture("my_fixture.yaml"), masks);
    let runner = replay::test_runner(&session);
    let gh_api = replay::test_gh_api(&session, &runner);
    let http = replay::test_http_client(&session);

    // Inject replay implementations into your provider
    let provider = MyProvider::new(runner, gh_api, http);

    // Assert expected behavior
    let result = provider.some_method().await;
    assert!(result.is_ok());

    // Verify all fixtures were consumed
    session.finish();
}
```

In replay mode, fixtures load from disk and responses are deterministic. In record mode (`RECORD=1`), real interactions are captured and masking applies to the fixture before saving.

### Fixture Format

YAML fixtures contain `interactions` — a sequence of captured requests and responses. Each interaction has a `channel` tag: `command`, `gh_api`, or `http`.

**Command channel** (shell execution):
```yaml
interactions:
  - channel: command
    cmd: git
    args: [branch, --list, --format=%(refname:short)]
    cwd: '{repo}'
    stdout: |
      main
      feature/foo
    stderr: null
    exit_code: 0
```

**GitHub API channel** (gh CLI or GhApiClient):
```yaml
interactions:
  - channel: gh_api
    method: GET
    endpoint: repos/owner/repo/pulls?state=open&per_page=100
    status: 200
    body: '[{"number": 42, "title": "Fix bug"}]'
    headers:
      etag: 'W/"abc123"'
      total_count: "1"
```

**HTTP channel** (reqwest client):
```yaml
interactions:
  - channel: http
    method: GET
    url: https://api.anthropic.com/v1/sessions
    request_headers:
      authorization: Bearer token-123
      anthropic-version: "2023-06-01"
    status: 200
    response_body: '{"data": [{"id": "s1", "title": "Work"}]}'
    response_headers:
      content-type: application/json
```

Note: `{repo}` in `cwd` fields is a **mask placeholder** (see Masks section). It is replaced with the real path during recording and restored during replay.

### Recording

Capture real interactions:

```bash
RECORD=1 cargo test -p flotilla-core test_my_provider
```

This runs your test against real systems (git, GitHub, Claude API, HTTP endpoints). The test must succeed. On exit, the fixture file is written with all interactions masked.

**What is recorded:**
- **CommandRunner**: All shell commands (git, wt, etc.) and their output
- **GhApi**: All GitHub API calls (endpoints, request/response bodies)
- **HttpClient**: Manual only (see below)

**HTTP recording limitation:** `HttpClient` recording is not yet automated. Instead:
1. Capture HTTP interactions with `curl -v` or browser DevTools
2. Format them manually as YAML interactions
3. Add them to your fixture file

Example curl capture and format:
```bash
curl -v -H "authorization: Bearer token" https://api.anthropic.com/v1/sessions 2>&1 | tee capture.txt
# Then manually create the HTTP interaction YAML
```

### Masks

Masks replace sensitive or environment-dependent values (paths, tokens, IDs) with placeholders before saving fixtures. During replay, placeholders are restored.

Add masks before calling `test_session()`:

```rust
let mut masks = Masks::new();
masks.add("/Users/alice/dev/my-repo", "{repo}");
masks.add("/Users/alice", "{home}");
masks.add("ghp_secrettoken123", "{github_token}");

let session = replay::test_session(&fixture("my.yaml"), masks);
```

**Important:** Register longer (more specific) values first. Shorter values can partially match longer ones:

```rust
let mut masks = Masks::new();
// RIGHT: longer first
masks.add("/Users/alice/dev/my-repo/src", "{src}");
masks.add("/Users/alice/dev/my-repo", "{repo}");
masks.add("/Users/alice", "{home}");

// WRONG: shorter first would cause over-masking
// masks.add("/Users/alice", "{home}");  // Would mask the longer paths
// masks.add("/Users/alice/dev/my-repo", "{repo}");
```

### Example: Testing GitHub Code Review Provider

```rust
#[tokio::test]
async fn test_list_pull_requests() {
    let recording = replay::is_recording();
    let repo_slug = "owner/repo".to_string();
    let repo_root = if recording {
        PathBuf::from("/Users/alice/dev/my-repo")  // Real repo during recording
    } else {
        PathBuf::from("/test/repo")  // Fixture path during replay
    };

    let session = replay::test_session(
        &fixture("github_prs.yaml"),
        Masks::new(),  // Adjust if recording with secrets
    );
    let api = replay::test_gh_api(&session, &Arc::new(session.command_runner()));
    let runner = Arc::new(session.command_runner());

    let provider = GitHubCodeReview::new("github".into(), repo_slug, api, runner);
    let prs = provider.list_change_requests(&repo_root, 100).await.unwrap();

    for (id, pr) in &prs {
        assert!(!id.is_empty());
        assert!(!pr.title.is_empty());
    }

    session.finish();  // Saves if recording, asserts_complete if replaying
}
```

When run normally: loads `github_prs.yaml`, replays interactions, asserts all were consumed.
When run with `RECORD=1`: calls real GitHub API, saves fixture.
