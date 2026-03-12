# Close PR Action

Add a "Close PR" action to the TUI action menu, with a confirmation dialog.

## Flow

Follows the existing Intent -> Command -> Executor -> Provider pattern.

### 1. Intent & Menu

Add `Intent::CloseChangeRequest` to the enum in `intent.rs`.

- **Availability:** coarse check — `item.change_request_key.is_some()`. Fine-grained status filtering (only open PRs) happens in `resolve()`, which returns `None` for non-open PRs. This matches the `LinkIssuesToChangeRequest` pattern. The action menu already filters out intents where `resolve()` returns `None`.
- **Menu:** included in `all_in_menu_order()`. Not in `enter_priority()` (closing on Enter would be dangerous). `shortcut_hint()` returns `None`.
- **Label:** `format!("Close {}", labels.code_review.noun)` — consistent with `OpenChangeRequest` label pattern.
- **Host filtering:** not in `requires_local_host()` — closing is an API call, not a filesystem operation, same as `OpenChangeRequest`.

### 2. Confirmation Mode

Add `UiMode::CloseConfirm { id: String, title: String }` to `ui_state.rs`. The `title` comes from `item.description`.

Two-step dispatch pattern (same as `DeleteConfirm`):

1. `resolve_and_push()` in `key_handlers.rs` intercepts `Intent::CloseChangeRequest` — instead of pushing a command, it transitions to `UiMode::CloseConfirm` with the PR id and title.
2. `handle_close_confirm_key()` handles user input:
   - `y` or `Enter` → dispatches `Command::CloseChangeRequest { id }`, returns to `Normal` mode
   - `n` or `Esc` → returns to `Normal` mode
3. `handle_key` gets a match arm for `UiMode::CloseConfirm`. `handle_mouse` blocks mouse events in this mode (same as `DeleteConfirm`).

No async loading needed — all info available at intent resolution time.

### 3. Command & Protocol

Add `Command::CloseChangeRequest { id: String }` to `commands.rs`.

- Description: `"Closing PR..."`
- Update existing exhaustive tests: `command_roundtrip_covers_all_variants`, `command_description_covers_all_variants`

### 4. Provider Trait

Add to `CodeReview` trait in `code_review/mod.rs`:

```rust
async fn close_change_request(&self, repo_root: &Path, id: &str) -> Result<(), String>;
```

GitHub implementation in `code_review/github.rs` runs `gh pr close <id>`.

### 5. Executor

Handle `Command::CloseChangeRequest` in `executor.rs`:

- Call `cr.close_change_request(repo_root, &id).await`
- Return `CommandResult::Ok`
- No immediate refresh — relies on periodic refresh cycle

### 6. Tests

- `is_available`: available when `change_request_key` is present, unavailable when absent
- `resolve`: returns `Some(Command::CloseChangeRequest)` for open PRs, `None` for merged/closed
- Confirmation flow: intent opens `CloseConfirm` mode, `y` dispatches command, `Esc` cancels
- Executor: with and without a `CodeReview` provider (following existing `open_change_request` test pattern)
- Command serde roundtrip and description coverage (update existing exhaustive tests)
- `is_config` test in `ui_state.rs` updated for new variant

## Files Changed

| File | Change |
|------|--------|
| `crates/flotilla-tui/src/app/intent.rs` | Add variant, availability, resolve, label, menu order |
| `crates/flotilla-tui/src/app/ui_state.rs` | Add `CloseConfirm` mode, update `is_config` test |
| `crates/flotilla-tui/src/app/key_handlers.rs` | Add `handle_close_confirm_key`, wire intent to confirm mode, handle_key/handle_mouse arms |
| `crates/flotilla-tui/src/ui.rs` | Render confirmation dialog |
| `crates/flotilla-protocol/src/commands.rs` | Add command variant, update exhaustive tests |
| `crates/flotilla-core/src/providers/code_review/mod.rs` | Add trait method |
| `crates/flotilla-core/src/providers/code_review/github.rs` | Implement with `gh pr close` |
| `crates/flotilla-core/src/executor.rs` | Handle new command, add tests |
