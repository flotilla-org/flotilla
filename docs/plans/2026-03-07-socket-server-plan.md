# Socket Server Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add two-process mode where a standalone daemon serves state over a unix socket and the TUI connects as a client.

**Architecture:** The daemon process embeds `InProcessDaemon` and exposes it over a unix socket with newline-delimited JSON. A new `SocketDaemon` in `flotilla-tui` implements `DaemonHandle` by talking to the socket. The binary gains subcommand dispatch (`flotilla daemon`, `flotilla status`, `flotilla watch`). The existing `--embedded` (in-process) mode is preserved.

**Tech Stack:** Rust, tokio (unix sockets, signal handling), serde_json (ndjson wire format), clap (subcommands).

**Design doc:** `docs/plans/2026-03-07-socket-server-design.md`

---

### Task 1: Protocol — update Message::Response to untyped data

**Files:**
- Modify: `crates/flotilla-protocol/src/lib.rs`
- Modify: `crates/flotilla-protocol/Cargo.toml` (no change needed — already has serde_json)

**Changes:**

Update the `Message::Response` variant from its current shape:

```rust
// Current:
Response { id: u64, result: CommandResult },

// New:
Response {
    id: u64,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
},
```

Add a helper type and impl for client-side parsing:

```rust
/// Parsed response from the wire — before type-specific deserialization.
#[derive(Debug)]
pub struct RawResponse {
    pub ok: bool,
    pub data: Option<serde_json::Value>,
    pub error: Option<String>,
}

impl RawResponse {
    /// Parse the data payload into the expected type.
    pub fn parse<T: serde::de::DeserializeOwned>(self) -> Result<T, String> {
        if !self.ok {
            return Err(self.error.unwrap_or_else(|| "unknown error".into()));
        }
        let data = self.data.ok_or("response missing data field")?;
        serde_json::from_value(data).map_err(|e| format!("failed to parse response: {e}"))
    }

    /// Parse a response with no data payload (refresh, add_repo, remove_repo).
    pub fn parse_empty(self) -> Result<(), String> {
        if !self.ok {
            return Err(self.error.unwrap_or_else(|| "unknown error".into()));
        }
        Ok(())
    }
}
```

Update tests: fix the existing `message_request_roundtrip` test and add a `message_response_roundtrip` test for the new shape (ok=true with data, ok=false with error).

**Verify:** `cargo test -p flotilla-protocol`

**Commit:** `refactor: update Message::Response to untyped data envelope`

---

### Task 2: Protocol — add helper to build Response messages

**Files:**
- Modify: `crates/flotilla-protocol/src/lib.rs`

**Changes:**

Add builder functions for daemon-side response construction:

```rust
impl Message {
    /// Build a success response with a serializable payload.
    pub fn ok_response<T: serde::Serialize>(id: u64, data: &T) -> Self {
        Message::Response {
            id,
            ok: true,
            data: Some(serde_json::to_value(data).unwrap_or(serde_json::Value::Null)),
            error: None,
        }
    }

    /// Build a success response with no payload.
    pub fn empty_ok_response(id: u64) -> Self {
        Message::Response {
            id,
            ok: true,
            data: None,
            error: None,
        }
    }

    /// Build an error response.
    pub fn error_response(id: u64, message: impl Into<String>) -> Self {
        Message::Response {
            id,
            ok: false,
            data: None,
            error: Some(message.into()),
        }
    }

    /// Extract a RawResponse from a Response message.
    /// Returns None if this is not a Response.
    pub fn into_raw_response(self) -> Option<(u64, RawResponse)> {
        match self {
            Message::Response { id, ok, data, error } => {
                Some((id, RawResponse { ok, data, error }))
            }
            _ => None,
        }
    }
}
```

Add tests for each builder.

**Verify:** `cargo test -p flotilla-protocol`

**Commit:** `feat: add Message response builder helpers`

---

### Task 3: Config — extract config_dir as a configurable path

**Files:**
- Modify: `crates/flotilla-core/src/config.rs`

**Changes:**

Currently `config_dir()` is hardcoded to `~/.config/flotilla/repos`. Extract a `flotilla_config_dir()` function that returns the base `~/.config/flotilla/` path, usable by both config and daemon socket:

```rust
/// Base flotilla config directory. Defaults to ~/.config/flotilla/.
pub fn flotilla_config_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("~"))
        .join(".config/flotilla")
}

fn config_dir() -> PathBuf {
    flotilla_config_dir().join("repos")
}
```

Update `tab_order_file()` and `load_config()` to use `flotilla_config_dir()` instead of duplicating the path construction.

**Verify:** `cargo test --workspace`

**Commit:** `refactor: extract flotilla_config_dir helper`

---

### Task 4: Daemon server — connection handler

**Files:**
- Modify: `crates/flotilla-daemon/Cargo.toml`
- Create: `crates/flotilla-daemon/src/lib.rs` (replace empty file)
- Create: `crates/flotilla-daemon/src/server.rs`

**Changes in Cargo.toml:**

```toml
[package]
name = "flotilla-daemon"
version = "0.1.0"
edition = "2021"

[dependencies]
flotilla-core = { path = "../flotilla-core" }
flotilla-protocol = { path = "../flotilla-protocol" }
tokio = { workspace = true }
serde = { workspace = true }
serde_json = { workspace = true }
async-trait = { workspace = true }
tracing = { workspace = true }
```

**Changes in lib.rs:**

```rust
pub mod server;
```

**Changes in server.rs:**

Implement the daemon server with:

```rust
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::net::UnixListener;
use tokio::sync::broadcast;
use tracing::{info, error, warn};

use flotilla_core::daemon::DaemonHandle;
use flotilla_core::in_process::InProcessDaemon;
use flotilla_protocol::{Command, DaemonEvent, Message};

pub struct DaemonServer {
    daemon: Arc<InProcessDaemon>,
    socket_path: PathBuf,
    idle_timeout: Duration,
    client_count: Arc<AtomicUsize>,
}

impl DaemonServer {
    pub async fn new(
        repo_paths: Vec<PathBuf>,
        socket_path: PathBuf,
        idle_timeout: Duration,
    ) -> Self {
        let daemon = InProcessDaemon::new(repo_paths).await;
        Self {
            daemon,
            socket_path,
            idle_timeout,
            client_count: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub async fn run(self) -> Result<(), String> {
        // Clean up stale socket
        if self.socket_path.exists() {
            let _ = std::fs::remove_file(&self.socket_path);
        }

        // Ensure parent directory exists
        if let Some(parent) = self.socket_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }

        let listener = UnixListener::bind(&self.socket_path)
            .map_err(|e| format!("failed to bind {}: {e}", self.socket_path.display()))?;
        info!("listening on {}", self.socket_path.display());

        // Spawn idle timeout watcher
        let client_count = self.client_count.clone();
        let idle_timeout = self.idle_timeout;
        let shutdown = tokio::sync::watch::channel(false);
        let shutdown_tx = shutdown.0;
        let mut shutdown_rx = shutdown.1.clone();

        let idle_client_count = client_count.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(10)).await;
                if idle_client_count.load(Ordering::Relaxed) == 0 {
                    // Start idle countdown
                    tokio::time::sleep(idle_timeout).await;
                    if idle_client_count.load(Ordering::Relaxed) == 0 {
                        info!("idle timeout — shutting down");
                        let _ = shutdown_tx.send(true);
                        return;
                    }
                }
            }
        });

        loop {
            tokio::select! {
                accept = listener.accept() => {
                    match accept {
                        Ok((stream, _)) => {
                            self.client_count.fetch_add(1, Ordering::Relaxed);
                            info!("client connected (total: {})", self.client_count.load(Ordering::Relaxed));
                            let daemon = self.daemon.clone();
                            let count = self.client_count.clone();
                            tokio::spawn(async move {
                                handle_client(stream, daemon).await;
                                let remaining = count.fetch_sub(1, Ordering::Relaxed) - 1;
                                info!("client disconnected (total: {remaining})");
                            });
                        }
                        Err(e) => {
                            error!("accept error: {e}");
                        }
                    }
                }
                _ = shutdown_rx.changed() => {
                    break;
                }
            }
        }

        // Cleanup
        let _ = std::fs::remove_file(&self.socket_path);
        info!("socket removed, exiting");
        Ok(())
    }
}
```

The `handle_client` function handles one connection:

```rust
async fn handle_client(
    stream: tokio::net::UnixStream,
    daemon: Arc<InProcessDaemon>,
) {
    let (reader, writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let writer = Arc::new(tokio::sync::Mutex::new(BufWriter::new(writer)));

    // Spawn event forwarder
    let mut event_rx = daemon.subscribe();
    let event_writer = writer.clone();
    let event_task = tokio::spawn(async move {
        loop {
            match event_rx.recv().await {
                Ok(event) => {
                    let msg = Message::Event { event: Box::new(event) };
                    if write_message(&event_writer, &msg).await.is_err() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!("client lagged, skipped {n} events");
                }
                Err(_) => break,
            }
        }
    });

    // Read request loop
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) => break, // EOF
            Ok(_) => {
                let response = match serde_json::from_str::<Message>(&line) {
                    Ok(Message::Request { id, method, params }) => {
                        dispatch_request(id, &method, params, &*daemon).await
                    }
                    Ok(_) => Message::error_response(0, "expected request"),
                    Err(e) => Message::error_response(0, format!("parse error: {e}")),
                };
                if write_message(&writer, &response).await.is_err() {
                    break;
                }
            }
            Err(_) => break,
        }
    }

    event_task.abort();
}

async fn dispatch_request(
    id: u64,
    method: &str,
    params: serde_json::Value,
    daemon: &dyn DaemonHandle,
) -> Message {
    match method {
        "list_repos" => match daemon.list_repos().await {
            Ok(repos) => Message::ok_response(id, &repos),
            Err(e) => Message::error_response(id, e),
        },
        "get_state" => {
            let Some(repo) = params.get("repo").and_then(|v| v.as_str()) else {
                return Message::error_response(id, "missing 'repo' param");
            };
            let repo = PathBuf::from(repo);
            match daemon.get_state(&repo).await {
                Ok(snapshot) => Message::ok_response(id, &snapshot),
                Err(e) => Message::error_response(id, e),
            }
        }
        "execute" => {
            let Some(repo) = params.get("repo").and_then(|v| v.as_str()) else {
                return Message::error_response(id, "missing 'repo' param");
            };
            let repo = PathBuf::from(repo);
            let cmd: Command = match serde_json::from_value(params.get("command").cloned().unwrap_or_default()) {
                Ok(cmd) => cmd,
                Err(e) => return Message::error_response(id, format!("invalid command: {e}")),
            };
            match daemon.execute(&repo, cmd).await {
                Ok(result) => Message::ok_response(id, &result),
                Err(e) => Message::error_response(id, e),
            }
        }
        "refresh" => {
            let Some(repo) = params.get("repo").and_then(|v| v.as_str()) else {
                return Message::error_response(id, "missing 'repo' param");
            };
            match daemon.refresh(Path::new(repo)).await {
                Ok(()) => Message::empty_ok_response(id),
                Err(e) => Message::error_response(id, e),
            }
        }
        "add_repo" => {
            let Some(path) = params.get("path").and_then(|v| v.as_str()) else {
                return Message::error_response(id, "missing 'path' param");
            };
            match daemon.add_repo(Path::new(path)).await {
                Ok(()) => Message::empty_ok_response(id),
                Err(e) => Message::error_response(id, e),
            }
        }
        "remove_repo" => {
            let Some(path) = params.get("path").and_then(|v| v.as_str()) else {
                return Message::error_response(id, "missing 'path' param");
            };
            match daemon.remove_repo(Path::new(path)).await {
                Ok(()) => Message::empty_ok_response(id),
                Err(e) => Message::error_response(id, e),
            }
        }
        _ => Message::error_response(id, format!("unknown method: {method}")),
    }
}

async fn write_message(
    writer: &tokio::sync::Mutex<BufWriter<tokio::net::unix::OwnedWriteHalf>>,
    msg: &Message,
) -> Result<(), ()> {
    let mut w = writer.lock().await;
    let json = serde_json::to_string(msg).map_err(|_| ())?;
    w.write_all(json.as_bytes()).await.map_err(|_| ())?;
    w.write_all(b"\n").await.map_err(|_| ())?;
    w.flush().await.map_err(|_| ())?;
    Ok(())
}
```

**Verify:** `cargo check -p flotilla-daemon`

**Commit:** `feat: daemon server with connection handler and request dispatch`

---

### Task 5: Socket client — SocketDaemon

**Files:**
- Modify: `crates/flotilla-tui/Cargo.toml`
- Create: `crates/flotilla-tui/src/socket.rs`
- Modify: `crates/flotilla-tui/src/lib.rs`

**Changes in Cargo.toml** — add dependencies:

```toml
serde = { workspace = true }
serde_json = { workspace = true }
async-trait = { workspace = true }
```

**Changes in lib.rs:**

```rust
pub mod socket;
```

**Changes in socket.rs:**

```rust
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::net::UnixStream;
use tokio::sync::{broadcast, oneshot, Mutex};
use tracing::{error, warn};

use flotilla_core::daemon::DaemonHandle;
use flotilla_protocol::{
    Command, CommandResult, DaemonEvent, Message, RawResponse, RepoInfo, Snapshot,
};

pub struct SocketDaemon {
    writer: Mutex<BufWriter<tokio::net::unix::OwnedWriteHalf>>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<RawResponse>>>>,
    event_tx: broadcast::Sender<DaemonEvent>,
    next_id: AtomicU64,
}

impl SocketDaemon {
    /// Connect to a daemon socket and spawn the background reader.
    pub async fn connect(socket_path: &Path) -> Result<Arc<Self>, String> {
        let stream = UnixStream::connect(socket_path)
            .await
            .map_err(|e| format!("failed to connect to {}: {e}", socket_path.display()))?;

        let (reader, writer) = stream.into_split();
        let (event_tx, _) = broadcast::channel(256);
        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<RawResponse>>>> =
            Arc::new(Mutex::new(HashMap::new()));

        let daemon = Arc::new(Self {
            writer: Mutex::new(BufWriter::new(writer)),
            pending: pending.clone(),
            event_tx: event_tx.clone(),
            next_id: AtomicU64::new(1),
        });

        // Spawn reader task
        let reader_pending = pending;
        let reader_event_tx = event_tx;
        tokio::spawn(async move {
            let mut reader = BufReader::new(reader);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) => break, // EOF — daemon disconnected
                    Ok(_) => {
                        match serde_json::from_str::<Message>(&line) {
                            Ok(Message::Response { id, ok, data, error }) => {
                                let raw = RawResponse { ok, data, error };
                                let mut pending = reader_pending.lock().await;
                                if let Some(tx) = pending.remove(&id) {
                                    let _ = tx.send(raw);
                                }
                            }
                            Ok(Message::Event { event }) => {
                                let _ = reader_event_tx.send(*event);
                            }
                            Ok(_) => {
                                warn!("unexpected message from daemon");
                            }
                            Err(e) => {
                                error!("failed to parse daemon message: {e}");
                            }
                        }
                    }
                    Err(e) => {
                        error!("daemon read error: {e}");
                        break;
                    }
                }
            }
            // Daemon disconnected — drop all pending requests
            let mut pending = reader_pending.lock().await;
            for (_, tx) in pending.drain() {
                let _ = tx.send(RawResponse {
                    ok: false,
                    data: None,
                    error: Some("daemon disconnected".into()),
                });
            }
        });

        Ok(daemon)
    }

    async fn request(&self, method: &str, params: serde_json::Value) -> Result<RawResponse, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();

        {
            let mut pending = self.pending.lock().await;
            pending.insert(id, tx);
        }

        let msg = Message::Request {
            id,
            method: method.to_string(),
            params,
        };

        {
            let mut writer = self.writer.lock().await;
            let json = serde_json::to_string(&msg).map_err(|e| format!("serialize: {e}"))?;
            writer.write_all(json.as_bytes()).await.map_err(|e| format!("write: {e}"))?;
            writer.write_all(b"\n").await.map_err(|e| format!("write: {e}"))?;
            writer.flush().await.map_err(|e| format!("flush: {e}"))?;
        }

        rx.await.map_err(|_| "daemon disconnected".to_string())
    }
}

#[async_trait]
impl DaemonHandle for SocketDaemon {
    fn subscribe(&self) -> broadcast::Receiver<DaemonEvent> {
        self.event_tx.subscribe()
    }

    async fn get_state(&self, repo: &Path) -> Result<Snapshot, String> {
        let resp = self.request("get_state", serde_json::json!({"repo": repo})).await?;
        resp.parse()
    }

    async fn list_repos(&self) -> Result<Vec<RepoInfo>, String> {
        let resp = self.request("list_repos", serde_json::json!({})).await?;
        resp.parse()
    }

    async fn execute(&self, repo: &Path, command: Command) -> Result<CommandResult, String> {
        let resp = self.request("execute", serde_json::json!({
            "repo": repo,
            "command": command,
        })).await?;
        resp.parse()
    }

    async fn refresh(&self, repo: &Path) -> Result<(), String> {
        let resp = self.request("refresh", serde_json::json!({"repo": repo})).await?;
        resp.parse_empty()
    }

    async fn add_repo(&self, path: &Path) -> Result<(), String> {
        let resp = self.request("add_repo", serde_json::json!({"path": path})).await?;
        resp.parse_empty()
    }

    async fn remove_repo(&self, path: &Path) -> Result<(), String> {
        let resp = self.request("remove_repo", serde_json::json!({"path": path})).await?;
        resp.parse_empty()
    }
}
```

**Verify:** `cargo check -p flotilla-tui`

**Commit:** `feat: SocketDaemon client implementing DaemonHandle`

---

### Task 6: Subcommand dispatch — restructure main.rs

**Files:**
- Modify: `Cargo.toml` (root)
- Modify: `src/main.rs`

**Changes in root Cargo.toml** — add flotilla-daemon dependency:

```toml
[dependencies]
flotilla-core = { path = "crates/flotilla-core" }
flotilla-daemon = { path = "crates/flotilla-daemon" }
flotilla-tui = { path = "crates/flotilla-tui" }
```

**Changes in main.rs:**

Restructure with clap subcommands. The key changes:

1. Add `Subcommand` enum: `Daemon`, `Status`, `Watch` (no subcommand = TUI)
2. Move existing TUI code into a `run_tui()` function
3. Add `--embedded` flag and `--socket` / `--config-dir` flags
4. Add `run_daemon()`, `run_status()`, `run_watch()` stubs that will be filled in next tasks

```rust
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// Git repo roots (repeatable; auto-detected from cwd if omitted)
    #[arg(long)]
    repo_root: Vec<PathBuf>,

    /// Config directory
    #[arg(long, default_value_t = default_config_dir())]
    config_dir: String,

    /// Socket path (default: ${config_dir}/flotilla.sock)
    #[arg(long)]
    socket: Option<PathBuf>,

    /// Run in embedded mode (no daemon, in-process)
    #[arg(long)]
    embedded: bool,

    #[command(subcommand)]
    command: Option<SubCommand>,
}

#[derive(clap::Subcommand)]
enum SubCommand {
    /// Run the daemon server
    Daemon {
        /// Idle timeout in seconds (0 = no timeout)
        #[arg(long, default_value = "300")]
        timeout: u64,
    },
    /// Print repo list and state
    Status,
    /// Stream daemon events to stdout
    Watch,
}

fn default_config_dir() -> String {
    flotilla_core::config::flotilla_config_dir()
        .to_string_lossy()
        .to_string()
}

impl Cli {
    fn socket_path(&self) -> PathBuf {
        self.socket
            .clone()
            .unwrap_or_else(|| PathBuf::from(&self.config_dir).join("flotilla.sock"))
    }
}
```

The `main()` function dispatches:

```rust
#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match &cli.command {
        Some(SubCommand::Daemon { timeout }) => {
            run_daemon(&cli, *timeout).await
        }
        Some(SubCommand::Status) => {
            run_status(&cli).await
        }
        Some(SubCommand::Watch) => {
            run_watch(&cli).await
        }
        None => {
            run_tui(cli).await
        }
    }
}
```

Move the existing TUI logic into `run_tui()`, keeping `--embedded` as the only mode for now (socket connection will come in Task 8).

**Verify:** `cargo build` and `cargo run -- --help` shows subcommands

**Commit:** `feat: add subcommand dispatch for daemon, status, watch`

---

### Task 7: Daemon subcommand — wire up server

**Files:**
- Modify: `src/main.rs`

**Changes:**

Implement `run_daemon()`:

```rust
async fn run_daemon(cli: &Cli, timeout_secs: u64) -> Result<()> {
    // Initialize logging to stderr
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter("info")
        .init();

    let config_dir = PathBuf::from(&cli.config_dir);
    let socket_path = cli.socket_path();
    let timeout = if timeout_secs == 0 {
        Duration::from_secs(u64::MAX)
    } else {
        Duration::from_secs(timeout_secs)
    };

    // Load repos from config
    let repo_roots = flotilla_core::config::load_repos();
    info!("starting daemon with {} repo(s)", repo_roots.len());

    let server = flotilla_daemon::server::DaemonServer::new(
        repo_roots,
        socket_path,
        timeout,
    ).await;

    server.run().await.map_err(|e| color_eyre::eyre::eyre!(e))
}
```

Add signal handling — the daemon should clean up on SIGTERM/SIGINT. This can be done inside `DaemonServer::run()` by adding a `tokio::signal` branch to the select loop.

**Verify:** `cargo run -- daemon --help` and `cargo run -- daemon --timeout 10` starts and listens

**Commit:** `feat: wire up daemon subcommand`

---

### Task 8: TUI startup — auto-spawn and connect

**Files:**
- Modify: `src/main.rs`

**Changes:**

Update `run_tui()` to support both embedded and socket modes:

```rust
async fn run_tui(cli: Cli) -> Result<()> {
    color_eyre::install()?;
    event_log::init();
    let startup = std::time::Instant::now();

    let daemon: Arc<dyn DaemonHandle> = if cli.embedded {
        // Embedded mode — current behavior
        let repo_roots = resolve_repo_roots(&cli.repo_root);
        if repo_roots.is_empty() {
            eprintln!("Error: no git repositories found (use --repo-root to specify)");
            std::process::exit(1);
        }
        let daemon = InProcessDaemon::new(repo_roots).await;
        info!("embedded daemon started in {:.0?}", startup.elapsed());
        daemon as Arc<dyn DaemonHandle>
    } else {
        // Socket mode — connect or auto-spawn
        let socket_path = cli.socket_path();
        let daemon = match connect_or_spawn(&socket_path, &cli).await {
            Ok(d) => d,
            Err(e) => {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        };
        info!("connected to daemon in {:.0?}", startup.elapsed());
        daemon as Arc<dyn DaemonHandle>
    };

    // Rest of TUI startup unchanged...
    let repos_info = daemon.list_repos().await.unwrap_or_default();
    // ...
}
```

The `connect_or_spawn` function:

```rust
async fn connect_or_spawn(
    socket_path: &Path,
    cli: &Cli,
) -> Result<Arc<SocketDaemon>, String> {
    // Try to connect to existing daemon
    if let Ok(daemon) = SocketDaemon::connect(socket_path).await {
        return Ok(daemon);
    }

    // Clean up stale socket
    let _ = std::fs::remove_file(socket_path);

    // Spawn daemon process
    let exe = std::env::current_exe().map_err(|e| format!("can't find self: {e}"))?;
    let mut cmd = std::process::Command::new(&exe);
    cmd.arg("daemon");
    cmd.arg("--config-dir").arg(&cli.config_dir);
    if let Some(ref socket) = cli.socket {
        cmd.arg("--socket").arg(socket);
    }
    // Detach: redirect stdio, don't inherit
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::null());
    cmd.spawn().map_err(|e| format!("failed to spawn daemon: {e}"))?;

    // Retry connection with backoff
    let delays = [50, 100, 200, 400, 800];
    for delay_ms in delays {
        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        if let Ok(daemon) = SocketDaemon::connect(socket_path).await {
            return Ok(daemon);
        }
    }

    Err("timed out waiting for daemon to start".into())
}
```

**Verify:** Kill any running daemon. Run `cargo run` (no `--embedded`). Verify it spawns a daemon and connects.

**Commit:** `feat: TUI auto-spawns daemon and connects via socket`

---

### Task 9: Status subcommand

**Files:**
- Modify: `src/main.rs`

**Changes:**

Implement `run_status()`:

```rust
async fn run_status(cli: &Cli) -> Result<()> {
    let socket_path = cli.socket_path();
    let daemon = SocketDaemon::connect(&socket_path)
        .await
        .map_err(|e| color_eyre::eyre::eyre!("cannot connect to daemon: {e}"))?;

    let repos = daemon.list_repos().await
        .map_err(|e| color_eyre::eyre::eyre!("{e}"))?;

    if repos.is_empty() {
        println!("No repos tracked.");
        return Ok(());
    }

    for repo in &repos {
        let name = &repo.name;
        let path = repo.path.display();
        let health: Vec<String> = repo.provider_health.iter()
            .map(|(k, v)| format!("{k}: {}", if *v { "ok" } else { "error" }))
            .collect();
        let loading = if repo.loading { " (loading)" } else { "" };
        println!("{name}{loading}  {path}");
        if !health.is_empty() {
            println!("  providers: {}", health.join(", "));
        }
    }

    Ok(())
}
```

**Verify:** Start daemon, then `cargo run -- status`

**Commit:** `feat: add status subcommand`

---

### Task 10: Watch subcommand

**Files:**
- Modify: `src/main.rs`

**Changes:**

Implement `run_watch()`:

```rust
async fn run_watch(cli: &Cli) -> Result<()> {
    let socket_path = cli.socket_path();
    let daemon = SocketDaemon::connect(&socket_path)
        .await
        .map_err(|e| color_eyre::eyre::eyre!("cannot connect to daemon: {e}"))?;

    let mut rx = daemon.subscribe();
    println!("watching events (Ctrl-C to stop)...");

    loop {
        match rx.recv().await {
            Ok(event) => {
                let json = serde_json::to_string_pretty(&event)
                    .unwrap_or_else(|_| format!("{event:?}"));
                println!("{json}");
            }
            Err(broadcast::error::RecvError::Lagged(n)) => {
                eprintln!("warning: skipped {n} events");
            }
            Err(_) => {
                eprintln!("daemon disconnected");
                break;
            }
        }
    }

    Ok(())
}
```

**Verify:** Start daemon, then `cargo run -- watch` in one terminal. Run the TUI in another. Verify events appear.

**Commit:** `feat: add watch subcommand`

---

### Task 11: Signal handling in daemon

**Files:**
- Modify: `crates/flotilla-daemon/src/server.rs`

**Changes:**

Add SIGTERM/SIGINT handling to the daemon's accept loop:

```rust
// In DaemonServer::run(), add to the select loop:
tokio::select! {
    accept = listener.accept() => { ... }
    _ = shutdown_rx.changed() => { break; }
    _ = tokio::signal::ctrl_c() => {
        info!("received signal — shutting down");
        break;
    }
}
```

Also ensure the socket file is removed on any exit path (signal, idle timeout, error).

**Verify:** Start daemon, send SIGTERM, verify socket file is cleaned up.

**Commit:** `feat: daemon signal handling and cleanup`

---

### Task 12: Integration test — full round trip

**Files:**
- Create: `crates/flotilla-daemon/tests/socket_roundtrip.rs`

**Changes:**

```rust
use std::path::PathBuf;
use std::time::Duration;

use flotilla_core::daemon::DaemonHandle;
use flotilla_daemon::server::DaemonServer;

#[tokio::test]
async fn socket_roundtrip() {
    let tmp = tempfile::TempDir::new().unwrap();
    let socket_path = tmp.path().join("test.sock");

    // Start daemon with current repo
    let repo = std::env::current_dir().unwrap();
    let server = DaemonServer::new(
        vec![repo.clone()],
        socket_path.clone(),
        Duration::from_secs(300),
    ).await;

    let server_handle = tokio::spawn(async move {
        let _ = server.run().await;
    });

    // Wait for socket to appear
    for _ in 0..20 {
        if socket_path.exists() { break; }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Connect client
    let client = flotilla_tui::socket::SocketDaemon::connect(&socket_path)
        .await
        .expect("connect");

    // list_repos
    let repos = client.list_repos().await.expect("list_repos");
    assert!(!repos.is_empty());
    assert_eq!(repos[0].path, repo);

    // get_state
    let snapshot = client.get_state(&repo).await.expect("get_state");
    assert_eq!(snapshot.repo, repo);

    // refresh
    client.refresh(&repo).await.expect("refresh");

    // Wait for a snapshot event
    let mut rx = client.subscribe();
    let event = tokio::time::timeout(Duration::from_secs(10), rx.recv())
        .await
        .expect("timeout waiting for event")
        .expect("recv");
    assert!(matches!(event, flotilla_protocol::DaemonEvent::Snapshot(_)));

    server_handle.abort();
}
```

Add `tempfile` and `flotilla-tui` as dev-dependencies to `flotilla-daemon/Cargo.toml`:

```toml
[dev-dependencies]
tempfile = "3"
flotilla-tui = { path = "../flotilla-tui" }
```

**Verify:** `cargo test -p flotilla-daemon -- socket_roundtrip`

**Commit:** `test: socket round-trip integration test`

---

### Task 13: Final verification and cleanup

**Steps:**

1. `cargo fmt`
2. `cargo clippy --workspace`
3. `cargo test --workspace`
4. Manual smoke tests:
   - `cargo run -- daemon &` then `cargo run -- status`
   - `cargo run -- watch` in one terminal, TUI (no --embedded) in another
   - Kill daemon, verify TUI exits with error
   - Start TUI without daemon running, verify auto-spawn
   - `cargo run -- --embedded` — verify current behavior preserved
5. Fix any issues found.

**Commit:** `chore: cleanup and formatting` (only if needed)
