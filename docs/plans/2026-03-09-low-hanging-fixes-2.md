# Low-Hanging Fixes (Batch 2) Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Fix three independent issues: #139 (gap recovery replay_since sends only one repo), #105 (idle timeout polling → notification), #132 (show active search indicator in status bar).

**Architecture:** Three independent fixes across socket client, daemon server, and TUI. No cross-cutting concerns.

**Tech Stack:** Rust, tokio, ratatui

---

### Task 1: #139 — Fix gap recovery to include all repos in replay_since

**Files:**
- Modify: `crates/flotilla-tui/src/socket.rs:327-361`

**Step 1: Fix `last_seen` to include all tracked repos**

At line 327-328, replace the single-repo `last_seen` with all entries from `local_seqs`:

```rust
    let last_seen = {
        let seqs = local_seqs.read().unwrap();
        seqs.iter()
            .map(|(path, &seq)| (path.clone(), seq))
            .collect::<HashMap<_, _>>()
    };
```

**Step 2: Remove the `repo ==` guards on replay event processing**

At lines 350-361, remove the `if snap.repo == repo` and `if delta.repo == repo` guards so all replay events update `local_seqs`:

```rust
                    match &event {
                        DaemonEvent::SnapshotFull(snap) => {
                            local_seqs
                                .write()
                                .unwrap()
                                .insert(snap.repo.clone(), snap.seq);
                        }
                        DaemonEvent::SnapshotDelta(delta) => {
                            local_seqs
                                .write()
                                .unwrap()
                                .insert(delta.repo.clone(), delta.seq);
                        }
                        _ => {}
                    }
```

**Step 3: Run tests**

Run: `cargo test --locked --workspace`
Expected: PASS

**Step 4: Commit**

```bash
git add crates/flotilla-tui/src/socket.rs
git commit -m "fix: include all repos in gap recovery replay_since (#139)"
```

---

### Task 2: #105 — Replace idle timeout polling with tokio::sync::Notify

**Files:**
- Modify: `crates/flotilla-daemon/src/server.rs:1-101,113,122`

**Step 1: Add Notify to DaemonServer struct and imports**

Add `tokio::sync::Notify` to the import at line 8 (alongside the existing `watch` import):

```rust
use tokio::sync::{watch, Notify};
```

Add to the `DaemonServer` struct (line 18-25) a new field:

```rust
pub struct DaemonServer {
    daemon: Arc<InProcessDaemon>,
    socket_path: PathBuf,
    idle_timeout: Duration,
    client_count: Arc<AtomicUsize>,
    client_notify: Arc<Notify>,
    shutdown_tx: watch::Sender<bool>,
    shutdown_rx: watch::Receiver<bool>,
}
```

Initialize it in `new()` (line 42-49):

```rust
        Self {
            daemon,
            socket_path,
            idle_timeout,
            client_count: Arc::new(AtomicUsize::new(0)),
            client_notify: Arc::new(Notify::new()),
            shutdown_tx,
            shutdown_rx,
        }
```

**Step 2: Replace the idle watcher task**

Replace lines 78-101 (the idle timeout watcher) with:

```rust
        // Spawn idle timeout watcher
        let idle_client_count = Arc::clone(&client_count);
        let idle_shutdown_tx = shutdown_tx.clone();
        let idle_notify = Arc::clone(&self.client_notify);
        tokio::spawn(async move {
            loop {
                // Wait until zero clients
                loop {
                    if idle_client_count.load(Ordering::SeqCst) == 0 {
                        break;
                    }
                    idle_notify.notified().await;
                }

                info!(
                    "no clients connected, waiting {} seconds before shutdown",
                    idle_timeout.as_secs()
                );

                // Race: timeout vs client count change
                tokio::select! {
                    () = tokio::time::sleep(idle_timeout) => {
                        if idle_client_count.load(Ordering::SeqCst) == 0 {
                            info!("idle timeout reached, shutting down");
                            let _ = idle_shutdown_tx.send(true);
                            return;
                        }
                        // Client connected during the sleep — loop back
                    }
                    () = idle_notify.notified() => {
                        // Client count changed — loop back to re-check
                    }
                }
            }
        });
```

**Step 3: Signal the Notify on connect and disconnect**

Extract `client_notify` alongside `client_count` (after line 72):

```rust
        let client_notify = self.client_notify;
```

At line 113 (client connect), add a notify call after `fetch_add`:

```rust
                            let count = client_count.fetch_add(1, Ordering::SeqCst) + 1;
                            info!("client connected (total: {count})");
                            client_notify.notify_one();
```

At line 122 (client disconnect), add a notify call. The disconnect happens inside a spawned task, so clone the notify Arc into it. Change the spawn block (lines 116-124) to:

```rust
                            let daemon = Arc::clone(&daemon);
                            let client_count = Arc::clone(&client_count);
                            let client_notify = Arc::clone(&client_notify);
                            let shutdown_rx = shutdown_rx.clone();

                            tokio::spawn(async move {
                                handle_client(stream, daemon, shutdown_rx).await;
                                let count = client_count.fetch_sub(1, Ordering::SeqCst) - 1;
                                info!("client disconnected (total: {count})");
                                client_notify.notify_one();
                            });
```

**Step 4: Run tests and build**

Run: `cargo build --locked && cargo test --locked --workspace`
Expected: compiles cleanly, all tests PASS

**Step 5: Commit**

```bash
git add crates/flotilla-daemon/src/server.rs
git commit -m "fix: replace idle timeout polling with tokio::sync::Notify (#105)"
```

---

### Task 3: #132 — Show active search indicator in status bar

**Files:**
- Modify: `crates/flotilla-tui/src/app/ui_state.rs:60-67` (add field)
- Modify: `crates/flotilla-tui/src/app/key_handlers.rs:77-84,424-435` (Esc handling + store query)
- Modify: `crates/flotilla-tui/src/ui.rs:172-193` (status bar rendering)

**Step 1: Add `active_search_query` to `RepoUiState`**

In `crates/flotilla-tui/src/app/ui_state.rs`, add a field to `RepoUiState` (line 60-67):

```rust
pub struct RepoUiState {
    pub table_view: GroupedWorkItems,
    pub table_state: TableState,
    pub selected_selectable_idx: Option<usize>,
    pub has_unseen_changes: bool,
    pub multi_selected: HashSet<WorkItemIdentity>,
    pub show_providers: bool,
    pub active_search_query: Option<String>,
}
```

This field is `Option<String>` and defaults to `None` via `Default` — but `RepoUiState` derives `Default`, and `Option<String>` defaults to `None`, so no manual `Default` impl change is needed.

**Step 2: Store query on search submit**

In `crates/flotilla-tui/src/app/key_handlers.rs`, in `handle_issue_search_key` (line 424-435), after pushing the `SearchIssues` command, store the query on the active repo's UI state:

```rust
            KeyCode::Enter => {
                let query = if let UiMode::IssueSearch { ref input } = self.ui.mode {
                    input.value().to_string()
                } else {
                    return;
                };
                if !query.is_empty() {
                    let repo = self.model.active_repo_root().clone();
                    self.proto_commands
                        .push(Command::SearchIssues { repo, query: query.clone() });
                    self.active_ui_mut().active_search_query = Some(query);
                }
                self.ui.mode = UiMode::Normal;
            }
```

Key change: add `.clone()` to `query` in the `SearchIssues` push, and store `Some(query)` on the active UI state.

**Step 3: Clear query on Esc in IssueSearch mode**

In the same function, the `KeyCode::Esc` arm (line 419-422) already sends `ClearIssueSearch`. Also clear the stored query:

```rust
            KeyCode::Esc => {
                let repo = self.model.active_repo_root().clone();
                self.proto_commands.push(Command::ClearIssueSearch { repo });
                self.active_ui_mut().active_search_query = None;
                self.ui.mode = UiMode::Normal;
            }
```

**Step 4: Add Esc in Normal mode to clear active search**

In `handle_normal_key` (line 77-85), add a check for active search before existing Esc behavior:

```rust
            KeyCode::Esc => {
                if self.active_ui().active_search_query.is_some() {
                    let repo = self.model.active_repo_root().clone();
                    self.proto_commands.push(Command::ClearIssueSearch { repo });
                    self.active_ui_mut().active_search_query = None;
                } else if self.active_ui().show_providers {
                    self.active_ui_mut().show_providers = false;
                } else if !self.active_ui().multi_selected.is_empty() {
                    self.active_ui_mut().multi_selected.clear();
                } else {
                    self.should_quit = true;
                }
            }
```

**Step 5: Show search indicator in status bar**

In `crates/flotilla-tui/src/ui.rs`, in the `UiMode::Normal` arm of `render_status_bar` (lines 172-193), add a check for the active search query. Replace the entire `UiMode::Normal` block:

```rust
        UiMode::Normal => {
            if rui.show_providers {
                " c:close providers  [/]:switch tab  ?:help  q:quit".into()
            } else if rui.active_search_query.is_some() {
                let q = rui.active_search_query.as_deref().unwrap();
                format!(" search: \"{q}\"  /:new search  esc:clear  ?:help  q:quit")
            } else if !rui.multi_selected.is_empty() {
                " enter:create branch  space:toggle  esc:clear  ?:help  q:quit".into()
            } else {
                let mut s = " enter:open".to_string();
                if let Some(item) = selected_work_item(model, ui) {
                    let labels = model.active_labels();
                    for &intent in Intent::all_in_menu_order() {
                        if let Some(hint) = intent.shortcut_hint(labels) {
                            if intent.is_available(item) {
                                s.push_str("  ");
                                s.push_str(&hint);
                            }
                        }
                    }
                }
                s.push_str("  .:menu  /:search  n:new  r:refresh  space:select  ?:help  q:quit");
                s
            }
        }
```

Note: the `active_search_query` check goes after `show_providers` but before `multi_selected`.

**Step 6: Run tests**

Run: `cargo test --locked --workspace`
Expected: PASS

**Step 7: Commit**

```bash
git add crates/flotilla-tui/src/app/ui_state.rs crates/flotilla-tui/src/app/key_handlers.rs crates/flotilla-tui/src/ui.rs
git commit -m "feat: show active search indicator in status bar with Esc to clear (#132)"
```

---

### Task 4: Final verification

**Step 1: Run full test suite and lints**

```bash
cargo fmt
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked --workspace
```

Expected: all pass, no warnings
