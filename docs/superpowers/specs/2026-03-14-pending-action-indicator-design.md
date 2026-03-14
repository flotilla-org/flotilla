# Pending Action Indicator on Work Item Rows

Addresses [#150](https://github.com/rjwittams/flotilla/issues/150).

After triggering an action from the action menu, there is no visual feedback on the affected row. The user waits for the next refresh with no indication that anything happened. This design adds per-row pending indicators: a spinner icon with a shimmer effect while in flight, and a red error marker on failure.

## Key Decisions

- **All actions** from the action menu get indicators, not just destructive ones. Consistent behavior, no classification needed.
- **TUI-side tracking** in `RepoUiState` via `pending_actions: HashMap<WorkItemIdentity, PendingAction>`. No protocol changes.
- **Clear on `CommandFinished`** — success removes the entry, error transitions it to `Failed`. No snapshot diffing.
- **Shimmer reuse** — generalize the existing `shimmer_spans` into a `Shimmer` struct that supports multi-segment rendering (row = multiple cells at column offsets). The existing single-text API becomes a thin wrapper.

## Data Model

### New types in `ui_state.rs`

```rust
#[derive(Clone, Debug)]
pub enum PendingStatus {
    InFlight,
    Failed(String),
}

#[derive(Clone, Debug)]
pub struct PendingAction {
    pub command_id: u64,
    pub status: PendingStatus,
    pub description: String,
}
```

### Storage

`RepoUiState` gains a new field:

```rust
pub pending_actions: HashMap<WorkItemIdentity, PendingAction>,
```

### Lifecycle

1. **Insert** — after `daemon.execute(cmd)` returns a command ID in `executor::dispatch()`, insert an entry keyed by the target work item's identity.
2. **Success** — on `CommandFinished` with a non-error result, find the entry by `command_id` and remove it.
3. **Error** — on `CommandFinished` with `Error { message }`, find the entry by `command_id` and set `status = Failed(message)`.
4. **Replaced** — if the user triggers another action on the same identity, the new entry overwrites the old one. An orphaned `CommandFinished` for the overwritten command is harmlessly ignored (no matching `command_id` found).
5. **Stale cleanup** — when `update_table_view()` rebuilds the table, retain only entries whose identity still exists. Same pattern as `multi_selected`.

## Shimmer Refactor

### Current state

`shimmer_spans(text: &str) -> Vec<Span>` computes a per-character color sweep across a single text string. It handles true-color and 256-color fallback. Used by the status bar.

### New struct

Replace the standalone function with a `Shimmer` struct in `shimmer.rs`:

```rust
pub struct Shimmer {
    pos: f32,
    band_half_width: f32,
    true_color: bool,
    padding: usize,
}

impl Shimmer {
    pub fn new(total_width: usize) -> Self {
        Self::new_at(total_width, elapsed_since_start())
    }

    /// Testable constructor — accepts an explicit elapsed duration.
    pub fn new_at(total_width: usize, elapsed: Duration) -> Self {
        let padding = 10;
        let period = total_width + padding * 2;
        let sweep_seconds = 2.0f32;
        let pos = (elapsed.as_secs_f32() % sweep_seconds)
            / sweep_seconds * period as f32;
        Self {
            pos,
            band_half_width: 5.0,
            true_color: has_true_color(),
            padding,
        }
    }

    /// Render a segment of the shimmer at `offset` characters from the row start.
    pub fn spans(&self, text: &str, offset: usize) -> Vec<Span<'static>> {
        // Same per-character loop as shimmer_spans, but the distance
        // calculation uses ((offset + i) as f32 + padding as f32) - pos
        // instead of (i as f32 + padding as f32) - pos, so the band
        // position is relative to the full row width.
    }
}

/// Convenience wrapper — single-segment shimmer (status bar, etc.).
pub fn shimmer_spans(text: &str) -> Vec<Span<'static>> {
    Shimmer::new(text.chars().count()).spans(text, 0)
}
```

The internal helpers (`blend`, `elapsed_since_start`, `has_true_color`) remain private. Existing callers of `shimmer_spans` are unchanged.

## Dispatch Integration

### Carrying identity through the command queue

Commands flow through two phases: `resolve_and_push()` synchronously pushes a `Command` onto `CommandQueue`, then the event loop drains the queue and calls `executor::dispatch()`. The work item identity and intent label are available at push time but the command ID is not assigned until `dispatch()` calls `daemon.execute()`.

Extend `CommandQueue` to store an optional `PendingActionContext` alongside each command:

```rust
pub struct PendingActionContext {
    pub identity: WorkItemIdentity,
    pub description: String,  // from Intent::label()
    pub repo_path: PathBuf,   // captured at push time, not drain time
}

pub struct CommandQueue {
    queue: VecDeque<(Command, Option<PendingActionContext>)>,
}
```

In `resolve_and_push()`, attach the context when pushing:

```rust
fn resolve_and_push(&mut self, intent: Intent, item: &WorkItem) {
    // ... existing host/availability checks ...
    if let Some(cmd) = intent.resolve(item, self) {
        let pending_ctx = PendingActionContext {
            identity: item.identity.clone(),
            description: intent.label(self.model.active_labels()),
        };
        // ... existing per-intent mode changes ...
        self.proto_commands.push_with_context(cmd, Some(pending_ctx));
    }
}
```

In `executor::dispatch()`, accept the optional context and insert on success:

```rust
pub async fn dispatch(
    cmd: Command,
    app: &mut App,
    pending_ctx: Option<PendingActionContext>,
) {
    // ... existing logic ...
    match app.daemon.execute(cmd).await {
        Ok(command_id) => {
            if let Some(ctx) = pending_ctx {
                let repo_path = app.model.active_repo_root().clone();
                if let Some(rui) = app.ui.repo_ui.get_mut(&repo_path) {
                    rui.pending_actions.insert(ctx.identity, PendingAction {
                        command_id,
                        status: PendingStatus::InFlight,
                        description: ctx.description,
                    });
                }
            }
        }
        Err(e) => { /* existing error handling */ }
    }
}
```

The event loop drain site in `run.rs` changes from `take_next() -> Option<Command>` to `take_next() -> Option<(Command, Option<PendingActionContext>)>`, passing both to `dispatch()`.

Background issue commands (viewport, search) pushed directly via `proto_commands.push()` continue to pass `None` for the context.

### Two-phase dispatch intents

Some intents have a two-phase dispatch: `resolve_and_push()` sends a preliminary command, and the real action is dispatched later from a confirm handler.

- **`RemoveCheckout`** pushes `FetchCheckoutStatus` first, then the actual removal comes from the delete-confirm handler after the user confirms. The pending indicator appears during the status fetch phase, which is reasonable — it shows the action is being processed.
- **`CloseChangeRequest`** returns before pushing (the confirm handler dispatches the command). Its confirm handler should attach a `PendingActionContext` when it pushes the real command.

Intents like `GenerateBranchName` also set modal UI modes (`BranchInput`). The shimmer on the underlying row may be briefly visible behind these dialogs. This is expected and harmless.

### Handling CommandFinished

In `handle_daemon_event()`, the `CommandFinished` arm already has the `command_id`. After calling `executor::handle_result()`, scan `pending_actions` across all repo UIs to find the entry matching the finished command's ID:

- Non-error results: remove the entry.
- `CommandResult::Error { message }`: transition to `PendingStatus::Failed(message)`.
- `CommandResult::Cancelled`: remove the entry.

The scan happens after `handle_result()` so that error status messages are set before the pending state transitions.

## Rendering

### build_item_row changes

`build_item_row()` gains a `pending: Option<&PendingAction>` parameter.

**InFlight:**
- Create `Shimmer::new(total_row_width)` for the row, where `total_row_width` is the sum of the `col_widths: &[u16]` slice already passed to `build_item_row()`.
- Replace the normal icon with a braille spinner character. Frame selected from elapsed time cycling through `"⠋⠙⠹⠸⠼⠴⠦⠧"`.
- Apply `shimmer.spans(cell_text, column_offset)` to each cell instead of `Span::styled()`.

**Failed:**
- Replace icon with `✗` in `Color::Red`.
- Apply dim red styling to the row — no shimmer.

**The caller** in `render_table` looks up `rui.pending_actions.get(&item.identity)` and passes it through.

### Animation

`needs_animation()` already returns true when `in_flight` is non-empty. Extend it to also return true when any `RepoUiState` has a pending action with `InFlight` status.

## Cleanup

`RepoUiState::update_table_view()` already retains `multi_selected` entries that exist in the new table view. Apply the same pattern to `pending_actions`:

```rust
self.pending_actions.retain(|id, _| current_identities.contains(id));
```

This handles the edge case where a successful action causes an item to vanish from the snapshot before `CommandFinished` arrives.
