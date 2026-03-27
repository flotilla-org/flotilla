# Archived/Expired Sessions Toggle Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let archived and expired cloud agent sessions flow through to the UI, hidden by default with a per-tab toggle to reveal them dimmed.

**Architecture:** Remove provider-level session filters (Claude, Cursor), add `GroupedWorkItems::filter_archived_sessions` to strip archived/expired session-only items in the TUI, controlled by a `show_archived` toggle on `RepoPage`.

**Tech Stack:** Rust, ratatui, flotilla-protocol, flotilla-core, flotilla-tui

**Spec:** `docs/superpowers/specs/2026-03-22-archived-sessions-toggle-design.md`

---

### Task 1: Distinguish expired from archived icons

**Files:**
- Modify: `crates/flotilla-tui/src/ui_helpers.rs:94-97,107-112`

- [ ] **Step 1: Write failing tests**

In the existing `session_status_display_all` test (line 390), the assertion for `Expired` currently expects `"○"`. Add a new test that checks the distinction:

```rust
#[test]
fn expired_icon_differs_from_archived() {
    assert_ne!(
        session_status_display(&SessionStatus::Expired),
        session_status_display(&SessionStatus::Archived),
        "expired and archived should have distinct icons"
    );
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p flotilla-tui expired_icon_differs`
Expected: FAIL — both currently return `"○"`

- [ ] **Step 3: Update icon functions**

In `session_status_display` (line 107), split the match arm:

```rust
pub fn session_status_display(status: &SessionStatus) -> &'static str {
    match status {
        SessionStatus::Running => "▶",
        SessionStatus::Idle => "◆",
        SessionStatus::Archived => "○",
        SessionStatus::Expired => "⊘",
    }
}
```

In `work_item_icon` (line 94), split the catch-all arm:

```rust
WorkItemKind::Session => match session_status {
    Some(SessionStatus::Running) => ("▶", theme.session),
    Some(SessionStatus::Idle) => ("◆", theme.session),
    Some(SessionStatus::Archived) => ("○", theme.session),
    Some(SessionStatus::Expired) => ("⊘", theme.session),
    None => ("○", theme.session),
},
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p flotilla-tui -- session_status`
Expected: PASS

- [ ] **Step 5: Update the existing snapshot assertion**

The `session_status_display_all` test (line 394) asserts `Expired` → `"○"`. Update to `"⊘"`.

- [ ] **Step 6: Run all TUI tests**

Run: `cargo test -p flotilla-tui --locked`
Expected: PASS

- [ ] **Step 7: Commit**

```
feat: distinguish expired (⊘) from archived (○) session icons
```

---

### Task 2: Remove provider-level session filters

**Files:**
- Modify: `crates/flotilla-core/src/providers/coding_agent/claude.rs:229`
- Modify: `crates/flotilla-core/src/providers/coding_agent/cursor.rs:187`
- Modify: `crates/flotilla-core/src/providers/coding_agent/fixtures/claude_sessions.yaml` (if needed)

- [ ] **Step 1: Update Claude provider test**

In `claude.rs`, find the test `fetch_sessions_inner_filters_archived_sorts_and_sends_auth_header` (line 414). Update:

```rust
#[tokio::test]
async fn fetch_sessions_inner_includes_archived_and_sorts() {
    let _test_lock = TEST_LOCK.lock().await;
    reset_auth_state();

    let runner = mock_runner(vec![Ok(token_json("abc123", now_epoch_secs() + 3600))]);
    let session = replay::test_session(&fixture("claude_sessions.yaml"), replay::Masks::new());
    let http = replay::test_http_client(&session);
    let agent = make_agent(runner, http);

    let sessions = agent.fetch_sessions_inner("https://api.test").await.expect("fetch sessions");
    session.finish();

    assert_eq!(sessions.len(), 3, "all sessions including archived should be returned");
    assert_eq!(sessions[0].id, "skip", "archived session with newest timestamp first");
    assert_eq!(sessions[1].id, "new");
    assert_eq!(sessions[2].id, "old");
}
```

Note: The fixture has 3 sessions: `old` (running, 2026-03-01), `skip` (archived, 2026-03-03), `new` (idle, 2026-03-02). After removing the filter, sorted desc by `updated_at`: skip, new, old.

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p flotilla-core fetch_sessions_inner_includes_archived`
Expected: FAIL — filter still removes archived

- [ ] **Step 3: Remove the Claude filter**

In `claude.rs` line 229, change:

```rust
let mut sessions: Vec<WebSession> = parsed.data.into_iter().filter(|s| s.session_status != "archived").collect();
```

to:

```rust
let mut sessions: Vec<WebSession> = parsed.data.into_iter().collect();
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p flotilla-core fetch_sessions_inner_includes_archived`
Expected: PASS

- [ ] **Step 5: Remove the Cursor filter**

In `cursor.rs` line 187, remove the `.filter(|a| a.session_status() != SessionStatus::Expired)` line. The remaining chain continues with `.filter(|a| a.repo_slug()...)`.

- [ ] **Step 6: Run all core tests**

Run: `cargo test -p flotilla-core --locked`
Expected: PASS

- [ ] **Step 7: Commit**

```
feat: stop filtering archived/expired sessions at provider level
```

---

### Task 3: Add `Action::ToggleArchived` and key binding

**Files:**
- Modify: `crates/flotilla-tui/src/keymap.rs:14-44,77-110`
- Modify: `crates/flotilla-tui/src/binding_table.rs:127-128`

- [ ] **Step 1: Write failing test**

In `keymap.rs` tests, add a round-trip config string test:

```rust
#[test]
fn toggle_archived_round_trips() {
    assert_eq!(Action::from_config_str("toggle_archived"), Some(Action::ToggleArchived));
    assert_eq!(Action::ToggleArchived.as_config_str(), "toggle_archived");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p flotilla-tui toggle_archived_round_trips`
Expected: FAIL — no `ToggleArchived` variant

- [ ] **Step 3: Add the Action variant**

In `keymap.rs`, add `ToggleArchived` to the `Action` enum (after `ToggleProviders` at line 32):

```rust
ToggleArchived,
```

Add to `from_config_str` (after `toggle_providers` at line 95):

```rust
"toggle_archived" => Action::ToggleArchived,
```

Add to `as_config_str` (after `toggle_providers` at line 140):

```rust
Action::ToggleArchived => "toggle_archived",
```

Add to `description()` (after `ToggleProviders` at line 182):

```rust
Action::ToggleArchived => "Toggle archived sessions",
```

Add to `help_sections` in the "General" section (line 349, after `ToggleProviders`):

```rust
Action::ToggleArchived,
```

- [ ] **Step 4: Add the key binding**

In `binding_table.rs`, add after the `ToggleProviders` line (line 128):

```rust
b(BindingModeId::Normal, "u", Action::ToggleArchived),
```

- [ ] **Step 5: Run tests**

Run: `cargo test -p flotilla-tui --locked`
Expected: PASS

- [ ] **Step 6: Commit**

```
feat: add ToggleArchived action bound to 'u' in Normal mode
```

---

### Task 4: Add `GroupedWorkItems::filter_archived_sessions`

**Files:**
- Modify: `crates/flotilla-core/src/data.rs`

- [ ] **Step 1: Write failing test**

Add a test in the `data.rs` test module. Neither `WorkItem` nor `CloudAgentSession` implement `Default`, so construct all fields explicitly. Add a test helper to reduce boilerplate:

```rust
fn test_session_work_item(id: &str) -> flotilla_protocol::WorkItem {
    flotilla_protocol::WorkItem {
        kind: WorkItemKind::Session,
        identity: WorkItemIdentity::Session(id.into()),
        host: flotilla_protocol::HostName::local(),
        branch: None,
        description: format!("session {id}"),
        checkout: None,
        change_request_key: None,
        session_key: Some(id.into()),
        issue_keys: Vec::new(),
        workspace_refs: Vec::new(),
        is_main_checkout: false,
        debug_group: Vec::new(),
        source: None,
        terminal_keys: Vec::new(),
        attachable_set_id: None,
        agent_keys: Vec::new(),
    }
}

fn test_cloud_agent_session(status: flotilla_protocol::SessionStatus) -> flotilla_protocol::CloudAgentSession {
    flotilla_protocol::CloudAgentSession {
        title: String::new(),
        status,
        model: None,
        updated_at: None,
        correlation_keys: Vec::new(),
        provider_name: String::new(),
        provider_display_name: String::new(),
        item_noun: String::new(),
    }
}

#[test]
fn filter_archived_sessions_removes_archived_and_expired() {
    use flotilla_protocol::SessionStatus;

    let active = test_session_work_item("s1");
    let archived = test_session_work_item("s2");

    let checkout = flotilla_protocol::WorkItem {
        kind: WorkItemKind::Checkout,
        identity: WorkItemIdentity::Checkout(flotilla_protocol::HostPath::new(
            flotilla_protocol::HostName::local(),
            std::path::PathBuf::from("/tmp/co"),
        )),
        host: flotilla_protocol::HostName::local(),
        branch: Some("main".into()),
        description: "checkout".into(),
        checkout: None,
        change_request_key: None,
        session_key: None,
        issue_keys: Vec::new(),
        workspace_refs: Vec::new(),
        is_main_checkout: false,
        debug_group: Vec::new(),
        source: None,
        terminal_keys: Vec::new(),
        attachable_set_id: None,
        agent_keys: Vec::new(),
    };

    let mut grouped = GroupedWorkItems::default();
    grouped.table_entries.push(GroupEntry::Header(SectionHeader("Sessions".into())));
    grouped.selectable_indices.push(1);
    grouped.table_entries.push(GroupEntry::Item(Box::new(active)));
    grouped.selectable_indices.push(2);
    grouped.table_entries.push(GroupEntry::Item(Box::new(archived)));
    grouped.table_entries.push(GroupEntry::Header(SectionHeader("Checkouts".into())));
    grouped.selectable_indices.push(4);
    grouped.table_entries.push(GroupEntry::Item(Box::new(checkout)));

    let mut providers = ProviderData::default();
    providers.sessions.insert("s1".into(), test_cloud_agent_session(SessionStatus::Running));
    providers.sessions.insert("s2".into(), test_cloud_agent_session(SessionStatus::Archived));

    let filtered = grouped.filter_archived_sessions(&providers);

    assert_eq!(filtered.selectable_indices.len(), 2);
    let header_count = filtered.table_entries.iter().filter(|e| matches!(e, GroupEntry::Header(_))).count();
    assert_eq!(header_count, 2); // Sessions header + Checkouts header both remain
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p flotilla-core filter_archived_sessions`
Expected: FAIL — method doesn't exist

- [ ] **Step 3: Implement the filter method**

Add to `GroupedWorkItems` in `data.rs`:

```rust
impl GroupedWorkItems {
    /// Return a new GroupedWorkItems with archived/expired session-only items removed.
    /// Agent items are never filtered. Items with non-Session kinds are kept.
    pub fn filter_archived_sessions(&self, providers: &ProviderData) -> GroupedWorkItems {
        let mut entries = Vec::new();
        let mut selectable = Vec::new();

        for entry in &self.table_entries {
            match entry {
                GroupEntry::Item(item) => {
                    if item.kind == WorkItemKind::Session {
                        let is_archived = item
                            .session_key
                            .as_deref()
                            .and_then(|k| providers.sessions.get(k))
                            .is_some_and(|s| matches!(s.status, SessionStatus::Archived | SessionStatus::Expired));
                        if is_archived {
                            continue;
                        }
                    }
                    selectable.push(entries.len());
                    entries.push(entry.clone());
                }
                GroupEntry::Header(_) => {
                    entries.push(entry.clone());
                }
            }
        }

        // Remove orphaned headers (header followed by another header or end-of-list)
        let mut cleaned = Vec::new();
        let mut cleaned_selectable = Vec::new();
        for (i, entry) in entries.iter().enumerate() {
            if let GroupEntry::Header(_) = entry {
                let next_is_item = entries.get(i + 1).is_some_and(|e| matches!(e, GroupEntry::Item(_)));
                if !next_is_item {
                    continue;
                }
            }
            if selectable.contains(&i) {
                cleaned_selectable.push(cleaned.len());
            }
            cleaned.push(entry.clone());
        }

        GroupedWorkItems { table_entries: cleaned, selectable_indices: cleaned_selectable }
    }
}
```

- [ ] **Step 4: Add a test for orphaned header removal**

```rust
#[test]
fn filter_archived_sessions_removes_orphaned_headers() {
    use flotilla_protocol::SessionStatus;

    let archived = test_session_work_item("s1");

    let mut grouped = GroupedWorkItems::default();
    grouped.table_entries.push(GroupEntry::Header(SectionHeader("Sessions".into())));
    grouped.selectable_indices.push(1);
    grouped.table_entries.push(GroupEntry::Item(Box::new(archived)));

    let mut providers = ProviderData::default();
    providers.sessions.insert("s1".into(), test_cloud_agent_session(SessionStatus::Archived));

    let filtered = grouped.filter_archived_sessions(&providers);
    assert!(filtered.table_entries.is_empty());
    assert!(filtered.selectable_indices.is_empty());
}
```

- [ ] **Step 5: Add a test that Agent items are never filtered**

```rust
#[test]
fn filter_archived_sessions_keeps_agent_items() {
    let agent = flotilla_protocol::WorkItem {
        kind: WorkItemKind::Agent,
        identity: WorkItemIdentity::Agent("a1".into()),
        host: flotilla_protocol::HostName::local(),
        branch: None,
        description: "agent".into(),
        checkout: None,
        change_request_key: None,
        session_key: None,
        issue_keys: Vec::new(),
        workspace_refs: Vec::new(),
        is_main_checkout: false,
        debug_group: Vec::new(),
        source: None,
        terminal_keys: Vec::new(),
        attachable_set_id: None,
        agent_keys: vec!["a1".into()],
    };

    let mut grouped = GroupedWorkItems::default();
    grouped.table_entries.push(GroupEntry::Header(SectionHeader("Agents".into())));
    grouped.selectable_indices.push(1);
    grouped.table_entries.push(GroupEntry::Item(Box::new(agent)));

    let providers = ProviderData::default();
    let filtered = grouped.filter_archived_sessions(&providers);

    assert_eq!(filtered.selectable_indices.len(), 1);
    assert_eq!(filtered.table_entries.len(), 2); // header + agent
}
```

- [ ] **Step 6: Run tests**

Run: `cargo test -p flotilla-core --locked`
Expected: PASS

- [ ] **Step 7: Commit**

```
feat: add GroupedWorkItems::filter_archived_sessions helper
```

---

### Task 5: Wire toggle into RepoPage

**Files:**
- Modify: `crates/flotilla-tui/src/widgets/repo_page.rs`

- [ ] **Step 1: Write failing test for toggle action**

In `repo_page.rs` tests, add:

```rust
#[test]
fn toggle_archived_flips_show_archived() {
    let harness = TestWidgetHarness::new();
    let data = test_repo_data(vec![]);
    let mut page = RepoPage::new(test_repo_identity(), data, RepoViewLayout::Auto);

    assert!(!page.show_archived);
    let mut ctx = harness.widget_context();
    page.handle_action(Action::ToggleArchived, &mut ctx);
    assert!(page.show_archived);
    page.handle_action(Action::ToggleArchived, &mut ctx);
    assert!(!page.show_archived);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p flotilla-tui toggle_archived_flips`
Expected: FAIL — no `show_archived` field

- [ ] **Step 3: Add field and handle action**

Add `show_archived: bool` to `RepoPage` struct (after `show_providers` at line 118):

```rust
pub show_archived: bool,
```

Initialize to `false` in `RepoPage::new` (after `show_providers: false` at line 134):

```rust
show_archived: false,
```

Add handler in `handle_action` match (after `ToggleProviders` at line 305):

```rust
Action::ToggleArchived => {
    self.show_archived = !self.show_archived;
    Outcome::Consumed
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p flotilla-tui toggle_archived_flips`
Expected: PASS

- [ ] **Step 5: Wire filtering into reconcile_if_changed**

In `reconcile_if_changed` (line 153), after `group_work_items` returns and before `self.table.update_items`, add filtering:

```rust
let grouped = flotilla_core::data::group_work_items(&data.work_items, &data.providers, &section_labels, &data.path);
let grouped = if self.show_archived {
    grouped
} else {
    grouped.filter_archived_sessions(&data.providers)
};
self.table.update_items(grouped);
```

- [ ] **Step 6: Update dismiss chain**

In `dismiss` method (after the `show_providers` branch at line 256), add:

```rust
} else if self.show_archived {
    self.show_archived = false;
```

- [ ] **Step 7: Update status_fragment**

In `status_fragment` (line 424), add `show_archived` check after `show_providers`:

```rust
fn status_fragment(&self) -> StatusFragment {
    let status = if self.show_providers {
        Some(StatusContent::Label("PROVIDERS".into()))
    } else if let Some(query) = &self.active_search_query {
        Some(StatusContent::Label(format!("SEARCH \"{query}\"")))
    } else if self.show_archived {
        Some(StatusContent::Label("ARCHIVED".into()))
    } else if !self.multi_selected.is_empty() {
        Some(StatusContent::Label(format!("{} SELECTED", self.multi_selected.len())))
    } else {
        None
    };
    StatusFragment { status }
}
```

- [ ] **Step 8: Write test for dismiss chain ordering**

```rust
#[test]
fn dismiss_clears_show_archived_before_multi_select() {
    let harness = TestWidgetHarness::new();
    let data = test_repo_data(vec![issue_item("1")]);
    let mut page = RepoPage::new(test_repo_identity(), data, RepoViewLayout::Auto);

    page.show_archived = true;
    page.multi_selected.insert(WorkItemIdentity::Issue("1".into()));

    let mut ctx = harness.widget_context();
    page.handle_action(Action::Dismiss, &mut ctx);
    assert!(!page.show_archived, "archived cleared first");
    assert!(!page.multi_selected.is_empty(), "multi-select not yet cleared");

    page.handle_action(Action::Dismiss, &mut ctx);
    assert!(page.multi_selected.is_empty(), "now multi-select cleared");
}
```

- [ ] **Step 9: Run all TUI tests**

Run: `cargo test -p flotilla-tui --locked`
Expected: PASS

- [ ] **Step 10: Commit**

```
feat: wire show_archived toggle into RepoPage with filtering and dismiss chain
```

---

### Task 6: Dim archived/expired rows in the table

**Files:**
- Modify: `crates/flotilla-tui/src/widgets/work_item_table.rs:404-549`

- [ ] **Step 1: Add dimming logic to build_item_row**

The `build_item_row` function (line 404) already receives `providers` and can look up session status. After the existing `session_status` lookup at line 414, add a `is_archived` flag:

```rust
let is_archived = session_status.is_some_and(|s| matches!(s, SessionStatus::Archived | SessionStatus::Expired));
```

Then at the end of the function where the normal `Row::new` is built (line 537), wrap the style selection:

```rust
let style_for = |normal_color: Color| -> Style {
    if is_archived {
        Style::default().fg(theme.muted)
    } else {
        Style::default().fg(normal_color)
    }
};

Row::new(vec![
    Cell::from(Span::styled(format!(" {icon}"), if is_archived { Style::default().fg(theme.muted) } else { Style::default().fg(icon_color) })),
    Cell::from(Span::styled(source_display, style_for(theme.source))),
    Cell::from(Span::styled(path_display, style_for(theme.path))),
    Cell::from(Span::styled(description, style_for(theme.text))),
    Cell::from(Span::styled(branch_display, style_for(theme.branch))),
    Cell::from(Span::styled(wt_indicator.to_string(), style_for(theme.checkout))),
    Cell::from(Span::styled(ws_indicator, style_for(theme.workspace))),
    Cell::from(Span::styled(pr_display, style_for(theme.change_request))),
    Cell::from(Span::styled(session_display, style_for(theme.session))),
    Cell::from(Span::styled(issues_display, style_for(theme.issue))),
    Cell::from(Span::styled(git_display, style_for(theme.git_status))),
])
```

- [ ] **Step 2: Run tests and check for snapshot changes**

Run: `cargo test -p flotilla-tui --locked`
Expected: PASS — existing snapshots don't include archived sessions, so no snapshot changes expected.

- [ ] **Step 3: Commit**

```
feat: dim archived/expired session rows with muted colour
```

---

### Task 7: Run full CI checks

- [ ] **Step 1: Format check**

Run: `cargo +nightly-2026-03-12 fmt --check`

- [ ] **Step 2: Clippy**

Run: `cargo clippy --workspace --all-targets --locked -- -D warnings`

- [ ] **Step 3: Full test suite**

Run: `cargo test --workspace --locked`

- [ ] **Step 4: Fix any issues and commit**

---
