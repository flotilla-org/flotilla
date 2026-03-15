# CLI Multi-Host Query Commands Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement the narrowed `#284` CLI slice: `host list`, `host <host> status`, `host <host> providers`, and `topology` with human and JSON output.

**Architecture:** Add host/topology query response types to `flotilla-protocol`, extend `DaemonHandle`, and mirror peer-manager host/topology snapshots into `InProcessDaemon` so both in-process and socket-backed paths can answer the same queries. Keep the CLI thin: parse commands in `src/main.rs`, fetch typed responses through `DaemonHandle`, and render them in `crates/flotilla-tui/src/cli.rs`.

**Tech Stack:** Rust, Tokio, Clap, Serde, existing `DaemonHandle`/socket RPC layer, `comfy_table`.

---

## Chunk 1: Shared Query Data And Daemon Plumbing

### Task 1: Add host/topology protocol response types and daemon trait methods

**Files:**
- Modify: `crates/flotilla-protocol/src/query.rs`
- Modify: `crates/flotilla-protocol/src/lib.rs`
- Modify: `crates/flotilla-core/src/daemon.rs`
- Modify: `crates/flotilla-tui/src/app/test_support.rs`

- [ ] **Step 1: Write failing protocol tests for the new response types**

Add serde round-trip tests in `crates/flotilla-protocol/src/query.rs` for:

```rust
HostListResponse
HostStatusResponse
HostProvidersResponse
TopologyResponse
```

Cover at least:

- `HostListEntry` with `has_summary = false`
- `HostProvidersResponse` carrying a `HostSummary`
- `TopologyRoute` with fallbacks

- [ ] **Step 2: Run the protocol test target and verify it fails**

Run:

```bash
cargo test -p flotilla-protocol --locked query
```

Expected: compile failures because the new response types do not exist yet.

- [ ] **Step 3: Add the new query structs and exports**

In `crates/flotilla-protocol/src/query.rs`, add:

```rust
pub struct HostListResponse { pub hosts: Vec<HostListEntry> }
pub struct HostListEntry { ... }
pub struct HostStatusResponse { ... }
pub struct HostProvidersResponse { ... }
pub struct TopologyResponse { pub local_host: HostName, pub routes: Vec<TopologyRoute> }
pub struct TopologyRoute { ... }
```

In `crates/flotilla-protocol/src/lib.rs`, re-export the new types.

In `crates/flotilla-core/src/daemon.rs`, extend `DaemonHandle` with:

```rust
async fn list_hosts(&self) -> Result<HostListResponse, String>;
async fn get_host_status(&self, host: &str) -> Result<HostStatusResponse, String>;
async fn get_host_providers(&self, host: &str) -> Result<HostProvidersResponse, String>;
async fn get_topology(&self) -> Result<TopologyResponse, String>;
```

In `crates/flotilla-tui/src/app/test_support.rs`, add stub implementations that return `Err("not implemented".into())` so the trait compiles before CLI test doubles are upgraded later.

- [ ] **Step 4: Re-run the protocol test target**

Run:

```bash
cargo test -p flotilla-protocol --locked query
```

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-protocol/src/query.rs crates/flotilla-protocol/src/lib.rs crates/flotilla-core/src/daemon.rs crates/flotilla-tui/src/app/test_support.rs
git commit -m "feat: add host and topology query protocol types"
```

### Task 2: Build host/topology query state inside `InProcessDaemon`

**Files:**
- Modify: `crates/flotilla-core/src/in_process.rs`
- Create: `crates/flotilla-core/src/host_queries.rs`
- Modify: `crates/flotilla-core/src/lib.rs`
- Test: `crates/flotilla-core/tests/in_process_daemon.rs`

- [ ] **Step 1: Write failing daemon tests for host queries**

Add `InProcessDaemon` tests covering:

- `list_hosts()` includes the local host
- configured disconnected peers still appear in `list_hosts()`
- `get_host_providers(local_host)` returns the local `HostSummary`
- `get_host_providers(remote_host)` errors when no summary is available
- remote host repo/work counts are derived from peer overlays
- `get_topology()` returns mirrored route data

Use the existing peer-overlay test patterns in `crates/flotilla-core/tests/in_process_daemon.rs`.

- [ ] **Step 2: Run the targeted daemon test subset and verify it fails**

Run:

```bash
cargo test -p flotilla-core --locked --features test-support host_
```

Expected: compile failures because the daemon methods and helper state do not exist yet.

- [ ] **Step 3: Add a focused helper module for host query builders**

Create `crates/flotilla-core/src/host_queries.rs` to hold pure helper logic so `in_process.rs` does not absorb another large formatting-free query subsystem.

Move into that file:

- host-name resolution helper
- aggregation of repo/work counts from local repos and `peer_providers`
- conversion from mirrored route snapshot to `TopologyResponse`
- construction of `HostListEntry`, `HostStatusResponse`, and `HostProvidersResponse`

Keep the helpers input-driven:

```rust
pub(crate) fn build_host_list(...)
pub(crate) fn build_host_status(...)
pub(crate) fn build_host_providers(...)
pub(crate) fn build_topology(...)
```

- [ ] **Step 4: Extend `InProcessDaemon` state for mirrored peer host/topology data**

In `crates/flotilla-core/src/in_process.rs`, add fields for:

- configured peer names
- mirrored remote host summaries
- mirrored topology route snapshot

Add daemon-side mutators used by the server / embedded peer wiring:

```rust
pub async fn set_configured_peer_names(&self, peers: Vec<HostName>);
pub async fn set_peer_host_summaries(&self, summaries: HashMap<HostName, HostSummary>);
pub async fn set_topology_routes(&self, routes: Vec<TopologyRoute>);
```

Do not let the query path reach into `PeerManager` directly.

- [ ] **Step 5: Implement the new `DaemonHandle` methods for `InProcessDaemon`**

Use the new helper module and existing state:

- `list_hosts()` merges local host, configured peers, and mirrored remote summaries
- `get_host_status()` resolves a host name from the merged view
- `get_host_providers()` returns the local summary or a mirrored remote summary
- `get_topology()` returns the mirrored routing view, defaulting to an empty route set when peer networking is absent

For repo/work counts:

- local host counts come from tracked local repo snapshots
- remote host counts come from `peer_providers`

- [ ] **Step 6: Re-run the targeted daemon tests**

Run:

```bash
cargo test -p flotilla-core --locked --features test-support host_
```

Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add crates/flotilla-core/src/in_process.rs crates/flotilla-core/src/host_queries.rs crates/flotilla-core/src/lib.rs crates/flotilla-core/tests/in_process_daemon.rs
git commit -m "feat: add daemon host and topology query support"
```

## Chunk 2: Peer Snapshot Mirroring, RPC, And CLI Surface

### Task 3: Mirror peer-manager snapshots into the daemon and expose socket RPCs

**Files:**
- Modify: `crates/flotilla-daemon/src/peer/manager.rs`
- Modify: `crates/flotilla-daemon/src/server.rs`
- Modify: `crates/flotilla-client/src/lib.rs`
- Test: `crates/flotilla-daemon/src/server.rs`
- Test: `crates/flotilla-daemon/tests/socket_roundtrip.rs`

- [ ] **Step 1: Write failing server and socket tests**

Add dispatch tests for:

- `list_hosts`
- `get_host_status`
- `get_host_providers`
- `get_topology`

Add socket round-trip coverage for the same methods in `crates/flotilla-daemon/tests/socket_roundtrip.rs`.

- [ ] **Step 2: Run the server/socket test subset and verify it fails**

Run:

```bash
mkdir -p .codex-tmp && TMPDIR="$PWD/.codex-tmp" cargo test -p flotilla-daemon --locked --features flotilla-daemon/skip-no-sandbox-tests socket_roundtrip
```

Expected: compile or dispatch failures because the new RPC methods are not wired yet.

- [ ] **Step 3: Expose peer-manager snapshots needed by the query layer**

In `crates/flotilla-daemon/src/peer/manager.rs`, add read-only accessors that snapshot:

- remote host summaries
- current route table as `(target, primary, fallbacks)`
- configured peer names if not already exposed in a suitable form

Do not leak mutable access outside `PeerManager`.

- [ ] **Step 4: Mirror peer-manager state into `InProcessDaemon`**

In `crates/flotilla-daemon/src/server.rs`:

- seed configured peer names during peer-manager construction
- whenever host summaries change, push the full snapshot into `InProcessDaemon`
- whenever connection/routing state changes, rebuild and push the topology snapshot into `InProcessDaemon`

Update both normal daemon mode and embedded peer networking so they behave the same way.

- [ ] **Step 5: Add request dispatch and client RPC methods**

In `crates/flotilla-daemon/src/server.rs`, extend `dispatch_request()` with:

```rust
"list_hosts"
"get_host_status"
"get_host_providers"
"get_topology"
```

Use `extract_str_param()` for host names.

In `crates/flotilla-client/src/lib.rs`, implement the matching `SocketDaemon` methods:

```rust
let resp = self.request("list_hosts", serde_json::json!({})).await?;
```

and the corresponding host-name requests.

- [ ] **Step 6: Re-run the socket test subset**

Run:

```bash
mkdir -p .codex-tmp && TMPDIR="$PWD/.codex-tmp" cargo test -p flotilla-daemon --locked --features flotilla-daemon/skip-no-sandbox-tests socket_roundtrip
```

Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add crates/flotilla-daemon/src/peer/manager.rs crates/flotilla-daemon/src/server.rs crates/flotilla-client/src/lib.rs crates/flotilla-daemon/tests/socket_roundtrip.rs
git commit -m "feat: add host and topology socket queries"
```

### Task 4: Add CLI grammar, query runners, and human output

**Files:**
- Modify: `src/main.rs`
- Modify: `crates/flotilla-tui/src/cli.rs`
- Test: `crates/flotilla-tui/src/cli.rs`

- [ ] **Step 1: Write failing parser/formatter tests**

Add tests for:

- `host list`
- `host alpha status`
- `host alpha providers`
- `host alpha repo add /tmp/repo` still parsing as a host-scoped control command
- `topology --json`
- human formatter coverage for host list, host status, host providers, and topology

- [ ] **Step 2: Run the CLI test target and verify it fails**

Run:

```bash
cargo test -p flotilla-tui --locked cli::
```

Expected: parse or compile failures because the new command grammar and renderers do not exist yet.

- [ ] **Step 3: Refactor CLI parsing in `src/main.rs`**

Change the CLI command shape to support host queries without breaking host-scoped controls.

Recommended parsing structure:

```rust
enum HostCommand {
    List,
    Query { host: String, detail: HostQueryCommand },
    Control(Command),
}

enum HostQueryCommand {
    Status,
    Providers,
}
```

Implementation notes:

- keep `SubCommand::Host` as a raw-args parser
- parse `list` before interpreting the first token as a host name
- preserve existing `parse_host_control_command()` behavior for control commands
- add `SubCommand::Topology { json: bool }`

- [ ] **Step 4: Add CLI runners and formatters**

In `crates/flotilla-tui/src/cli.rs`, add:

```rust
pub async fn run_host_list(...)
pub async fn run_host_status(...)
pub async fn run_host_providers(...)
pub async fn run_topology(...)
```

Add human formatters:

- `format_host_list_human`
- `format_host_status_human`
- `format_host_providers_human`
- `format_topology_human`

Reuse `HostSummary` contents rather than re-deriving inventory/provider tables.

- [ ] **Step 5: Re-run the CLI test target**

Run:

```bash
cargo test -p flotilla-tui --locked cli::
```

Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/main.rs crates/flotilla-tui/src/cli.rs
git commit -m "feat: add multi-host CLI query commands"
```

### Task 5: Final verification

**Files:**
- No new files; verification only

- [ ] **Step 1: Run the main targeted verification suite**

Run:

```bash
mkdir -p .codex-tmp && TMPDIR="$PWD/.codex-tmp" cargo test --workspace --locked --features flotilla-daemon/skip-no-sandbox-tests
```

Expected: PASS.

- [ ] **Step 2: Run format and clippy checks**

Run:

```bash
cargo +nightly-2026-03-12 fmt --check
```

Expected: PASS.

Run:

```bash
cargo clippy --all-targets --locked -- -D warnings
```

Expected: PASS.

- [ ] **Step 3: Update issue/spec references if implementation drifted**

If command names, response shapes, or error semantics differ from the design doc or issue text, update:

- `docs/superpowers/specs/2026-03-15-cli-multihost-query-commands-design.md`
- issue `#284`

before closing out execution.

- [ ] **Step 4: Final commit**

```bash
git add docs/superpowers/specs/2026-03-15-cli-multihost-query-commands-design.md docs/superpowers/plans/2026-03-15-cli-multihost-query-commands.md
git commit -m "docs: add plan for multi-host query commands"
```

Plan complete and saved to `docs/superpowers/plans/2026-03-15-cli-multihost-query-commands.md`. Ready to execute?
