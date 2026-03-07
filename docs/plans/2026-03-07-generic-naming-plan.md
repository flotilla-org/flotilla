# Generic Naming Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Rename Command, Intent, CommandResult, and protocol type fields from provider-specific terms (worktree, PR) to generic terms (checkout, change request).

**Architecture:** Pure rename, bottom-up: protocol crate first (everything depends on it), then core, then TUI, then examples. Each task is one commit. No behavioral changes.

**Tech Stack:** Rust, serde (rename_all = snake_case affects wire format, acceptable since no deployed socket daemon exists).

**Design doc:** `docs/plans/2026-03-07-generic-naming-design.md`

---

### Task 1: Protocol — rename Command variants and fields

**Files:**
- Modify: `crates/flotilla-protocol/src/commands.rs`

**Changes:**

```rust
// commands.rs — Command enum
SwitchWorktree { path } → CreateWorkspaceForCheckout { checkout_path: PathBuf }
CreateWorktree { branch, create_branch, issue_ids } → CreateCheckout { branch, create_branch, issue_ids }
FetchDeleteInfo { branch, worktree_path, pr_number } → FetchCheckoutStatus { branch, checkout_path: Option<PathBuf>, change_request_id: Option<String> }
OpenPr { id } → OpenChangeRequest { id }
OpenIssueBrowser { id } → OpenIssue { id }
LinkIssuesToPr { pr_id, issue_ids } → LinkIssuesToChangeRequest { change_request_id: String, issue_ids }

// commands.rs — CommandResult enum
WorktreeCreated { branch } → CheckoutCreated { branch }
DeleteInfo(DeleteInfo) → CheckoutStatus(CheckoutStatus)

// commands.rs — struct DeleteInfo → CheckoutStatus
pub struct DeleteInfo → pub struct CheckoutStatus
  pr_status → change_request_status
```

**Verify:** `cargo check -p flotilla-protocol` will fail (dependents not updated yet — that's expected). Check that the protocol crate itself compiles: `cargo check -p flotilla-protocol --lib`

**Commit:** `refactor: rename Command/CommandResult variants to generic terms`

---

### Task 2: Protocol — rename snapshot fields

**Files:**
- Modify: `crates/flotilla-protocol/src/snapshot.rs`

**Changes:**

```rust
// WorkItem
pub pr_key: Option<String> → pub change_request_key: Option<String>
pub is_main_worktree: bool → pub is_main_checkout: bool

// CheckoutRef
pub is_main_worktree: bool → pub is_main_checkout: bool
```

**Verify:** `cargo check -p flotilla-protocol --lib`

**Commit:** `refactor: rename snapshot fields to generic terms`

---

### Task 3: Protocol — fix protocol tests

**Files:**
- Modify: `crates/flotilla-protocol/src/lib.rs`

**Changes:** Update all test code referencing renamed variants and fields:
- `WorktreeCreated` → `CheckoutCreated`
- `DeleteInfo` → `CheckoutStatus` (struct and variant)
- `pr_status` → `change_request_status`
- `pr_key` → `change_request_key`
- `is_main_worktree` → `is_main_checkout`
- `worktree_path` → `checkout_path`
- `SwitchWorktree` → `CreateWorkspaceForCheckout`
- `OpenPr` → `OpenChangeRequest`
- etc.

**Verify:** `cargo test -p flotilla-protocol`

**Commit:** `test: update protocol tests for generic naming`

---

### Task 4: Core — rename data.rs types and functions

**Files:**
- Modify: `crates/flotilla-core/src/data.rs`

**Changes:**

```rust
// CorrelatedWorkItem
pub linked_pr: Option<String> → pub linked_change_request: Option<String>

// CorrelationResult methods
pub fn pr_key() → pub fn change_request_key()
pub fn is_main_worktree() → pub fn is_main_checkout()

// Free function
pub async fn fetch_delete_confirm_info(branch, worktree_path, pr_number, repo_root)
→ pub async fn fetch_checkout_status(branch, checkout_path, change_request_id, repo_root)

// Inside fetch_checkout_status:
pr_num → change_request_id (local variable)
wt_path → checkout_path (local variable)

// DeleteInfo field reference
info.pr_status → info.change_request_status
```

Also update all internal references: local variables named `pr_key` in the correlation/grouping logic, field accesses on `WorkItem::pr_key` → `change_request_key`, `is_main_worktree` → `is_main_checkout`.

**Verify:** `cargo check -p flotilla-core`

**Commit:** `refactor: rename core data types to generic terms`

---

### Task 5: Core — rename convert.rs and executor.rs

**Files:**
- Modify: `crates/flotilla-core/src/convert.rs`
- Modify: `crates/flotilla-core/src/executor.rs`

**Changes in convert.rs:**

```rust
// correlation_result_to_work_item
pr_key: item.pr_key() → change_request_key: item.change_request_key()
is_main_worktree: item.is_main_worktree() → is_main_checkout: item.is_main_checkout()
is_main_worktree: co.is_main_worktree → is_main_checkout: co.is_main_checkout

// convert tests
linked_pr → linked_change_request
pr_key → change_request_key
is_main_worktree → is_main_checkout
```

**Changes in executor.rs:**

```rust
// Match arms
Command::SwitchWorktree { path } → Command::CreateWorkspaceForCheckout { checkout_path }
  (use checkout_path instead of path internally)
Command::CreateWorktree { .. } → Command::CreateCheckout { .. }
Command::FetchDeleteInfo { branch, worktree_path, pr_number }
  → Command::FetchCheckoutStatus { branch, checkout_path, change_request_id }
  (call fetch_checkout_status instead of fetch_delete_confirm_info)
Command::OpenPr { id } → Command::OpenChangeRequest { id }
Command::OpenIssueBrowser { id } → Command::OpenIssue { id }
Command::LinkIssuesToPr { pr_id, .. } → Command::LinkIssuesToChangeRequest { change_request_id, .. }

// Return values
CommandResult::WorktreeCreated → CommandResult::CheckoutCreated
CommandResult::DeleteInfo(info) → CommandResult::CheckoutStatus(info)

// Log messages: update "worktree"→"checkout", "PR"→"change request" where they appear in info!/error! macros
```

**Verify:** `cargo check -p flotilla-core`

**Commit:** `refactor: rename convert and executor to generic terms`

---

### Task 6: Core — run core tests

**Files:**
- Modify: `crates/flotilla-core/src/data.rs` (test section)
- Modify: `crates/flotilla-core/src/convert.rs` (test section)

Update any remaining test references. These should already be caught in tasks 4-5, but verify:

**Verify:** `cargo test -p flotilla-core`

**Commit:** Only if changes needed: `test: fix remaining core test references`

---

### Task 7: TUI — rename intents

**Files:**
- Modify: `crates/flotilla-tui/src/app/intent.rs`

**Changes:**

```rust
// Enum variants
RemoveWorktree → RemoveCheckout
CreateWorktreeAndWorkspace → CreateCheckoutAndWorkspace
OpenPr → OpenChangeRequest
LinkIssuesToPr → LinkIssuesToChangeRequest

// In resolve():
Command::SwitchWorktree → Command::CreateWorkspaceForCheckout
  path: → checkout_path:
Command::FetchDeleteInfo → Command::FetchCheckoutStatus
  worktree_path → checkout_path
  pr_number → change_request_id
Command::CreateWorktree → Command::CreateCheckout
Command::OpenPr → Command::OpenChangeRequest
Command::OpenIssueBrowser → Command::OpenIssue
Command::LinkIssuesToPr → Command::LinkIssuesToChangeRequest
  pr_id: → change_request_id:

// In is_available():
is_main_worktree → is_main_checkout
pr_key → change_request_key

// In shortcut_hint():
RemoveWorktree → RemoveCheckout
OpenPr → OpenChangeRequest

// In all_in_menu_order() and enter_priority():
Update variant names
```

**Verify:** `cargo check -p flotilla-tui`

**Commit:** `refactor: rename TUI intents to generic terms`

---

### Task 8: TUI — rename executor and app references

**Files:**
- Modify: `crates/flotilla-tui/src/app/executor.rs`
- Modify: `crates/flotilla-tui/src/app/mod.rs`

**Changes in executor.rs:**

```rust
CommandResult::WorktreeCreated → CommandResult::CheckoutCreated
CommandResult::DeleteInfo(info) → CommandResult::CheckoutStatus(info)
// Update log message
```

**Changes in mod.rs:**

```rust
// All references to renamed intents:
Intent::RemoveWorktree → Intent::RemoveCheckout
Intent::CreateWorktreeAndWorkspace → Intent::CreateCheckoutAndWorkspace
Intent::OpenPr → Intent::OpenChangeRequest
Intent::LinkIssuesToPr → Intent::LinkIssuesToChangeRequest

// Command construction:
Command::CreateWorktree → Command::CreateCheckout
Command::SwitchWorktree → Command::CreateWorkspaceForCheckout

// Field references:
item.pr_key → item.change_request_key
item.is_main_worktree → item.is_main_checkout
worktree_path → checkout_path
```

**Verify:** `cargo check -p flotilla-tui`

**Commit:** `refactor: rename TUI executor and app references`

---

### Task 9: TUI — rename ui.rs references

**Files:**
- Modify: `crates/flotilla-tui/src/ui.rs`

**Changes:**

```rust
// WorkItem field access
item.pr_key → item.change_request_key
item.is_main_worktree → item.is_main_checkout

// DeleteInfo/CheckoutStatus
info.pr_status → info.change_request_status

// Any display text referring to "worktree" in generic context
// (but keep "worktree" where it comes from labels.checkouts.noun)
```

**Verify:** `cargo check`

**Commit:** `refactor: rename ui.rs field references`

---

### Task 10: Examples and integration tests

**Files:**
- Modify: `examples/debug_sessions.rs`
- Modify: `crates/flotilla-core/tests/in_process_daemon.rs`

**Changes:** Update any references to renamed types/fields.

**Verify:** `cargo test --workspace && cargo clippy && cargo fmt --check`

**Commit:** `refactor: update examples and integration tests for generic naming`

---

### Task 11: Final verification and cleanup

**Steps:**

1. `cargo fmt`
2. `cargo clippy`
3. `cargo test --workspace`
4. Grep for any remaining `worktree` references in protocol/TUI layers that should be generic:
   ```bash
   grep -rn 'worktree\|pr_key\|pr_id\|pr_number\|pr_status\|OpenPr\|SwitchWorktree\|CreateWorktree\b\|DeleteInfo\|WorktreeCreated\|RemoveWorktree\|LinkIssuesToPr' \
     crates/flotilla-protocol/src crates/flotilla-tui/src crates/flotilla-core/src/executor.rs \
     crates/flotilla-core/src/convert.rs crates/flotilla-core/src/data.rs \
     crates/flotilla-core/src/daemon.rs examples/
   ```
   Expected: no matches (provider internals like `git_worktree.rs` are excluded from the grep path).
5. Fix any stragglers.

**Commit:** `chore: formatting and final cleanup` (only if needed)
