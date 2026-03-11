# Research Spike: Terminal Theming and Keybinding Configuration

Research findings and recommended approaches for issues #217 (colour themes) and #218 (key bindings).

## Methodology

Examined 7 ratatui/terminal TUI codebases and evaluated relevant crates:

| Project | Keybindings | Theming | Config format |
|---------|-------------|---------|---------------|
| **helix** | Trie-based, per-mode HashMap, recursive merge | HashMap scopes with hierarchical fallback, 100+ community themes | TOML |
| **yazi** | Prepend/append arrays, multi-key sequences, 8 layers | 3-layer merge (preset/flavor/user), per-widget sections | TOML |
| **zellij** | Per-mode HashMap, `shared_except`/`shared_among`, `unbind` | Widget-role names with emphasis sub-colours | KDL |
| **gitui** | Flat struct + `struct_patch` for partial overrides | Flat `Theme` struct with style-producing methods | RON |
| **television** | HashMap + composable multi-action bindings | Flat struct + `ThemeOverrides`, `Colorscheme` intermediate | TOML |
| **bottom** | Hardcoded only | Per-widget TOML sections, 6 built-in themes, hex/RGB/named | TOML |
| **lazygit** (Go) | Per-context YAML, custom command bindings, `<disabled>` | Configurable selection/border/author colours | YAML |

---

## Part 1: Colour Theming (#217)

### Crates evaluated

| Crate | What it does | Verdict |
|-------|-------------|---------|
| `catppuccin` (with `ratatui` feature) | 4 flavours, 26 colours each, `impl From<Color> for ratatui::Color` | Best default palette. Not a theming framework. |
| `ratatui-base16` | 16-slot base16 palettes, YAML/TOML loading | Too few slots, syntax-highlighting semantics, poor fit for TUI widgets |
| ratatui built-in palettes | Tailwind/Material colour constants + `serde` feature for `Style`/`Color` | Building blocks, not a framework |
| `tui-theme-builder` | Proc macro for theme structs | Under-maintained, not recommended |

Every major TUI rolls its own theme system. The dispatch logic from semantic colour names to ratatui `Style` objects is inherently app-specific.

### Patterns observed

**gitui** (simplest, closest fit): flat `Theme` struct with ~20 `Color` fields. Style-producing *methods* on the struct (e.g. `theme.tab(selected)`, `theme.text(enabled, selected)`) that combine colours with modifiers and context. `struct_patch` for partial TOML overrides — users specify only the fields they want to change.

**television** (clean intermediate layer): flat `Theme` struct deserialized from TOML, converted to a `Colorscheme` struct that groups colours by widget area (`general`, `results`, `preview`, `input`, `mode`). This separation keeps the config simple while giving renderers a focused palette.

**helix** (most flexible): `HashMap<String, Style>` with dot-separated hierarchical scopes (`ui.statusline.inactive`). Fallback walks up the hierarchy. Theme inheritance via `inherits`. Overkill for a non-editor TUI.

**bottom** (per-widget config): TOML sections per widget area (`[styles.cpu]`, `[styles.widgets]`). Two input formats: shorthand (just a colour string) and full format (table with `color`, `bg_color`, `bold`, `italics`). Good UX for partial customisation.

### Recommended approach

A **gitui-inspired flat struct** with a **television-style widget-area grouping**, using **catppuccin as the default palette**.

#### Phase 1: Centralise colours into a Theme struct

Define a `Theme` struct in `crates/flotilla-tui/src/theme.rs` with semantic colour fields covering all current uses:

```rust
pub struct Theme {
    // Chrome
    pub tab_active: Color,
    pub tab_inactive: Color,
    pub tab_label_bg: Color,
    pub border: Color,
    pub row_highlight: Color,
    pub multi_select_bg: Color,
    pub section_header: Color,
    pub muted: Color,

    // Work item types
    pub checkout: Color,
    pub session: Color,
    pub change_request: Color,
    pub issue: Color,
    pub remote_branch: Color,
    pub workspace: Color,

    // Semantic
    pub branch: Color,
    pub path: Color,
    pub error: Color,
    pub warning: Color,
    pub info: Color,

    // Interactive
    pub action_menu_highlight: Color,
    pub input_text: Color,

    // Status
    pub status_ok: Color,
    pub status_error: Color,
    pub in_flight: Color,
}
```

Add style-producing methods for common patterns:

```rust
impl Theme {
    pub fn tab_style(&self, active: bool) -> Style { ... }
    pub fn work_item_style(&self, kind: WorkItemKind) -> Style { ... }
    pub fn status_style(&self, status: &ProviderStatus) -> Style { ... }
}
```

Pass `&Theme` to all render functions, replacing the ~86 hardcoded `Color::*` references across `ui.rs` (~70) and `ui_helpers.rs` (~16).

#### Phase 2: Default palette via catppuccin

Add `catppuccin = { version = "2", features = ["ratatui"] }`. Map Mocha colours to theme fields. This gives a polished default with zero user configuration.

#### Phase 3: User-configurable themes via TOML

Enable ratatui's `serde` feature. Derive `Serialize + Deserialize` on the Theme struct. Store user overrides in `~/.config/flotilla/theme.toml`. For partial overrides, the simplest approach is to make all Theme fields `Option<Color>` in the config struct and merge non-None values over defaults — no extra crate needed. Alternatively, `struct-patch` (used by gitui) generates a `Patch` struct with all-Optional fields via proc macro, which is cleaner if the Theme struct grows large.

Colour input formats to support: named (`"red"`), hex (`"#ff5555"`), indexed (`42`). All natively supported by ratatui's `Color` serde implementation.

### Key decisions

| Decision | Recommendation | Rationale |
|----------|---------------|-----------|
| Flat struct vs HashMap | Flat struct | Compile-time safety, ~25 fields is manageable |
| Colour fields vs full Style fields | Colour fields + style methods | Keeps config simple, methods compose styles |
| catppuccin vs hand-picked | catppuccin default, framework-agnostic struct | Professional palette with light/dark variants |
| Light/dark mode | Defer to Phase 3 | catppuccin Latte (light) and Mocha (dark) ready when needed |
| Built-in only vs configurable | Phase 1 built-in, Phase 3 adds TOML | Non-breaking progression |

---

## Part 2: Key Binding Configuration (#218)

### Crates evaluated

| Crate | What it does | Verdict |
|-------|-------------|---------|
| `crokey` | Parse/display crossterm key combos, serde support, `key!()` macro | Best for key string parsing. Not a binding framework. Battle-tested (broot). |
| `keybinds-rs` | Key binding dispatcher with crossterm feature | Too new (0.0.3), no modal support |
| `crossterm-keybind` | Derive macro for keybinding enums | No modal awareness, forces flat enum |
| `keymap-rs` | Parse terminal input events from config | Poorly documented, small community |

Again, every major TUI rolls its own binding system. The dispatch logic is tightly coupled to the app's mode/state model.

### Patterns observed

**helix** (gold standard): `HashMap<Mode, KeyTrie>` where `KeyTrie` is a recursive trie supporting key sequences. Programmatic defaults merged with user TOML via recursive `merge_keys()`. Dash-separated key strings (`C-s`, `A-x`). Compile-time conflict detection in defaults.

**yazi** (most user-friendly): `KeymapRules` with `keymap`, `prepend_keymap`, `append_keymap` — users extend defaults without replacing them. Array-based (order matters for multi-key). 8 mode layers. `Chord` struct with `on: Vec<Key>` for multi-key sequences.

**zellij** (best shared-bindings model): `HashMap<InputMode, HashMap<Key, Vec<Action>>>`. `shared_except` and `shared_among` blocks apply bindings across multiple modes. `clear-defaults` per mode. `unbind` for removing specific keys.

**television** (simplest): `HashMap<Key, Actions>` with composable multi-action bindings (`ctrl-s = ["reload_source", "copy"]`). Custom key parsing (dash-separated).

### Recommended approach

Flotilla's current binding surface is small (~25 bindings across 8 `UiMode` variants: Normal, Help, Config, ActionMenu, BranchInput, FilePicker, DeleteConfirm, IssueSearch). It has a clean Intent-based architecture. The key insight from the user: **shared actions (confirm, cancel, scroll, navigate) should be widget-focus-driven rather than duplicated per mode.**

This means two layers:

1. **Global action bindings** — shared navigation, confirm/cancel, scroll. These dispatch based on which widget has focus, not which mode is active.
2. **Mode-specific bindings** — actions only available in a particular mode (e.g. `d` for delete in Normal, `y` for confirm in DeleteConfirm).

#### Action enum

A new `Action` enum that wraps the existing `Intent` for work-item operations and adds UI-level actions:

```rust
pub enum Action {
    // Navigation (widget-focus-aware)
    SelectNext,          // j/Down — table row, menu item, log entry, etc.
    SelectPrev,          // k/Up
    ScrollDown,          // scroll context based on focus
    ScrollUp,
    Confirm,             // Enter — execute focused item
    Dismiss,             // Esc — context-sensitive: in overlays, closes them;
                         // in Normal, cascades: clear search → hide providers
                         // → clear selection → quit

    // Tabs
    PrevTab,
    NextTab,
    MoveTabLeft,
    MoveTabRight,

    // Mode switches
    OpenActionMenu,
    BranchInput,
    IssueSearch,
    FilePicker,
    ToggleHelp,
    ToggleProviders,
    ToggleDebug,

    // Work-item operations (wraps Intent)
    Dispatch(Intent),    // e.g. Dispatch(RemoveCheckout)

    // Multi-select
    ToggleMultiSelect,

    // App lifecycle
    Refresh,
    Quit,
}
```

`Dismiss` unifies the old `Cancel` and `Escape` concepts. In overlay modes (ActionMenu, DeleteConfirm, Help, etc.), it closes the overlay and returns to Normal. In Normal mode, it cascades through dismissible states (active search → provider panel → multi-selection) before quitting. The focus-aware dispatch in `dispatch_action` handles this routing.

The TOML action strings map to `Action` variants via a string lookup table. `Intent`-wrapping actions use the intent name directly: `"remove_checkout"` → `Action::Dispatch(Intent::RemoveCheckout)`, `"open_change_request"` → `Action::Dispatch(Intent::OpenChangeRequest)`, etc.

#### Config format

TOML with two sections: shared bindings and per-mode overrides.

```toml
# Shared bindings — dispatched based on widget focus.
# These work in any mode unless overridden by a mode-specific binding.
[keys.shared]
"j"     = "select_next"
"Down"  = "select_next"
"k"     = "select_prev"
"Up"    = "select_prev"
"Enter" = "confirm"
"Esc"   = "dismiss"
"["     = "prev_tab"
"]"     = "next_tab"

# Normal mode — extends shared bindings
[keys.normal]
"q"     = "quit"
"Space" = "toggle_multi_select"
"."     = "open_action_menu"
"d"     = "remove_checkout"
"p"     = "open_change_request"
"r"     = "refresh"
"D"     = "toggle_debug"
"c"     = "toggle_providers"
"n"     = "branch_input"
"/"     = "issue_search"
"a"     = "file_picker"
"?"     = "toggle_help"
"{"     = "move_tab_left"
"}"     = "move_tab_right"

# Delete confirm — Esc/Enter inherited from shared, add y/n
[keys.delete_confirm]
"y"     = "confirm"
"n"     = "dismiss"

# Action menu — navigation and dismiss inherited from shared
# (no extra bindings needed)

# Config — adds quit on q
[keys.config]
"q"     = "quit"
```

The `[keys.shared]` section is inspired by zellij's `shared_except`/`shared_among` but simpler — shared bindings apply everywhere, mode-specific bindings override them.

#### Dispatch flow

```
KeyEvent
  → look up in mode-specific bindings
  → if not found, look up in shared bindings
  → if found, dispatch Action
  → Action::SelectNext/SelectPrev/Confirm/Dismiss route based on widget focus
  → Action::Dispatch(intent) resolves via existing Intent::resolve()
```

Widget focus determines what navigation/confirm/cancel *means*:
- Table focused → SelectNext moves row, Confirm does action_enter
- Action menu focused → SelectNext moves menu cursor, Confirm executes action
- Help focused → SelectNext scrolls down
- Log (config) focused → SelectNext scrolls log

#### Implementation strategy

1. **Use `crokey` for key parsing** — handles `FromStr`/`Display` for key combos with serde support. Watch for crossterm version alignment.
2. **Build the dispatch layer** — `Keymap { shared: HashMap<KeyCombo, Action>, modes: HashMap<UiMode, HashMap<KeyCombo, Action>> }` with a `resolve(mode, key) -> Option<Action>` method.
3. **Defaults in Rust, overrides in TOML** — programmatic defaults, merged with user config. Mode-specific entries override shared entries.
4. **Widget focus routing** — `SelectNext`, `SelectPrev`, `Confirm`, `Cancel` dispatch differently based on the current UI focus context. This formalises the existing ad-hoc per-mode handling.
5. **Auto-generated help** — the help screen reads from the active keymap + action descriptions, not hardcoded text.

### Key decisions

| Decision | Recommendation | Rationale |
|----------|---------------|-----------|
| Library vs hand-rolled dispatch | `crokey` for parsing, hand-rolled dispatch | No crate handles modal dispatch well |
| Shared vs per-mode bindings | Both: `[keys.shared]` + `[keys.<mode>]` | Eliminates duplication, matches widget-focus model |
| Key sequences (chords) | Defer | Binding surface is small, adds significant complexity |
| Which modes are configurable | `shared` + `normal` + `config` + `delete_confirm` are configurable; ActionMenu, BranchInput, IssueSearch, FilePicker remain hardcoded | Overlay modes have tiny fixed binding sets (text input via tui-input, or just navigation); Normal and Config are where users want customisation |
| Config file location | `~/.config/flotilla/keybindings.toml` | Separate from main config, consistent with gitui |
| Help screen | Auto-generated from active keymap | Stays in sync with user customisations |
| Mouse bindings | Out of scope | More complex, less commonly customised |
| `crokey` vs custom key parsing | `crokey` if crossterm versions align; otherwise ~150 lines of custom parsing (helix's `input.rs` is a good reference) | `crokey` is mature but re-exports crossterm, risking version conflicts. As of crokey 1.4.0, it depends on crossterm ^0.29 which matches flotilla's current version. |
| Modifier keys (Ctrl, Alt) | Supported in config format (e.g. `"C-r"` = `"refresh"`) but not needed for current bindings | `crokey` handles modifiers natively; no current bindings use modifiers but the format should support them for user customisation |

---

## Part 3: Widget Focus Model (new design consideration)

The keybinding research surfaced a structural insight: flotilla currently encodes "what navigation means" via per-mode match arms. With configurable bindings, this should be formalised into a widget focus system.

### Current state

```rust
// Same physical key (j) has different semantic meanings per mode:
UiMode::Normal => self.select_next(),           // table row
UiMode::Help => self.ui.help_scroll += 1,       // help text
UiMode::Config => self.ui.event_log.selected += 1, // log entry
UiMode::ActionMenu => index += 1,               // menu item
```

### Proposed: focus-aware dispatch

```rust
enum FocusTarget {
    WorkItemTable,
    ActionMenu,
    HelpText,
    EventLog,
    BranchInput,
    IssueSearchInput,
    FilePickerList,
    DeleteConfirmDialog,
}

impl App {
    fn current_focus(&self) -> FocusTarget { ... }

    fn dispatch_action(&mut self, action: Action) {
        match (action, self.current_focus()) {
            (Action::SelectNext, FocusTarget::WorkItemTable) => self.select_next(),
            (Action::SelectNext, FocusTarget::ActionMenu) => self.menu_next(),
            (Action::SelectNext, FocusTarget::HelpText) => self.help_scroll_down(),
            (Action::SelectNext, FocusTarget::EventLog) => self.log_scroll_down(),
            (Action::Confirm, FocusTarget::WorkItemTable) => self.action_enter(),
            (Action::Confirm, FocusTarget::ActionMenu) => self.execute_menu_action(),
            (Action::Confirm, FocusTarget::DeleteConfirmDialog) => self.confirm_delete(),
            (Action::Dismiss, FocusTarget::WorkItemTable) => self.dismiss_cascade(),
            (Action::Dismiss, _) => self.return_to_normal(),
            // ...
        }
    }
}
```

This decouples key bindings from behaviour, making both independently testable. The focus target is derived from `UiMode` but could later support split-pane focus (e.g. preview panel getting focus).

This is a prerequisite for configurable keybindings and should be implemented first.

---

## Implementation phasing

### Phase 1: Foundation (no user config yet)
1. Create `Theme` struct, replace all hardcoded colours in `ui.rs` / `ui_helpers.rs`
2. Add catppuccin as default palette
3. Formalise `FocusTarget` enum and focus-aware dispatch (**prerequisite for step 4**)
4. Create `Action` enum, refactor `key_handlers.rs` to route through it (depends on step 3)

### Phase 2: Configuration
5. Add `crokey` dependency, build `Keymap` struct with defaults
6. Add TOML loading for `~/.config/flotilla/keybindings.toml`
7. Add TOML loading for `~/.config/flotilla/theme.toml`
8. Auto-generate help screen from active keymap

### Phase 3: Polish
9. Light/dark theme switching (catppuccin Latte vs Mocha)
10. Ship additional built-in themes (gruvbox, nord, etc.)
11. Conflict detection and user-friendly error reporting on invalid keybinding configs

---

## Reference files

### Flotilla (current)
- `crates/flotilla-tui/src/app/key_handlers.rs` — hardcoded key dispatch
- `crates/flotilla-tui/src/app/intent.rs` — Intent enum (domain operations)
- `crates/flotilla-tui/src/app/ui_state.rs` — UiMode enum
- `crates/flotilla-tui/src/ui.rs` — ~70 hardcoded Color references
- `crates/flotilla-tui/src/ui_helpers.rs` — work item icon colours

### Reference implementations (cloned in ~/dev/)
- `gitui/src/ui/style.rs` — Theme struct + style methods
- `gitui/src/keys/key_list.rs` — KeysList + struct_patch
- `helix/helix-term/src/keymap.rs` — KeyTrie, merge_keys, modal dispatch
- `helix/helix-view/src/theme.rs` — HashMap scopes with fallback
- `helix/helix-view/src/input.rs` — key string parsing (lines 335-448)
- `yazi/yazi-config/src/keymap/` — prepend/append model, Chord struct
- `yazi/yazi-config/src/theme/` — 3-layer merge, Style bridge
- `zellij/zellij-utils/src/input/keybinds.rs` — shared_except/shared_among
- `television/television/config/themes.rs` — Theme + ThemeOverrides
- `television/television/config/keybindings.rs` — key parsing, Actions
- `bottom/src/options/config/style.rs` — per-widget theming

### Crates
- [`catppuccin`](https://crates.io/crates/catppuccin) — default palette (with `ratatui` feature)
- [`crokey`](https://crates.io/crates/crokey) — key combo parsing/display for crossterm
- [`struct-patch`](https://crates.io/crates/struct-patch) — partial struct overrides (used by gitui)

### Blog posts and discussions
- [Manage keybindings in a Rust terminal application](https://dystroy.org/blog/keybindings/) — broot author's practical experience
- [Configurable Keybindings recipe (Ant Lab)](https://www.ant-lab.tw/blog/2025-12-24/) — ratatui-specific patterns
- [Ratatui Discussion #627](https://github.com/ratatui/ratatui/discussions/627) — community discussion on keybinding support
