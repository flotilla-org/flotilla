# Generic Naming: Command, Intent, and Protocol Type Renames

**Goal:** Replace provider-specific terms (worktree, PR) with generic terms (checkout, change request) in the protocol, command, and TUI layers. Provider implementations keep their specific terms.

**Boundary rule:** Protocol types and TUI code use generic labels. Provider implementations (e.g. `git_worktree.rs`) use implementation-specific terms.

## Renames

### Commands (`flotilla-protocol/src/commands.rs`)

| Current | New | Field changes |
|---|---|---|
| `SwitchWorktree { path }` | `CreateWorkspaceForCheckout { checkout_path }` | `path` -> `checkout_path` |
| `CreateWorktree { branch, create_branch, issue_ids }` | `CreateCheckout { branch, create_branch, issue_ids }` | none |
| `FetchDeleteInfo { branch, worktree_path, pr_number }` | `FetchCheckoutStatus { branch, checkout_path, change_request_id }` | two field renames |
| `OpenPr { id }` | `OpenChangeRequest { id }` | none |
| `OpenIssueBrowser { id }` | `OpenIssue { id }` | none |
| `LinkIssuesToPr { pr_id, issue_ids }` | `LinkIssuesToChangeRequest { change_request_id, issue_ids }` | `pr_id` -> `change_request_id` |

`RemoveCheckout`, `SelectWorkspace`, `ArchiveSession`, `GenerateBranchName`, `TeleportSession`, `AddRepo`, `RemoveRepo`, `Refresh` — unchanged.

### CommandResult (`flotilla-protocol/src/commands.rs`)

| Current | New |
|---|---|
| `WorktreeCreated { branch }` | `CheckoutCreated { branch }` |
| `DeleteInfo(DeleteInfo)` | `CheckoutStatus(CheckoutStatus)` |

Struct `DeleteInfo` -> `CheckoutStatus`. Fields:
- `pr_status` -> `change_request_status`
- rest unchanged (`branch`, `merge_commit_sha`, `unpushed_commits`, `has_uncommitted`, `base_detection_warning`)

### Snapshot types (`flotilla-protocol/src/snapshot.rs`)

| Current | New |
|---|---|
| `WorkItem::pr_key` | `change_request_key` |
| `WorkItem::is_main_worktree` | `is_main_checkout` |
| `CheckoutRef::is_main_worktree` | `is_main_checkout` |

### Core types (`flotilla-core/src/data.rs`)

| Current | New |
|---|---|
| `CorrelatedWorkItem::linked_pr` | `linked_change_request` |
| `fn pr_key()` | `fn change_request_key()` |
| `fn is_main_worktree()` | `fn is_main_checkout()` |
| `fetch_delete_confirm_info(worktree_path, pr_number, ..)` | `fetch_checkout_status(checkout_path, change_request_id, ..)` |

### Intents (`flotilla-tui/src/app/intent.rs`)

| Current | New |
|---|---|
| `RemoveWorktree` | `RemoveCheckout` |
| `CreateWorktreeAndWorkspace` | `CreateCheckoutAndWorkspace` |
| `OpenPr` | `OpenChangeRequest` |
| `LinkIssuesToPr` | `LinkIssuesToChangeRequest` |

### Not renamed

- Provider internals (`git_worktree.rs`, `wt.rs`) — these are git worktrees
- `render_worktree_path` — internal to git worktree provider
- Log messages in provider impls that say "worktree" — they're about actual git worktrees

## Serde compatibility

The `Command` and `CommandResult` enums use `#[serde(tag = "command", rename_all = "snake_case")]`. Renaming variants changes the wire format. This is acceptable — no deployed socket daemon exists yet, so there are no backwards-compatibility constraints.

## Testing

- All existing tests updated to use new names
- `cargo test --workspace` must pass
- `cargo clippy` must be clean
- No behavioral changes — pure rename
