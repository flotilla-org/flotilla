# Status Fragment and Binding Table

## Problem

The status bar hardcodes knowledge about page-level state. It reaches into `RepoUiState` and runs an if/else chain to decide what to show ("PROVIDERS", "3 SELECTED", "SEARCH \"query\""). Key chips are hardcoded per `ModeId` in `status_bar_content()`, duplicating what the keymap already knows. A sync bridge copies state between `RepoPage` and `RepoUiState` every dispatch cycle to feed the status bar.

This design replaces three sources of duplication with two focused mechanisms: a data-driven binding table and a widget-provided status fragment.

## Design

### Binding Table

The keymap is defined as a flat table of `(mode, key, action, hint)` entries:

```rust
const BINDINGS: &[Binding] = &[
    //  mode             key      action               hint
    b( Shared,           "j",     SelectNext,          None       ),
    b( Shared,           "k",     SelectPrev,          None       ),
    b( Shared,           "enter", Confirm,             None       ),
    b( Shared,           "esc",   Dismiss,             None       ),
    b( Shared,           "?",     ToggleHelp,          None       ),

    b( Normal,           "q",     Quit,                h("Quit")  ),
    b( Normal,           ".",     OpenActionMenu,      h("Menu")  ),
    b( Normal,           "n",     OpenBranchInput,     h("New")   ),
    b( Normal,           "?",     ToggleHelp,          h("Help")  ),
    b( Normal,           "r",     Refresh,             None       ),
    // ...

    b( DeleteConfirm,    "y",     Confirm,             h("Yes")   ),
    b( DeleteConfirm,    "n",     Dismiss,             h("No")    ),
    b( DeleteConfirm,    "q",     Dismiss,             None       ),

    b( SearchActive,     "esc",   ClearSearch,         h("Clear") ),
    // ...
];
```

At startup, this table compiles into efficient lookup structures:
- `HashMap<BindingModeId, HashMap<KeyCombination, Action>>` — for key resolution
- `HashMap<BindingModeId, Vec<KeyChip>>` — for status bar hints
- Help sections for the help screen

Only flat `BindingModeId` variants appear as keys in these maps. `Composed` modes are resolved at query time by looking up each constituent mode and merging results (see below).

User config overrides apply on top of the compiled table, same as today.

The binding table is the single source of truth for keymap resolution, status bar key chips, and help display.

### KeyBindingMode

Replaces `ModeId`. Two types work together:

`BindingModeId` is a flat, hashable enum used as keys in the compiled lookup tables:

```rust
#[derive(Clone, Copy, Hash, Eq, PartialEq)]
enum BindingModeId {
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
```

`KeyBindingMode` is what widgets return from `binding_mode()`. It is either a single `BindingModeId` or a composed list:

```rust
enum KeyBindingMode {
    Single(BindingModeId),
    Composed(Vec<BindingModeId>),
}
```

With a `From<BindingModeId>` impl so the common case is zero-ceremony — just return a single variant and `.into()` handles it.

**Resolution for single modes:** Look up the mode's bindings, layer on top of `Shared`.

**Resolution for composed modes:** Flatten the list, look up each `BindingModeId` in the compiled table, merge with later-wins for key conflicts. `Shared` is always implicitly at the bottom. For key chips, later modes override earlier modes for the same key — so `SearchActive`'s `ESC → Clear` replaces `Shared`'s `ESC → Dismiss` in the chip list. Chips for keys not overridden are kept.

Common case: `KeyBindingMode::from(BindingModeId::Normal)`. Composed case: `KeyBindingMode::Composed(vec![BindingModeId::Normal, BindingModeId::SearchActive])`.

### StatusFragment

Widgets declare what they want shown in the status bar's left-side status text:

```rust
struct StatusFragment {
    status: Option<StatusContent>,
}

enum StatusContent {
    /// Static label — "PROVIDERS", "3 SELECTED", "FLOTILLA"
    Label(String),
    /// Text input being actively edited — renders with cursor
    ActiveInput { prefix: String, text: String },
    /// Progress indicator — shimmer animation
    Progress(String),
}
```

Key chips are NOT part of the fragment — they are derived from the widget's `binding_mode()` via the compiled binding table.

**Cascade:** Screen walks the widget stack top-down (modals first, then the active page). For `status`, it takes the first `Some` it finds. If nothing provides one, the default is `Label("/ for commands")`.

`RepoPage` can answer `status_fragment()` from its own fields — it already owns `multi_selected`, `show_providers`, and `active_search_query`. No access to `RepoUiState` or shared state is needed.

### InteractiveWidget Trait Changes

`mode_id()` is replaced by `binding_mode()`. `status_data()` is replaced by `status_fragment()`:

```rust
trait InteractiveWidget {
    fn handle_action(&mut self, action: Action, ctx: &mut WidgetContext) -> Outcome;
    fn handle_raw_key(&mut self, key: KeyEvent, ctx: &mut WidgetContext) -> Outcome { ... }
    fn handle_mouse(&mut self, mouse: MouseEvent, ctx: &mut WidgetContext) -> Outcome { ... }
    fn render(&mut self, frame: &mut Frame, area: Rect, ctx: &mut RenderContext);
    fn captures_raw_keys(&self) -> bool { false }
    fn as_any(&self) -> &dyn Any;
    fn as_any_mut(&mut self) -> &mut dyn Any;

    fn binding_mode(&self) -> KeyBindingMode;
    fn status_fragment(&self) -> StatusFragment { StatusFragment::default() }
}
```

### Status Bar Simplification

The status bar becomes a pure renderer. Screen resolves all content before calling it:

1. Walk the widget stack for `status_fragment()` → resolved `StatusContent`
2. Get `binding_mode()` from the top widget → look up chips from the compiled binding table
3. Get in-flight task info → `TaskSection`
4. Pass all three to the status bar

The `render_bespoke()` signature changes to accept pre-resolved content:

```rust
fn render_bespoke(
    &mut self,
    status: StatusContent,
    key_chips: Vec<KeyChip>,
    task: Option<TaskSection>,
    error_items: Vec<VisibleStatusItem>,
    mode_indicators: Vec<ModeIndicator>,
    show_keys: bool,
    theme: &Theme,
    frame: &mut Frame,
    area: Rect,
)
```

The ~200 lines of `status_bar_content()` with its `ModeId` matching, `UiMode` fallback, and `RepoUiState` reads are replaced by the resolution logic in Screen.

Error status items (provider errors with dismiss buttons) and the task spinner remain app-level concerns — Screen passes them alongside the fragment-derived content. Errors take priority: when a visible error exists, it replaces the fragment's status content in the left section. The status bar renderer handles this — if `error_items` is non-empty, it displays the error instead of the `StatusContent`.

**Mode indicators** (layout icon, host label) remain app-level. Screen passes them to the status bar alongside the other resolved content. They are not part of `StatusFragment` — they reflect app-wide state, not widget state.

**`show_keys` toggle** remains as-is. Screen passes the flag to the status bar, which uses it to decide whether to render the key chips. This is orthogonal to how chips are derived.

### IssueSearchWidget / CommandPaletteWidget Migration

These widgets currently write `active_search_query` to `ctx.repo_ui`. Change them to use `AppAction`:

```rust
AppAction::SetSearchQuery { repo: RepoIdentity, query: String }
AppAction::ClearSearchQuery { repo: RepoIdentity }
```

`App::process_app_actions()` writes directly to `RepoPage.active_search_query`.

`IssueSearchWidget` currently syncs its input text into `UiMode::IssueSearch` so the status bar can display it. After this change, it provides `StatusFragment { status: Some(ActiveInput { prefix: "SEARCH", text }) }` via `status_fragment()`. The `UiMode::IssueSearch` variant becomes dead code.

### What This Enables (Follow-Up)

With the status bar no longer reading `RepoUiState`, and modal widgets no longer writing to `ctx.repo_ui`, the sync bridge in `key_handlers.rs` can be removed. `RepoUiState` can then be deleted — its remaining fields (`has_unseen_changes`) move to the tab label system, and the old ctx-based methods on `WorkItemTable` and `PreviewPanel` are deleted. This is mechanical cleanup and does not require design decisions.

### Who Provides What

| Widget | `binding_mode()` | `status_fragment()` |
|--------|------------------|---------------------|
| RepoPage | `Normal` or `Composed([Normal, SearchActive])` | `Label("3 SELECTED")`, `Label("SEARCH \"q\"")`, or default |
| OverviewPage | `Overview` | `Label("FLOTILLA")` |
| HelpWidget | `Help` | `Label("HELP")` |
| ActionMenuWidget | `ActionMenu` | `Label("ACTIONS")` |
| DeleteConfirmWidget | `DeleteConfirm` | `Label("CONFIRM DELETE")` |
| CloseConfirmWidget | `CloseConfirm` | `Label("CONFIRM CLOSE")` |
| BranchInputWidget | `BranchInput` | `Progress("Generating...")` or `ActiveInput { prefix: "NEW BRANCH", text }` |
| IssueSearchWidget | `IssueSearch` | `ActiveInput { prefix: "SEARCH", text }` |
| CommandPaletteWidget | `CommandPalette` | `ActiveInput { prefix: "/", text }` |
| FilePickerWidget | `FilePicker` | `Label("ADD REPO")` |

Every modal widget must provide a status fragment. Returning default causes a regression — the status bar shows "/ for commands" instead of the mode-specific label.
