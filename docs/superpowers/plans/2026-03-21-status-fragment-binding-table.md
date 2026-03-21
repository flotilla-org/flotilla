# Status Fragment and Binding Table Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace hardcoded status bar content routing and duplicated key chip definitions with a data-driven binding table and widget-provided status fragments.

**Architecture:** A flat binding table defines all key bindings with optional hint annotations. At startup it compiles into lookup maps for key resolution and status bar chips. Widgets provide `StatusFragment` (status text) and `KeyBindingMode` (which binding set to use). Screen resolves these and passes pre-computed content to a pure-renderer status bar.

**Tech Stack:** Rust, ratatui, crossterm

**Spec:** `docs/superpowers/specs/2026-03-21-status-fragment-binding-table-design.md`

---

## File Structure

### New files
| File | Responsibility |
|------|---------------|
| `crates/flotilla-tui/src/binding_table.rs` | `Binding`, `BindingModeId`, `KeyBindingMode`, `StatusFragment`, `StatusContent`, compiled table types, hint resolution |

### Files to heavily modify
| File | Changes |
|------|---------|
| `crates/flotilla-tui/src/keymap.rs` | Replace `ModeId` with `BindingModeId`, replace procedural `defaults()` with binding table, keep `Keymap` struct but build from table |
| `crates/flotilla-tui/src/widgets/mod.rs` | Replace `WidgetStatusData` with `StatusFragment`, replace `mode_id()` with `binding_mode()`, replace `status_data()` with `status_fragment()` on trait |
| `crates/flotilla-tui/src/widgets/status_bar_widget.rs` | Delete `status_bar_content()`, simplify `render_bespoke()` to accept pre-resolved content |
| `crates/flotilla-tui/src/widgets/screen.rs` | Add resolution logic: walk stack for fragments, look up chips from compiled table, pass to status bar |
| `crates/flotilla-tui/src/widgets/issue_search.rs` | Remove `sync_mode()`, use `AppAction` for search query, provide `status_fragment()` |
| `crates/flotilla-tui/src/widgets/command_palette.rs` | Use `AppAction` for search query instead of `ctx.repo_ui` |
| `crates/flotilla-tui/src/widgets/repo_page.rs` | Implement `status_fragment()` and `binding_mode()` |

### Files with moderate changes
| File | Changes |
|------|---------|
| `crates/flotilla-tui/src/widgets/overview_page.rs` | Implement `binding_mode()` and `status_fragment()` |
| `crates/flotilla-tui/src/widgets/branch_input.rs` | Replace `status_data()` with `status_fragment()` |
| `crates/flotilla-tui/src/widgets/action_menu.rs` | Replace `mode_id()` with `binding_mode()` |
| `crates/flotilla-tui/src/widgets/delete_confirm.rs` | Replace `mode_id()` with `binding_mode()` |
| `crates/flotilla-tui/src/widgets/close_confirm.rs` | Replace `mode_id()` with `binding_mode()` |
| `crates/flotilla-tui/src/widgets/help.rs` | Replace `mode_id()` with `binding_mode()` |
| `crates/flotilla-tui/src/widgets/file_picker.rs` | Replace `mode_id()` with `binding_mode()` |
| `crates/flotilla-tui/src/app/mod.rs` | Add `AppAction::SetSearchQuery`/`ClearSearchQuery` handlers |
| `crates/flotilla-tui/src/app/key_handlers.rs` | Update `resolve_action()` to use `BindingModeId`, simplify sync bridge |
| `crates/flotilla-tui/src/status_bar.rs` | No structural changes — types (KeyChip, StatusSection, etc.) stay |

---

## Task 1: Create binding table and BindingModeId

**Files:**
- Create: `crates/flotilla-tui/src/binding_table.rs`
- Modify: `crates/flotilla-tui/src/lib.rs` (add `pub mod binding_table;`)

Standalone types and the flat binding table. No integration yet.

- [ ] **Step 1: Define core types**

```rust
// binding_table.rs

/// Flat enum for hashable binding mode identifiers.
/// Used as keys in compiled lookup tables.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BindingModeId {
    Shared,
    Normal,
    Overview,
    Help,
    ActionMenu,
    DeleteConfirm,
    CloseConfirm,
    BranchInput,
    IssueSearch,
    CommandPalette,
    FilePicker,
    SearchActive,
}

/// What widgets return from `binding_mode()`.
/// Single variant for the common case, Composed for layered modes.
#[derive(Debug, Clone)]
pub enum KeyBindingMode {
    Single(BindingModeId),
    Composed(Vec<BindingModeId>),
}

impl From<BindingModeId> for KeyBindingMode {
    fn from(id: BindingModeId) -> Self {
        KeyBindingMode::Single(id)
    }
}

/// Widget-provided status content for the status bar.
#[derive(Debug, Clone, Default)]
pub struct StatusFragment {
    pub status: Option<StatusContent>,
}

#[derive(Debug, Clone)]
pub enum StatusContent {
    Label(String),
    ActiveInput { prefix: String, text: String },
    Progress(String),
}

/// A single entry in the binding table.
pub struct Binding {
    pub mode: BindingModeId,
    pub key: &'static str,
    pub action: Action,
    pub hint: Option<&'static str>,
}

pub const fn b(mode: BindingModeId, key: &'static str, action: Action, hint: Option<&'static str>) -> Binding {
    Binding { mode, key, action, hint }
}

pub const fn h(label: &'static str) -> Option<&'static str> { Some(label) }
```

- [ ] **Step 2: Define the binding table**

Port all bindings from `Keymap::defaults()` (keymap.rs:278-358) into the flat table format. Include hint annotations for bindings that should appear as status bar key chips. Refer to the current `status_bar_content()` function (status_bar_widget.rs:302-462) to identify which bindings get hints.

**Important:** `ModeId::Config` maps to `BindingModeId::Overview`. Port the Config mode bindings (keymap.rs:319-324: `q` → Dismiss, `[` → PrevTab, `]` → NextTab) under `BindingModeId::Overview`.

The `SearchActive` mode's `esc` binding maps to `Action::Dismiss` (not a new `ClearSearch` action). The "Clear" hint is just a display label — the actual behavior (clearing the search query) is handled by RepoPage's dismiss cascade. No new Action variant needed.

- [ ] **Step 3: Add compiled table types and build function**

```rust
pub struct CompiledBindings {
    pub key_map: HashMap<BindingModeId, HashMap<KeyCombination, Action>>,
    pub hints: HashMap<BindingModeId, Vec<KeyChip>>,
}

impl CompiledBindings {
    pub fn from_table(bindings: &[Binding]) -> Self { ... }

    /// Resolve key chips for a KeyBindingMode.
    /// For Single: return that mode's hints.
    /// For Composed: merge hints from each mode, later modes override by key.
    /// Shared hints are always included at the bottom.
    pub fn hints_for(&self, mode: &KeyBindingMode) -> Vec<KeyChip> { ... }

    /// Resolve a key press for a KeyBindingMode.
    /// For Single: check mode then Shared.
    /// For Composed: check modes in reverse order (later wins), then Shared.
    pub fn resolve(&self, mode: &KeyBindingMode, key: KeyCombination) -> Option<Action> { ... }
}
```

- [ ] **Step 4: Write tests**

```rust
#[test]
fn compiled_bindings_resolve_single_mode() { ... }

#[test]
fn compiled_bindings_resolve_composed_mode_later_wins() { ... }

#[test]
fn compiled_bindings_shared_fallback() { ... }

#[test]
fn hints_for_single_mode() { ... }

#[test]
fn hints_for_composed_mode_overrides_by_key() { ... }

#[test]
fn from_table_parses_all_keys() { ... }
```

- [ ] **Step 5: Run CI and commit**

```bash
cargo test --workspace --locked && cargo clippy --workspace --all-targets --locked -- -D warnings && cargo +nightly-2026-03-12 fmt --check
git commit -am "feat: add binding table with BindingModeId, KeyBindingMode, StatusFragment"
```

---

## Task 2: Integrate binding table into Keymap

**Files:**
- Modify: `crates/flotilla-tui/src/keymap.rs`
- Modify: `crates/flotilla-tui/src/binding_table.rs` (if needed)

Replace `Keymap::defaults()` procedural construction with building from the binding table. Replace `ModeId` with `BindingModeId` throughout.

- [ ] **Step 1: Replace ModeId with BindingModeId**

Change `Keymap` to use `BindingModeId`:

```rust
pub struct Keymap {
    compiled: CompiledBindings,
}

impl Keymap {
    pub fn resolve(&self, mode: &KeyBindingMode, key: KeyCombination) -> Option<Action> {
        self.compiled.resolve(mode, key)
    }

    pub fn hints_for(&self, mode: &KeyBindingMode) -> Vec<KeyChip> {
        self.compiled.hints_for(mode)
    }

    pub fn defaults() -> Self {
        Self { compiled: CompiledBindings::from_table(&BINDINGS) }
    }

    pub fn from_config(config: &KeysConfig) -> Self {
        let mut keymap = Self::defaults();
        // Apply user overrides on top of compiled table
        // ...
        keymap
    }

    pub fn help_sections(&self) -> Vec<HelpSection> {
        // Derive from compiled bindings instead of hardcoding
        // ...
    }
}
```

Remove the old `ModeId` enum from keymap.rs. Update all imports to use `BindingModeId` from `binding_table`.

- [ ] **Step 2: Update all ModeId references across the codebase**

Search for `ModeId` and replace with `BindingModeId`. Key locations:
- `widgets/mod.rs` — `RenderContext.active_widget_mode`, `InteractiveWidget::mode_id()`
- `widgets/screen.rs` — `active_mode_id()`
- `app/key_handlers.rs` — `resolve_action()`, mode matching in `handle_key()`
- Every widget's `mode_id()` implementation

**Don't change the trait method name yet** — that's Task 4. Just change the return type.

- [ ] **Step 3: Update from_config() to apply overrides to CompiledBindings**

User config overrides (`KeysConfig`) should modify the compiled table's key_map. The hint annotations from the base table are preserved unless the override changes the action for a hinted key.

Map config section names to `BindingModeId`: `normal` → `Normal`, `help` → `Help`, `config` → `Overview`, `action_menu` → `ActionMenu`, `delete_confirm` → `DeleteConfirm`, `close_confirm` → `CloseConfirm`. Other modes (IssueSearch, CommandPalette, FilePicker, BranchInput, SearchActive) are not user-configurable for now.

- [ ] **Step 3a: Keep help_sections() working**

`help_sections()` currently uses a curated list with specific section ordering. Keep it as a curated list for now, but have it read key display strings from `CompiledBindings` instead of hardcoding them. This preserves section ordering and grouping while picking up user config overrides. A fully data-driven help screen can be a follow-up.

- [ ] **Step 4: Run CI and commit**

```bash
cargo test --workspace --locked && cargo clippy --workspace --all-targets --locked -- -D warnings
git commit -am "refactor: replace ModeId with BindingModeId, build Keymap from binding table"
```

---

## Task 3: Add StatusFragment to InteractiveWidget trait

**Files:**
- Modify: `crates/flotilla-tui/src/widgets/mod.rs`
- Modify: all widget files that implement `InteractiveWidget`

- [ ] **Step 1: Replace trait methods**

In `InteractiveWidget`:
- Rename `mode_id()` → `binding_mode()`, change return type to `KeyBindingMode`
- Replace `status_data()` → `status_fragment()`, change return type to `StatusFragment`
- Remove `WidgetStatusData` enum

- [ ] **Step 2: Update all widget implementations**

For each widget, replace `mode_id()` with `binding_mode()` returning the appropriate `BindingModeId.into()`:

| Widget | `binding_mode()` return | `status_fragment()` |
|--------|------------------------|---------------------|
| RepoPage | `Normal.into()` (composed case in Task 5) | (Task 5) |
| OverviewPage | `Overview.into()` | `Label("FLOTILLA")` |
| HelpWidget | `Help.into()` | `Label("HELP")` |
| ActionMenuWidget | `ActionMenu.into()` | `Label("ACTIONS")` |
| DeleteConfirmWidget | `DeleteConfirm.into()` | `Label("CONFIRM DELETE")` |
| CloseConfirmWidget | `CloseConfirm.into()` | `Label("CONFIRM CLOSE")` |
| BranchInputWidget | `BranchInput.into()` | `Progress("Generating...")` when generating, `ActiveInput { prefix: "NEW BRANCH", text }` when manual |
| IssueSearchWidget | `IssueSearch.into()` | `ActiveInput { prefix: "SEARCH", text }` |
| CommandPaletteWidget | `CommandPalette.into()` | `ActiveInput { prefix: "/", text }` |
| FilePickerWidget | `FilePicker.into()` | `Label("ADD REPO")` |
| WorkItemTable | `Normal.into()` | default |
| PreviewPanel | `Normal.into()` | default |
| EventLogWidget | `Overview.into()` | default |

Every modal widget MUST provide a status fragment — returning default causes a regression where the status bar shows "/ for commands" instead of the mode-specific label. Only sub-widgets embedded inside pages (WorkItemTable, PreviewPanel, EventLogWidget) should return default.

- [ ] **Step 3: Update Screen's helper methods**

`active_mode_id()` → `active_binding_mode()` returning `KeyBindingMode`.
`active_status_data()` → `active_status_fragment()` returning `StatusFragment`.

`active_binding_mode()` must walk: top modal → active page (repo or overview). It must NOT hardcode `Normal` as the fallback — when the Flotilla tab is active, the page's binding mode is `Overview`, not `Normal`. Query the active page widget's `binding_mode()`.

Similarly, `active_status_fragment()` must walk: top modal → active page, returning the first `Some(status)` it finds.

Update `RenderContext` to use the new types.

- [ ] **Step 4: Update key_handlers.rs**

`resolve_action()` uses `binding_mode()` instead of `mode_id()`. Update the mode matching for hybrid widgets (CommandPalette, FilePicker hardcoded keys). The hybrid matching currently checks `ModeId::CommandPalette` / `ModeId::FilePicker` — change to `BindingModeId::CommandPalette` / `BindingModeId::FilePicker` (extract from `KeyBindingMode::Single`).

Also update all test assertions that call `.mode_id()` or compare against `ModeId::*` — there are ~25 of these in `key_handlers.rs`. Change to `.binding_mode()` and `BindingModeId::*`.

- [ ] **Step 5: Run CI and commit**

```bash
cargo test --workspace --locked && cargo clippy --workspace --all-targets --locked -- -D warnings
git commit -am "refactor: replace mode_id/status_data with binding_mode/status_fragment on InteractiveWidget"
```

---

## Task 4: Simplify status bar to pure renderer

**Files:**
- Modify: `crates/flotilla-tui/src/widgets/status_bar_widget.rs`
- Modify: `crates/flotilla-tui/src/widgets/screen.rs`

- [ ] **Step 1: Add resolution logic to Screen**

In `Screen::render()`, before calling the status bar:

```rust
// 1. Resolve status fragment — walk stack top-down
let status = self.active_status_fragment()
    .status
    .unwrap_or_else(|| StatusContent::Label("/ for commands".into()));

// 2. Resolve key chips from binding mode
let mode = self.active_binding_mode();
let key_chips = keymap.hints_for(&mode);  // keymap from RenderContext or passed in

// 3. Task spinner (unchanged)
let task = active_task(ctx.model, ctx.in_flight);

// 4. Error items (unchanged)
let error_items = collect_visible_status_items(ctx.model, ctx.ui);

// 5. Mode indicators (unchanged)
let mode_indicators = normal_mode_indicators(ctx.ui);

// 6. show_keys flag
let show_keys = ctx.ui.status_bar.show_keys;
```

Pass all resolved values to `status_bar.render_bespoke()`.

The cascade for `status_fragment()` should walk: top modal → ... → base page. Implement as a method on Screen that iterates `modal_stack` then falls back to the active page.

- [ ] **Step 2: Simplify render_bespoke()**

Change the signature to accept pre-resolved content (per spec). Delete `status_bar_content()` — all its logic is now in Screen's resolution or derived from the binding table.

Move `active_task()`, `collect_visible_status_items()`, and `normal_mode_indicators()` to Screen or keep as free functions — they're app-level, not status-bar-level.

- [ ] **Step 3: Remove WidgetStatusData from RenderContext**

`RenderContext` no longer needs `active_widget_mode` or `active_widget_data` — Screen resolves everything before calling the status bar.

- [ ] **Step 4: Run CI and commit**

```bash
cargo test --workspace --locked && cargo clippy --workspace --all-targets --locked -- -D warnings
git commit -am "refactor: simplify status bar to pure renderer, resolve content in Screen"
```

---

## Task 5: Widget status fragments and composed bindings

**Files:**
- Modify: `crates/flotilla-tui/src/widgets/repo_page.rs`
- Modify: `crates/flotilla-tui/src/widgets/overview_page.rs`
- Modify: `crates/flotilla-tui/src/widgets/issue_search.rs`
- Modify: `crates/flotilla-tui/src/widgets/command_palette.rs`

- [ ] **Step 1: RepoPage status_fragment and binding_mode**

```rust
fn status_fragment(&self) -> StatusFragment {
    let status = if self.show_providers {
        Some(StatusContent::Label("PROVIDERS".into()))
    } else if let Some(query) = &self.active_search_query {
        Some(StatusContent::Label(format!("SEARCH \"{query}\"")))
    } else if !self.multi_selected.is_empty() {
        Some(StatusContent::Label(format!("{} SELECTED", self.multi_selected.len())))
    } else {
        None // default "/ for commands"
    };
    StatusFragment { status }
}

fn binding_mode(&self) -> KeyBindingMode {
    if self.active_search_query.is_some() {
        KeyBindingMode::Composed(vec![BindingModeId::Normal, BindingModeId::SearchActive])
    } else {
        BindingModeId::Normal.into()
    }
}
```

- [ ] **Step 2: OverviewPage status_fragment**

```rust
fn status_fragment(&self) -> StatusFragment {
    StatusFragment { status: Some(StatusContent::Label("FLOTILLA".into())) }
}
```

- [ ] **Step 3: IssueSearchWidget status_fragment**

```rust
fn status_fragment(&self) -> StatusFragment {
    StatusFragment {
        status: Some(StatusContent::ActiveInput {
            prefix: "SEARCH".into(),
            text: self.input.value().to_string(),
        }),
    }
}
```

Remove `sync_mode()` — the status bar now reads from `status_fragment()` instead of `UiMode::IssueSearch`.

- [ ] **Step 4: CommandPaletteWidget status_fragment**

```rust
fn status_fragment(&self) -> StatusFragment {
    StatusFragment {
        status: Some(StatusContent::ActiveInput {
            prefix: "/".into(),
            text: self.input.value().to_string(),
        }),
    }
}
```

- [ ] **Step 5: Write tests**

Test that each widget returns the correct fragment for its state:
- RepoPage with multi-select returns "N SELECTED"
- RepoPage with search returns "SEARCH \"query\""
- RepoPage with search active returns composed binding mode
- IssueSearchWidget returns ActiveInput
- OverviewPage returns "FLOTILLA"

- [ ] **Step 6: Run CI and commit**

```bash
cargo test --workspace --locked && cargo clippy --workspace --all-targets --locked -- -D warnings
git commit -am "feat: implement status_fragment and binding_mode on all widgets"
```

---

## Task 6: Migrate search query writes to AppAction

**Files:**
- Modify: `crates/flotilla-tui/src/widgets/mod.rs` (add AppAction variants)
- Modify: `crates/flotilla-tui/src/widgets/issue_search.rs`
- Modify: `crates/flotilla-tui/src/widgets/command_palette.rs`
- Modify: `crates/flotilla-tui/src/widgets/repo_page.rs`
- Modify: `crates/flotilla-tui/src/app/mod.rs` (handle new AppActions)
- Modify: `crates/flotilla-tui/src/app/key_handlers.rs` (simplify sync bridge)

- [ ] **Step 1: Add AppAction variants**

```rust
AppAction::SetSearchQuery { repo: RepoIdentity, query: String },
AppAction::ClearSearchQuery { repo: RepoIdentity },
```

- [ ] **Step 2: Update IssueSearchWidget**

On Confirm: emit `AppAction::SetSearchQuery` instead of writing `ctx.repo_ui`. On Dismiss: emit `AppAction::ClearSearchQuery`.

Remove `sync_mode()` and the `*ctx.mode = UiMode::IssueSearch { ... }` call. `UiMode::IssueSearch` becomes dead code.

- [ ] **Step 3: Update CommandPaletteWidget**

Same pattern — search-related writes go through AppAction instead of `ctx.repo_ui`.

- [ ] **Step 4: Update RepoPage dismiss**

RepoPage's dismiss cascade clears `active_search_query` and writes to `ctx.repo_ui`. Change it to emit `AppAction::ClearSearchQuery` instead of writing `ctx.repo_ui` directly.

- [ ] **Step 5: Handle new AppActions in process_app_actions()**

```rust
AppAction::SetSearchQuery { repo, query } => {
    if let Some(page) = self.screen.repo_pages.get_mut(&repo) {
        page.active_search_query = Some(query);
    }
}
AppAction::ClearSearchQuery { repo } => {
    if let Some(page) = self.screen.repo_pages.get_mut(&repo) {
        page.active_search_query = None;
    }
}
```

- [ ] **Step 6: Simplify sync bridge**

`sync_ui_state_to_repo_page()` no longer needs to sync `active_search_query` — it's now written directly to RepoPage via AppAction. If that was the only thing it synced, remove the function entirely.

`sync_repo_page_state()` synced selection/multi-select/show_providers back to RepoUiState for the status bar. The status bar now reads from `status_fragment()`. Check if any other code still reads these RepoUiState fields. If only tests remain, update the tests and remove the sync.

- [ ] **Step 7: Run CI and commit**

```bash
cargo test --workspace --locked && cargo clippy --workspace --all-targets --locked -- -D warnings
git commit -am "refactor: migrate search query writes to AppAction, simplify sync bridge"
```

---

## Task 7: Clean up dead code

**Files:**
- Modify: `crates/flotilla-tui/src/app/ui_state.rs`
- Modify: `crates/flotilla-tui/src/widgets/mod.rs`
- Modify: various files with stale imports

- [ ] **Step 1: Remove UiMode::IssueSearch**

Check if any code still references `UiMode::IssueSearch`. If not, remove the variant. If `UiMode` only has `Normal` and `Config` left, check if `Config` can also be removed (OverviewPage routing may still use `is_config()`).

- [ ] **Step 2: Remove WidgetStatusData if still present**

Should have been replaced by `StatusFragment` in Task 3. Verify no references remain.

- [ ] **Step 3: Remove stale imports and dead functions**

Search for unused imports of `ModeId`, `WidgetStatusData`, `status_bar_content`. Clean up.

- [ ] **Step 4: Run CI and commit**

```bash
cargo test --workspace --locked && cargo clippy --workspace --all-targets --locked -- -D warnings
git commit -am "chore: remove UiMode::IssueSearch, WidgetStatusData, and stale imports"
```

---

## Notes for implementers

- **Read the spec** at `docs/superpowers/specs/2026-03-21-status-fragment-binding-table-design.md` before starting.
- **Run CI after every task**: `cargo +nightly-2026-03-12 fmt --check && cargo clippy --workspace --all-targets --locked -- -D warnings && cargo test --workspace --locked`
- **Task dependencies**: Task 1 is independent. Task 2 depends on 1. Task 3 depends on 2. Task 4 depends on 3. Tasks 5 and 6 depend on Task 3 (not Task 4) and can be done in either order or in parallel with Task 4. Task 7 depends on 4+5+6.
- **The hardest task is 4** — simplifying the status bar and moving resolution into Screen. The current `status_bar_content()` is ~160 lines of mode matching that all needs to be replaced with the binding table lookup + fragment cascade.
- **The binding table key strings** ("j", "esc", "S-K", etc.) need to parse into `KeyCombination`. Use the existing `crokey::key!` macro or write a parser for the string format. Check how `from_config()` currently parses key strings.
- **`captures_raw_keys()`** stays unchanged — it's orthogonal to binding modes.
- **Snapshot tests** may need updating if status bar rendering changes. Investigate, don't blindly accept.
