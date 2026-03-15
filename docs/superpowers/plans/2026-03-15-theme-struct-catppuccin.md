# Theme Struct with Catppuccin Default Palette — Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace all hardcoded colours in the TUI crate with a `Theme` struct, ship catppuccin Mocha as the default palette, preserve the current palette as `Theme::classic()`, and support runtime theme switching.

**Architecture:** A flat `Theme` struct with ~35 `Color` fields and style-producing methods lives in `crates/flotilla-tui/src/theme.rs`. It is stored on `App` and passed as `&Theme` to all render functions. Two constructors (`catppuccin_mocha`, `classic`) provide built-in palettes. A `CycleTheme` action and `--theme` CLI arg control selection.

**Tech Stack:** `catppuccin = { version = "2", features = ["ratatui"] }`, ratatui `Color`/`Style`, clap for CLI arg.

**Spec:** `docs/superpowers/specs/2026-03-15-theme-struct-catppuccin-design.md`

---

## File structure

| File | Status | Responsibility |
|------|--------|----------------|
| `crates/flotilla-tui/src/theme.rs` | Create | `Theme` struct, `TextTransform`, `BarKind`, `BarSiteStyle`, constructors, style methods |
| `crates/flotilla-tui/src/lib.rs` | Modify | Add `pub mod theme;` |
| `crates/flotilla-tui/Cargo.toml` | Modify | Add `catppuccin` dependency |
| `crates/flotilla-tui/src/ui.rs` | Modify | Thread `&Theme` through all render functions, replace ~70 hardcoded `Color::*` |
| `crates/flotilla-tui/src/ui_helpers.rs` | Modify | `work_item_icon()` takes `&Theme` |
| `crates/flotilla-tui/src/segment_bar.rs` | Modify | `TabBarStyle`/`RibbonStyle` take `&Theme`; add themed variants |
| `crates/flotilla-tui/src/shimmer.rs` | Modify | Extract RGB from `theme.shimmer_base`/`shimmer_highlight` |
| `crates/flotilla-tui/src/run.rs` | Modify | Pass `&app.theme` to `ui::render` |
| `crates/flotilla-tui/src/app/mod.rs` | Modify | Store `Theme` on `App`, add construction |
| `crates/flotilla-tui/src/app/key_handlers.rs` | Modify | Add `CycleTheme` action + keybinding (`T`) |
| `crates/flotilla-core/src/config.rs` | Modify | Add `theme: String` to `UiConfig` |
| `src/main.rs` | Modify | Add `--theme` CLI arg, pass to `App::new` |
| `crates/flotilla-tui/tests/support/mod.rs` | Modify | Pass `Theme::classic()` to `render_to_buffer` |
| `crates/flotilla-tui/tests/snapshots.rs` | Modify | Add theme switching test |

---

## Chunk 1: Theme struct foundation

### Task 1: Add catppuccin dependency

**Files:**
- Modify: `crates/flotilla-tui/Cargo.toml`

- [ ] **Step 1: Add catppuccin to Cargo.toml**

Add after the `dirs = "6"` line:

```toml
catppuccin = { version = "2", features = ["ratatui"] }
```

- [ ] **Step 2: Verify it resolves**

Run: `cargo check -p flotilla-tui 2>&1 | tail -5`
Expected: successful check (or only pre-existing warnings)

- [ ] **Step 3: Commit**

```bash
git add crates/flotilla-tui/Cargo.toml Cargo.lock
git commit -m "chore: add catppuccin dependency for theme palette"
```

---

### Task 2: Create theme.rs with types and classic constructor

**Files:**
- Create: `crates/flotilla-tui/src/theme.rs`
- Modify: `crates/flotilla-tui/src/lib.rs`

- [ ] **Step 1: Write tests for Theme::classic field values**

At the bottom of the new `theme.rs`, add a `#[cfg(test)] mod tests` with spot-check assertions that `Theme::classic()` reproduces the current hardcoded colours:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Color;

    #[test]
    fn classic_tab_colours() {
        let t = Theme::classic();
        assert_eq!(t.tab_active, Color::Cyan);
        assert_eq!(t.tab_inactive, Color::DarkGray);
    }

    #[test]
    fn classic_work_item_colours() {
        let t = Theme::classic();
        assert_eq!(t.checkout, Color::Green);
        assert_eq!(t.session, Color::Magenta);
        assert_eq!(t.change_request, Color::Blue);
        assert_eq!(t.issue, Color::Yellow);
        assert_eq!(t.remote_branch, Color::DarkGray);
    }

    #[test]
    fn classic_indexed_colours() {
        let t = Theme::classic();
        assert_eq!(t.status_error, Color::Indexed(203));
        assert_eq!(t.source, Color::Indexed(67));
        assert_eq!(t.path, Color::Indexed(245));
        assert_eq!(t.key_hint, Color::Indexed(208));
        assert_eq!(t.multi_select_bg, Color::Indexed(236));
    }

    #[test]
    fn classic_logo_colours() {
        let t = Theme::classic();
        assert_eq!(t.logo_fg, Color::Black);
        assert_eq!(t.logo_bg, Color::Cyan);
        assert_eq!(t.logo_config_bg, Color::White);
    }

    #[test]
    fn classic_shimmer_colours() {
        let t = Theme::classic();
        assert_eq!(t.shimmer_base, Color::Rgb(140, 130, 40));
        assert_eq!(t.shimmer_highlight, Color::Rgb(255, 240, 120));
    }

    #[test]
    fn classic_name() {
        assert_eq!(Theme::classic().name, "classic");
    }

    #[test]
    fn text_transform_uppercase() {
        assert_eq!(TextTransform::Uppercase.apply("hello World"), "HELLO WORLD");
    }

    #[test]
    fn text_transform_titlecase() {
        assert_eq!(TextTransform::Titlecase.apply("hello world"), "Hello World");
    }

    #[test]
    fn text_transform_as_is() {
        assert_eq!(TextTransform::AsIs.apply("Hello World"), "Hello World");
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p flotilla-tui --lib theme 2>&1 | tail -10`
Expected: compilation errors (Theme does not exist yet)

- [ ] **Step 3: Write the Theme struct and supporting types**

Create `crates/flotilla-tui/src/theme.rs` with the full struct definition. All fields match the spec in `docs/superpowers/specs/2026-03-15-theme-struct-catppuccin-design.md`:

```rust
use ratatui::style::{Color, Modifier, Style};

/// How bar labels are transformed before display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextTransform {
    Uppercase,
    Titlecase,
    AsIs,
}

impl TextTransform {
    pub fn apply(&self, text: &str) -> String {
        match self {
            TextTransform::Uppercase => text.to_uppercase(),
            TextTransform::Titlecase => text
                .split_whitespace()
                .map(|word| {
                    let mut chars = word.chars();
                    match chars.next() {
                        Some(c) => {
                            let upper: String = c.to_uppercase().collect();
                            format!("{upper}{}", chars.as_str().to_lowercase())
                        }
                        None => String::new(),
                    }
                })
                .collect::<Vec<_>>()
                .join(" "),
            TextTransform::AsIs => text.to_string(),
        }
    }
}

/// Which segment bar renderer to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BarKind {
    /// Pipe-separated labels: ` Alpha | Beta | Gamma `
    Pipe,
    /// Powerline chevron-delimited chips
    Chevron,
}

/// Per-site bar configuration (tab bar, status bar).
#[derive(Debug, Clone)]
pub struct BarSiteStyle {
    pub kind: BarKind,
    pub label_transform: TextTransform,
}

/// All visual parameters for the TUI.
///
/// Constructed via named constructors (`catppuccin_mocha`, `classic`).
/// Render functions read colour fields directly and call style methods
/// for common patterns.
#[derive(Debug, Clone)]
pub struct Theme {
    pub name: &'static str,

    // ── Chrome ──────────────────────────────────────
    pub tab_active: Color,
    pub tab_inactive: Color,
    pub border: Color,
    pub row_highlight: Color,
    pub multi_select_bg: Color,
    pub section_header: Color,
    pub muted: Color,

    // ── Logo tab ────────────────────────────────────
    pub logo_fg: Color,
    pub logo_bg: Color,
    pub logo_config_bg: Color,

    // ── Work item kinds ─────────────────────────────
    pub checkout: Color,
    pub session: Color,
    pub change_request: Color,
    pub issue: Color,
    pub remote_branch: Color,
    pub workspace: Color,

    // ── Semantic ────────────────────────────────────
    pub branch: Color,
    pub path: Color,
    pub source: Color,
    pub git_status: Color,
    pub error: Color,
    pub warning: Color,
    pub info: Color,

    // ── Interactive ─────────────────────────────────
    pub action_highlight: Color,
    pub input_text: Color,

    // ── Status ──────────────────────────────────────
    pub status_ok: Color,
    pub status_error: Color,

    // ── Surfaces ────────────────────────────────────
    pub base: Color,
    pub surface: Color,
    pub text: Color,
    pub subtext: Color,

    // ── Shimmer ─────────────────────────────────────
    pub shimmer_base: Color,
    pub shimmer_highlight: Color,

    // ── Bar chrome ──────────────────────────────────
    pub bar_bg: Color,
    pub key_hint: Color,
    pub key_chip_bg: Color,
    pub key_chip_fg: Color,

    // ── Bar site styling ────────────────────────────
    pub tab_bar: BarSiteStyle,
    pub status_bar: BarSiteStyle,
}
```

Add the `classic()` constructor that reproduces the current hardcoded values:

```rust
impl Theme {
    /// The original hardcoded colour palette.
    pub fn classic() -> Self {
        Self {
            name: "classic",
            tab_active: Color::Cyan,
            tab_inactive: Color::DarkGray,
            border: Color::DarkGray,
            row_highlight: Color::DarkGray,
            multi_select_bg: Color::Indexed(236),
            section_header: Color::Yellow,
            muted: Color::DarkGray,

            logo_fg: Color::Black,
            logo_bg: Color::Cyan,
            logo_config_bg: Color::White,

            checkout: Color::Green,
            session: Color::Magenta,
            change_request: Color::Blue,
            issue: Color::Yellow,
            remote_branch: Color::DarkGray,
            workspace: Color::Green,

            branch: Color::Cyan,
            path: Color::Indexed(245),
            source: Color::Indexed(67),
            git_status: Color::Red,
            error: Color::Red,
            warning: Color::Yellow,
            info: Color::DarkGray,

            action_highlight: Color::Blue,
            input_text: Color::Cyan,

            status_ok: Color::Green,
            status_error: Color::Indexed(203),

            base: Color::Reset,
            surface: Color::DarkGray,
            text: Color::White,
            subtext: Color::DarkGray,

            shimmer_base: Color::Rgb(140, 130, 40),
            shimmer_highlight: Color::Rgb(255, 240, 120),

            bar_bg: Color::Black,
            key_hint: Color::Indexed(208),
            key_chip_bg: Color::DarkGray,
            key_chip_fg: Color::Black,

            tab_bar: BarSiteStyle { kind: BarKind::Pipe, label_transform: TextTransform::AsIs },
            status_bar: BarSiteStyle { kind: BarKind::Chevron, label_transform: TextTransform::Uppercase },
        }
    }
}
```

- [ ] **Step 4: Add `pub mod theme;` to lib.rs**

In `crates/flotilla-tui/src/lib.rs`, add `pub mod theme;` after the existing module declarations.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p flotilla-tui --lib theme 2>&1 | tail -15`
Expected: all tests pass

- [ ] **Step 6: Commit**

```bash
git add crates/flotilla-tui/src/theme.rs crates/flotilla-tui/src/lib.rs
git commit -m "feat: add Theme struct with classic constructor and TextTransform"
```

---

### Task 3: Add catppuccin_mocha constructor

**Files:**
- Modify: `crates/flotilla-tui/src/theme.rs`

- [ ] **Step 1: Write tests for catppuccin_mocha**

Add to the existing test module:

```rust
#[test]
fn catppuccin_mocha_name() {
    assert_eq!(Theme::catppuccin_mocha().name, "catppuccin");
}

#[test]
fn catppuccin_mocha_uses_rgb_colours() {
    let t = Theme::catppuccin_mocha();
    // Catppuccin colours are all RGB, not named
    assert!(matches!(t.tab_active, Color::Rgb(_, _, _)));
    assert!(matches!(t.checkout, Color::Rgb(_, _, _)));
    assert!(matches!(t.error, Color::Rgb(_, _, _)));
}

#[test]
fn catppuccin_differs_from_classic() {
    let c = Theme::classic();
    let m = Theme::catppuccin_mocha();
    assert_ne!(c.tab_active, m.tab_active);
    assert_ne!(c.checkout, m.checkout);
    assert_ne!(c.error, m.error);
}

#[test]
fn available_themes_contains_both() {
    let themes = available_themes();
    assert_eq!(themes.len(), 2);
    assert_eq!((themes[0])().name, "catppuccin");
    assert_eq!((themes[1])().name, "classic");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p flotilla-tui --lib theme 2>&1 | tail -10`
Expected: compilation error — `catppuccin_mocha` and `available_themes` do not exist

- [ ] **Step 3: Implement catppuccin_mocha constructor**

Add to `theme.rs`, using the `catppuccin` crate:

```rust
use catppuccin::PALETTE;

impl Theme {
    /// Catppuccin Mocha palette — a warm dark theme.
    pub fn catppuccin_mocha() -> Self {
        let p = &PALETTE.mocha.colors;
        Self {
            name: "catppuccin",
            tab_active: p.sapphire.into(),
            tab_inactive: p.overlay0.into(),
            border: p.surface1.into(),
            row_highlight: p.surface0.into(),
            multi_select_bg: p.surface1.into(),
            section_header: p.yellow.into(),
            muted: p.overlay0.into(),

            logo_fg: p.crust.into(),
            logo_bg: p.sapphire.into(),
            logo_config_bg: p.text.into(),

            checkout: p.green.into(),
            session: p.mauve.into(),
            change_request: p.blue.into(),
            issue: p.yellow.into(),
            remote_branch: p.overlay0.into(),
            workspace: p.green.into(),

            branch: p.teal.into(),
            path: p.subtext0.into(),
            source: p.lavender.into(),
            git_status: p.red.into(),
            error: p.red.into(),
            warning: p.yellow.into(),
            info: p.blue.into(),

            action_highlight: p.blue.into(),
            input_text: p.teal.into(),

            status_ok: p.green.into(),
            status_error: p.red.into(),

            base: p.base.into(),
            surface: p.surface0.into(),
            text: p.text.into(),
            subtext: p.subtext0.into(),

            shimmer_base: p.yellow.into(),
            shimmer_highlight: p.rosewater.into(),

            bar_bg: p.crust.into(),
            key_hint: p.peach.into(),
            key_chip_bg: p.surface1.into(),
            key_chip_fg: p.crust.into(),

            tab_bar: BarSiteStyle { kind: BarKind::Pipe, label_transform: TextTransform::AsIs },
            status_bar: BarSiteStyle { kind: BarKind::Chevron, label_transform: TextTransform::Uppercase },
        }
    }
}

/// Returns the ordered list of built-in themes for cycling.
pub fn available_themes() -> &'static [fn() -> Theme] {
    &[Theme::catppuccin_mocha, Theme::classic]
}

/// Resolve a theme name to a constructor. Falls back to catppuccin.
pub fn theme_by_name(name: &str) -> Theme {
    match name {
        "classic" => Theme::classic(),
        _ => Theme::catppuccin_mocha(),
    }
}
```

Note: `catppuccin::Color` implements `Into<ratatui::style::Color>` via the `ratatui` feature flag. The `.into()` calls convert directly.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p flotilla-tui --lib theme 2>&1 | tail -15`
Expected: all tests pass

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-tui/src/theme.rs
git commit -m "feat: add Theme::catppuccin_mocha constructor and theme_by_name"
```

---

### Task 4: Add style-producing methods

**Files:**
- Modify: `crates/flotilla-tui/src/theme.rs`

- [ ] **Step 1: Write tests for style methods**

```rust
#[test]
fn logo_style_normal() {
    let t = Theme::classic();
    let s = t.logo_style(false);
    assert_eq!(s.fg, Some(Color::Black));
    assert_eq!(s.bg, Some(Color::Cyan));
    assert!(s.add_modifier.contains(Modifier::BOLD));
}

#[test]
fn logo_style_config() {
    let t = Theme::classic();
    let s = t.logo_style(true);
    assert_eq!(s.bg, Some(Color::White));
}

#[test]
fn tab_style_active() {
    let t = Theme::classic();
    let s = t.tab_style(true, false);
    assert_eq!(s.fg, Some(Color::Cyan));
    assert!(s.add_modifier.contains(Modifier::BOLD));
}

#[test]
fn tab_style_inactive() {
    let t = Theme::classic();
    let s = t.tab_style(false, false);
    assert_eq!(s.fg, Some(Color::DarkGray));
    assert!(!s.add_modifier.contains(Modifier::BOLD));
}

#[test]
fn tab_style_dragging() {
    let t = Theme::classic();
    let s = t.tab_style(true, true);
    assert!(s.add_modifier.contains(Modifier::UNDERLINED));
}

#[test]
fn work_item_color_checkout() {
    let t = Theme::classic();
    assert_eq!(t.work_item_color(&WorkItemKind::Checkout), Color::Green);
}

#[test]
fn work_item_color_session() {
    let t = Theme::classic();
    assert_eq!(t.work_item_color(&WorkItemKind::Session), Color::Magenta);
}

#[test]
fn header_style_uses_section_header() {
    let t = Theme::classic();
    let s = t.header_style();
    assert_eq!(s.fg, Some(Color::Yellow));
    assert!(s.add_modifier.contains(Modifier::BOLD));
}

#[test]
fn log_level_style_error() {
    let t = Theme::classic();
    let s = t.log_level_style("ERROR");
    assert_eq!(s.fg, Some(Color::Red));
}

#[test]
fn log_level_style_warn() {
    let t = Theme::classic();
    let s = t.log_level_style("WARN");
    assert_eq!(s.fg, Some(Color::Yellow));
}

#[test]
fn log_level_style_debug() {
    let t = Theme::classic();
    let s = t.log_level_style("DEBUG");
    assert_eq!(s.fg, Some(Color::Cyan));
}

#[test]
fn log_level_style_info_uses_info_colour() {
    let t = Theme::classic();
    let s = t.log_level_style("INFO");
    assert_eq!(s.fg, Some(Color::DarkGray));
}

#[test]
fn change_request_status_color_merged() {
    let t = Theme::classic();
    assert_eq!(t.change_request_status_color("Merged"), Color::Green);
}

#[test]
fn change_request_status_color_closed() {
    let t = Theme::classic();
    assert_eq!(t.change_request_status_color("Closed"), Color::Yellow);
}

#[test]
fn change_request_status_color_open() {
    let t = Theme::classic();
    assert_eq!(t.change_request_status_color("Open"), Color::Red);
}

#[test]
fn peer_status_style_connected() {
    let t = Theme::classic();
    assert_eq!(t.peer_status_color("Connected"), Color::Green);
}

#[test]
fn peer_status_style_disconnected() {
    let t = Theme::classic();
    assert_eq!(t.peer_status_color("Disconnected"), Color::Red);
}

#[test]
fn peer_status_style_connecting() {
    let t = Theme::classic();
    assert_eq!(t.peer_status_color("Connecting"), Color::Yellow);
}

#[test]
fn peer_status_style_reconnecting() {
    let t = Theme::classic();
    assert_eq!(t.peer_status_color("Reconnecting"), Color::Yellow);
}

#[test]
fn peer_status_style_rejected() {
    let t = Theme::classic();
    assert_eq!(t.peer_status_color("Rejected"), Color::Red);
}
```

Note: these tests need `use flotilla_protocol::WorkItemKind;` at the top of the test module.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p flotilla-tui --lib theme 2>&1 | tail -10`
Expected: compilation errors — style methods do not exist

- [ ] **Step 3: Implement style methods**

Add to `impl Theme` in `theme.rs`:

```rust
use flotilla_protocol::WorkItemKind;

impl Theme {
    pub fn logo_style(&self, config_mode: bool) -> Style {
        let bg = if config_mode { self.logo_config_bg } else { self.logo_bg };
        Style::default().bold().fg(self.logo_fg).bg(bg)
    }

    pub fn tab_style(&self, active: bool, dragging: bool) -> Style {
        if active && dragging {
            Style::default().bold().fg(self.tab_active).add_modifier(Modifier::UNDERLINED)
        } else if active {
            Style::default().bold().fg(self.tab_active)
        } else {
            Style::default().fg(self.tab_inactive)
        }
    }

    pub fn work_item_color(&self, kind: &WorkItemKind) -> Color {
        match kind {
            WorkItemKind::Checkout => self.checkout,
            WorkItemKind::Session => self.session,
            WorkItemKind::ChangeRequest => self.change_request,
            WorkItemKind::Issue => self.issue,
            WorkItemKind::RemoteBranch => self.remote_branch,
        }
    }

    pub fn header_style(&self) -> Style {
        Style::default().fg(self.section_header).bold()
    }

    pub fn log_level_style(&self, level: &str) -> Style {
        let color = match level {
            "ERROR" => self.error,
            "WARN" => self.warning,
            "DEBUG" => self.branch,
            "INFO" => self.info,
            _ => self.muted,
        };
        Style::default().fg(color)
    }

    pub fn change_request_status_color(&self, status: &str) -> Color {
        match status {
            "Merged" => self.status_ok,
            "Closed" => self.warning,
            _ => self.error,
        }
    }

    pub fn status_style(&self, ok: bool) -> Style {
        if ok {
            Style::default().fg(self.status_ok)
        } else {
            Style::default().fg(self.status_error)
        }
    }

    /// Map peer connection state to a colour.
    /// Connected → status_ok, Disconnected/Rejected → error,
    /// Connecting/Reconnecting → warning.
    pub fn peer_status_color(&self, state: &str) -> Color {
        match state {
            "Connected" => self.status_ok,
            "Disconnected" | "Rejected" => self.error,
            "Connecting" | "Reconnecting" => self.warning,
            _ => self.muted,
        }
    }

    /// Transform a label according to a bar site's text transform setting.
    pub fn transform_label(&self, site: &BarSiteStyle, text: &str) -> String {
        site.label_transform.apply(text)
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p flotilla-tui --lib theme 2>&1 | tail -15`
Expected: all tests pass

- [ ] **Step 5: Run clippy**

Run: `cargo clippy -p flotilla-tui --all-targets --locked -- -D warnings 2>&1 | tail -10`
Expected: no errors

- [ ] **Step 6: Commit**

```bash
git add crates/flotilla-tui/src/theme.rs
git commit -m "feat: add Theme style methods for tabs, work items, status, logs"
```

---

## Chunk 2: Thread Theme through rendering

### Task 5: Update segment_bar.rs to accept &Theme

**Files:**
- Modify: `crates/flotilla-tui/src/segment_bar.rs`

- [ ] **Step 1: Add themed bar style structs**

Add after the existing `RibbonStyle` impl, keeping the originals for now (tests reference them):

```rust
use crate::theme::Theme;

/// Tab bar style that reads colours from Theme.
pub struct ThemedTabBarStyle<'a> {
    pub theme: &'a Theme,
}

impl BarStyle for ThemedTabBarStyle<'_> {
    fn render_item(&self, item: &SegmentItem) -> RenderedItem {
        let style = if let Some(override_style) = item.style_override {
            override_style
        } else if item.active && item.dragging {
            self.theme.tab_style(true, true)
        } else if item.active {
            self.theme.tab_style(true, false)
        } else {
            self.theme.tab_style(false, false)
        };
        RenderedItem::from_spans(vec![Span::styled(item.label.clone(), style)])
    }

    fn separator(&self) -> RenderedItem {
        RenderedItem::from_spans(vec![Span::styled(" | ", Style::default().fg(self.theme.muted))])
    }

    fn background_fill(&self) -> Option<Style> {
        None
    }
}

/// Ribbon (status bar) style that reads colours from Theme.
pub struct ThemedRibbonStyle<'a> {
    pub theme: &'a Theme,
}

impl BarStyle for ThemedRibbonStyle<'_> {
    fn render_item(&self, item: &SegmentItem) -> RenderedItem {
        let key = item.key_hint.as_deref().unwrap_or("");
        let label = self.theme.transform_label(&self.theme.status_bar, &item.label);
        RenderedItem::from_spans(vec![
            Span::styled(CHEVRON, Style::default().fg(self.theme.bar_bg).bg(self.theme.key_chip_bg)),
            Span::styled(" ", Style::default().fg(self.theme.key_chip_fg).bg(self.theme.key_chip_bg)),
            Span::styled("<", Style::default().fg(self.theme.key_chip_fg).bg(self.theme.key_chip_bg).bold()),
            Span::styled(key.to_string(), Style::default().fg(self.theme.key_hint).bg(self.theme.key_chip_bg).bold()),
            Span::styled(">", Style::default().fg(self.theme.key_chip_fg).bg(self.theme.key_chip_bg).bold()),
            Span::styled(format!(" {label} "), Style::default().fg(self.theme.key_chip_fg).bg(self.theme.key_chip_bg).bold()),
            Span::styled(CHEVRON, Style::default().fg(self.theme.key_chip_bg).bg(self.theme.bar_bg)),
        ])
    }

    fn separator(&self) -> RenderedItem {
        RenderedItem::empty()
    }

    fn background_fill(&self) -> Option<Style> {
        Some(Style::default().fg(self.theme.text).bg(self.theme.bar_bg))
    }
}
```

- [ ] **Step 2: Run existing segment_bar tests to verify nothing is broken**

Run: `cargo test -p flotilla-tui --lib segment_bar 2>&1 | tail -10`
Expected: all existing tests still pass

- [ ] **Step 3: Commit**

```bash
git add crates/flotilla-tui/src/segment_bar.rs
git commit -m "feat: add ThemedTabBarStyle and ThemedRibbonStyle for theme support"
```

---

### Task 6: Replace all hardcoded colours across ui.rs, ui_helpers.rs, and shimmer.rs

**Important:** This task modifies `ui_helpers.rs`, `shimmer.rs`, and `ui.rs` together as one atomic change. These files are interdependent — `ui.rs` calls `work_item_icon()` and `shimmer_spans()`, so their signatures must change in the same commit.

**Files:**
- Modify: `crates/flotilla-tui/src/ui_helpers.rs`
- Modify: `crates/flotilla-tui/src/shimmer.rs`
- Modify: `crates/flotilla-tui/src/ui.rs`

#### Part A: Update ui_helpers.rs

- [ ] **Step 1: Add &Theme parameter to work_item_icon**

Change the signature of `work_item_icon` to accept `&Theme` and use theme colours:

```rust
use crate::theme::Theme;

pub fn work_item_icon(kind: &WorkItemKind, has_workspace: bool, session_status: Option<&SessionStatus>, theme: &Theme) -> (&'static str, Color) {
    match kind {
        WorkItemKind::Checkout => {
            if has_workspace {
                ("●", theme.checkout)
            } else {
                ("○", theme.checkout)
            }
        }
        WorkItemKind::Session => match session_status {
            Some(SessionStatus::Running) => ("▶", theme.session),
            Some(SessionStatus::Idle) => ("◆", theme.session),
            _ => ("○", theme.session),
        },
        WorkItemKind::ChangeRequest => ("⊙", theme.change_request),
        WorkItemKind::RemoteBranch => ("⊶", theme.remote_branch),
        WorkItemKind::Issue => ("◇", theme.issue),
    }
}
```

- [ ] **Step 2: Fix tests that call work_item_icon**

Add `use crate::theme::Theme;` to the test module, then update each `work_item_icon` call to add `&Theme::classic()` as the last argument. Assertions remain unchanged — classic colours match old hardcoded values.

#### Part B: Update shimmer.rs

- [ ] **Step 3: Update Shimmer struct to store theme colours**

Make `Shimmer` store base/highlight colours and fallback instead of hardcoding them. Add a helper to extract RGB from a `Color`:

```rust
use crate::theme::Theme;

fn color_to_rgb(c: Color) -> (u8, u8, u8) {
    match c {
        Color::Rgb(r, g, b) => (r, g, b),
        Color::Yellow => (255, 240, 120),
        Color::Red => (255, 0, 0),
        Color::Green => (0, 255, 0),
        _ => (200, 200, 100),
    }
}

pub(crate) struct Shimmer {
    pos: f32,
    band_half_width: f32,
    true_color: bool,
    padding: usize,
    base: (u8, u8, u8),
    highlight: (u8, u8, u8),
    fallback_color: Color,
}
```

Update `new` and `new_at` to take `&Theme`:

```rust
impl Shimmer {
    pub fn new(total_width: usize, theme: &Theme) -> Self {
        Self::new_at(total_width, elapsed_since_start(), theme)
    }

    pub fn new_at(total_width: usize, elapsed: Duration, theme: &Theme) -> Self {
        let base = color_to_rgb(theme.shimmer_base);
        let highlight = color_to_rgb(theme.shimmer_highlight);
        let fallback_color = match theme.shimmer_highlight {
            Color::Rgb(_, _, _) => Color::Yellow,
            other => other,
        };
        let padding = 10usize;
        let period = total_width + padding * 2;
        let sweep_seconds = 2.0f32;
        let pos = (elapsed.as_secs_f32() % sweep_seconds) / sweep_seconds * period as f32;
        Self { pos, band_half_width: 5.0, true_color: has_true_color(), padding, base, highlight, fallback_color }
    }
}
```

Update `spans` to use `self.base`, `self.highlight`, `self.fallback_color` instead of the hardcoded values at lines 61-62 and 74-78.

Update `shimmer_spans` to take `&Theme`:

```rust
pub(crate) fn shimmer_spans(text: &str, theme: &Theme) -> Vec<Span<'static>> {
    Shimmer::new(text.chars().count(), theme).spans(text, 0)
}
```

- [ ] **Step 4: Fix shimmer tests**

Update test calls to pass `&Theme::classic()`. Behaviour is identical since classic reproduces the old shimmer colours.

#### Part C: Thread &Theme through ui.rs

This is the bulk — adding `theme: &Theme` to all render functions and replacing ~70 hardcoded `Color::*` references.

**Files:**
- Modify: `crates/flotilla-tui/src/ui.rs`

- [ ] **Step 1: Add `theme: &Theme` parameter to all render function signatures**

The public `render` function (line 90) becomes:

```rust
pub fn render(model: &TuiModel, ui: &mut UiState, in_flight: &HashMap<u64, InFlightCommand>, theme: &Theme, frame: &mut Frame) {
```

Every internal `render_*` function gains `theme: &Theme`. The call chain passes it through. Here is the full list of functions to update (current line numbers for reference):

| Function | Line | Signature change |
|----------|------|-----------------|
| `render` | 90 | Add `theme: &Theme` after `in_flight` |
| `render_tab_bar` | 107 | Add `theme: &Theme` after `frame` |
| `render_status_bar` | 219 | Add `theme: &Theme` after `frame` |
| `render_content` | 438 | Add `theme: &Theme` after `frame` |
| `render_repo_providers` | 464 | Add `theme: &Theme` after `frame` |
| `render_unified_table` | 488 | Add `theme: &Theme` after `frame` |
| `build_item_row` | 623 | Add `theme: &Theme` as last param |
| `render_preview` | 763 | Add `theme: &Theme` after `frame` |
| `render_preview_content` | 776 | Add `theme: &Theme` after `frame` |
| `render_debug_panel` | 861 | Add `theme: &Theme` after `frame` |
| `render_action_menu` | 876 | Add `theme: &Theme` after `frame` |
| `render_input_popup` | 899 | Add `theme: &Theme` after `frame` |
| `render_delete_confirm` | 923 | Add `theme: &Theme` after `frame` |
| `render_close_confirm` | 1016 | Add `theme: &Theme` after `frame` |
| `render_help` | 1037 | Add `theme: &Theme` after `frame` |
| `render_file_picker` | 1122 | Add `theme: &Theme` after `frame` |
| `render_config_screen` | 1174 | Add `theme: &Theme` after `frame` |
| `render_global_status` | 1204 | Add `theme: &Theme` after `frame` |
| `render_hosts_status` | 1251 | Add `theme: &Theme` after `frame` |
| `render_event_log` | 1270 | Add `theme: &Theme` after `frame` |

Add `use crate::theme::Theme;` to the imports at the top of `ui.rs`.

Update the call sites in `render()` (lines 96-104) to pass `theme`:

```rust
pub fn render(model: &TuiModel, ui: &mut UiState, in_flight: &HashMap<u64, InFlightCommand>, theme: &Theme, frame: &mut Frame) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0), Constraint::Length(1)])
        .split(frame.area());

    render_tab_bar(model, ui, frame, chunks[0], theme);
    render_content(model, ui, frame, chunks[1], theme);
    render_status_bar(model, ui, in_flight, frame, chunks[2], theme);
    render_action_menu(model, ui, frame, theme);
    render_input_popup(ui, frame, theme);
    render_delete_confirm(model, ui, frame, theme);
    render_close_confirm(model, ui, frame, theme);
    render_help(model, ui, frame, theme);
    render_file_picker(ui, frame, theme);
}
```

- [ ] **Step 2: Replace hardcoded colours in render_tab_bar (lines 107-170)**

Key replacements:
- Line 112-116: `flotilla_style` → `theme.logo_style(ui.mode.is_config())`
- Line 152: `Style::default().fg(Color::Green)` → `Style::default().fg(theme.status_ok)`
- Line 157: `segment_bar::TabBarStyle` → `segment_bar::ThemedTabBarStyle { theme }`

- [ ] **Step 3: Replace hardcoded colours in render_status_bar (lines 219-430)**

Key replacements:
- Lines 174-176: Provider status Green/Red/White → `theme.status_ok`/`theme.error`/`theme.text`
- Lines 199-201: DarkGray bold headers → `Style::default().fg(theme.muted).bold()`
- Line 234: `bg(Color::Black)` → `bg(theme.bar_bg)`
- Line 239: `Indexed(203)` → `theme.status_error`
- Line 240: `fg(Color::White).bg(Color::Black)` → `fg(theme.text).bg(theme.bar_bg)`
- Line 274: `segment_bar::RibbonStyle` → `segment_bar::ThemedRibbonStyle { theme }`
- All remaining `Color::White`/`Color::Black` in status bar → `theme.text`/`theme.bar_bg`

- [ ] **Step 4: Replace hardcoded colours in render_unified_table (lines 488-620)**

Key replacements:
- Line 516: `fg(Color::DarkGray).bold()` → `fg(theme.muted).bold()`
- Line 560: `bg(Color::Indexed(236))` → `bg(theme.multi_select_bg)`
- Line 571: `bg(Color::DarkGray).bold()` → `bg(theme.row_highlight).bold()`
- Line 587: `fg(Color::Yellow).bold()` → `theme.header_style()`

- [ ] **Step 5: Replace hardcoded colours in build_item_row (lines 623-761)**

Key replacements:
- Line 730: `fg(Color::Red)` → `fg(theme.error)`
- Line 750: `Color::Indexed(67)` → `theme.source`
- Line 751: `Color::Indexed(245)` → `theme.path`
- Line 753: `Color::Cyan` → `theme.branch`
- Line 754: `Color::Green` → `theme.checkout` (worktree indicator)
- Line 755: `Color::Green` → `theme.workspace`
- Line 756: `Color::Blue` → `theme.change_request`
- Line 757: `Color::Magenta` → `theme.session`
- Line 758: `Color::Yellow` → `theme.issue`
- Line 759: `Color::Red` → `theme.git_status`

Update `work_item_icon` call to pass `theme`.

- [ ] **Step 6: Replace hardcoded colours in render_preview_content (lines 776-860)**

Key replacements:
- Line 915: `fg(Color::Cyan)` → `fg(theme.input_text)`
- Lines 945-948: PR status colours → `theme.change_request_status_color(status)`
- Lines 968, 972, 991: `fg(Color::Red).bold()` → `fg(theme.error).bold()`
- Line 1004: `fg(Color::Green).bold()` → `fg(theme.status_ok).bold()`

- [ ] **Step 7: Replace hardcoded colours in render_action_menu (lines 876-898)**

Key replacement:
- Line 891: `bg(Color::Blue).bold()` → `bg(theme.action_highlight).bold()`

- [ ] **Step 8: Replace hardcoded colours in render_help (lines 1037-1120)**

Key replacements:
- Section header styles (`Style::default().bold()`) stay as-is — they use default fg, which is fine.
- The help screen content is text-only, no colour changes needed beyond the overlay block.

- [ ] **Step 9: Replace hardcoded colours in render_config_screen (lines 1174-1250)**

Key replacements:
- Line 1136: `fg(Color::Cyan)` → `fg(theme.input_text)`
- Lines 1155-1159: `Color::Green`/`Color::DarkGray` → `theme.status_ok`/`theme.muted`
- Line 1165: `bg(Color::DarkGray).bold()` → `bg(theme.row_highlight).bold()`

- [ ] **Step 10: Replace hardcoded colours in render_event_log (lines 1270-1330)**

Key replacements:
- Lines 1292-1296: Level colours → `theme.log_level_style(level)`
- Line 1300: Timestamp `DarkGray` → `theme.muted`
- Line 1307: Text `DarkGray` → `theme.muted`
- Line 1321: Title `DarkGray` → `theme.muted`
- Line 1323: Highlight `Indexed(236)` → `bg(theme.multi_select_bg)`

- [ ] **Step 11: Replace hardcoded colours in render_hosts_status (lines 1251-1268)**

Key replacements:
- Lines 1256-1260: Connection status colours → `theme.status_ok`/`theme.error`/`theme.warning`

- [ ] **Step 12: Replace hardcoded colours in remaining popups**

`render_input_popup`, `render_delete_confirm`, `render_close_confirm`, `render_file_picker`:
- DarkGray text → `theme.muted`
- Green confirmations → `theme.status_ok`
- Red warnings → `theme.error`

- [ ] **Step 13: Update shimmer_spans call sites**

All calls to `shimmer_spans(text)` in ui.rs become `shimmer_spans(text, theme)`. Similarly `Shimmer::new(width)` becomes `Shimmer::new(width, theme)`.

- [ ] **Step 14: Verify it compiles (expect partial failure)**

Run: `cargo check -p flotilla-tui 2>&1 | tail -20`
Expected: compilation errors from test harness (`tests/support/mod.rs`) and `run.rs` — they still pass the old `ui::render` signature without `&Theme`. The main library code should compile. These callers are fixed in subsequent tasks.

- [ ] **Step 15: Commit all three files together**

```bash
git add crates/flotilla-tui/src/ui.rs crates/flotilla-tui/src/ui_helpers.rs crates/flotilla-tui/src/shimmer.rs
git commit -m "refactor: replace all hardcoded colours with Theme fields across ui.rs, ui_helpers, shimmer"
```

---

## Chunk 3: App integration and wiring

### Task 7: Store Theme on App, update run.rs call site

**Files:**
- Modify: `crates/flotilla-tui/src/app/mod.rs`
- Modify: `crates/flotilla-tui/src/run.rs`

- [ ] **Step 1: Add `theme` field to App**

In `app/mod.rs`, add to the `App` struct:

```rust
use crate::theme::Theme;

pub struct App {
    // ... existing fields ...
    pub theme: Theme,
}
```

Update `App::new` to accept a `Theme` parameter and store it:

```rust
pub fn new(daemon: Arc<dyn DaemonHandle>, repos_info: Vec<RepoInfo>, config: Arc<ConfigStore>, theme: Theme) -> Self {
    // ... existing code ...
    Self {
        daemon,
        config,
        model,
        ui,
        proto_commands: Default::default(),
        in_flight: HashMap::new(),
        pending_cancel: None,
        should_quit: false,
        theme,
    }
}
```

- [ ] **Step 2: Update run.rs to pass &app.theme to ui::render**

Change both `ui::render` call sites (lines 36 and 233) from:

```rust
ui::render(&app.model, &mut app.ui, &app.in_flight, f)
```

to:

```rust
ui::render(&app.model, &mut app.ui, &app.in_flight, &app.theme, f)
```

- [ ] **Step 3: Update src/main.rs to pass Theme to App::new**

Find the `App::new(...)` call in `src/main.rs` and add `Theme::catppuccin_mocha()` as the last argument. Add the import: `use flotilla_tui::theme::Theme;`.

Note: This is a temporary placeholder. Task 9 replaces this with config-aware theme resolution (`--theme` CLI arg + config file).

- [ ] **Step 4: Verify it compiles**

Run: `cargo check 2>&1 | tail -20`
Expected: may still have test compilation errors, but the main binary should compile

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-tui/src/app/mod.rs crates/flotilla-tui/src/run.rs src/main.rs
git commit -m "feat: store Theme on App, pass to render pipeline"
```

---

### Task 8: Add CycleTheme keybinding

**Files:**
- Modify: `crates/flotilla-tui/src/app/key_handlers.rs`

- [ ] **Step 1: Add CycleTheme to the Action enum**

Add `CycleTheme` variant to the `Action` enum (after `CycleLayout`):

```rust
CycleTheme,
```

- [ ] **Step 2: Add keybinding in resolve_action**

Add after the `CycleLayout` binding (line 87):

```rust
KeyCode::Char('T') if in_work_item_table => Some(Action::CycleTheme),
```

Note: uppercase `T` (Shift+T) to avoid conflicting with lowercase `t`.

- [ ] **Step 3: Add dispatch handler**

Add after the `CycleLayout` handler (line ~272):

```rust
Action::CycleTheme => {
    let themes = crate::theme::available_themes();
    let current = self.theme.name;
    let idx = themes.iter().position(|f| (f)().name == current).unwrap_or(0);
    let next = (idx + 1) % themes.len();
    self.theme = (themes[next])();
}
```

- [ ] **Step 4: Run clippy**

Run: `cargo clippy -p flotilla-tui --all-targets --locked -- -D warnings 2>&1 | tail -10`
Expected: no errors

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-tui/src/app/key_handlers.rs
git commit -m "feat: add T keybinding to cycle through themes at runtime"
```

---

### Task 9: Add --theme CLI arg and config field

**Files:**
- Modify: `src/main.rs`
- Modify: `crates/flotilla-core/src/config.rs`

- [ ] **Step 1: Add theme field to UiConfig**

In `crates/flotilla-core/src/config.rs`, add to `UiConfig`:

```rust
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct UiConfig {
    #[serde(default)]
    pub preview: PreviewConfig,
    #[serde(default)]
    pub theme: Option<String>,
}
```

- [ ] **Step 2: Add --theme CLI arg**

In `src/main.rs`, add to the `Cli` struct:

```rust
/// Theme name (catppuccin, classic)
#[arg(long)]
theme: Option<String>,
```

- [ ] **Step 3: Resolve theme in main**

Where `App::new` is called, resolve the theme:

```rust
use flotilla_tui::theme;

let theme_name = cli.theme
    .or_else(|| config.load_config().ui.theme.clone())
    .unwrap_or_else(|| "catppuccin".to_string());
let initial_theme = theme::theme_by_name(&theme_name);
```

Pass `initial_theme` to `App::new(...)`.

- [ ] **Step 4: Verify it compiles and runs**

Run: `cargo build 2>&1 | tail -5`
Expected: successful build

- [ ] **Step 5: Commit**

```bash
git add src/main.rs crates/flotilla-core/src/config.rs
git commit -m "feat: add --theme CLI arg and config field for initial theme selection"
```

---

## Chunk 4: Testing and cleanup

### Task 10: Update test harness and snapshot tests

**Files:**
- Modify: `crates/flotilla-tui/tests/support/mod.rs`
- Modify: `crates/flotilla-tui/tests/snapshots.rs`

- [ ] **Step 1: Update TestHarness to use Theme**

In `tests/support/mod.rs`, update `render_to_buffer` to pass `&Theme::classic()`:

```rust
use flotilla_tui::theme::Theme;

impl TestHarness {
    pub fn render_to_buffer(&mut self) -> Buffer {
        let backend = TestBackend::new(self.width, self.height);
        let mut terminal = Terminal::new(backend).unwrap();
        let theme = Theme::classic();
        terminal
            .draw(|frame| {
                ui::render(&self.model, &mut self.ui, &self.in_flight, &theme, frame);
            })
            .unwrap();
        terminal.backend().buffer().clone()
    }
}
```

Add a `with_theme` builder method for tests that need a specific theme:

```rust
pub fn with_theme(mut self, theme: Theme) -> Self {
    self.theme = Some(theme);
    self
}
```

This requires adding `theme: Option<Theme>` to `TestHarness` and using it in `render_to_buffer`:

```rust
pub struct TestHarness {
    pub model: TuiModel,
    pub ui: UiState,
    pub in_flight: HashMap<u64, InFlightCommand>,
    width: u16,
    height: u16,
    theme: Option<Theme>,
}
```

Update all constructors (`empty`, `single_repo`, `multi_repo`) to set `theme: None`. In `render_to_buffer`, use `self.theme.clone().unwrap_or_else(Theme::classic)`.

Using `Theme::classic()` as default keeps existing snapshot tests unchanged.

- [ ] **Step 2: Run all snapshot tests**

Run: `cargo test -p flotilla-tui --test snapshots 2>&1 | tail -20`
Expected: all tests pass (classic theme reproduces old colours, snapshots only capture text content)

- [ ] **Step 3: Add theme switching test**

In `tests/snapshots.rs`, add:

```rust
#[test]
fn theme_switching_changes_output() {
    use flotilla_tui::theme::Theme;
    use ratatui::style::Color;

    let providers = ProviderData::default();
    let (path, checkout) = make_checkout("feat-login", "/test/my-project/feat-login", false);
    let mut providers = providers;
    providers.checkouts.insert(path, checkout);
    let items = vec![make_work_item_checkout("feat-login", "/test/my-project/feat-login")];

    let mut classic_harness =
        TestHarness::single_repo("my-project")
            .with_provider_data(providers.clone(), items.clone())
            .with_theme(Theme::classic());
    let classic_buf = classic_harness.render_to_buffer();

    let mut catppuccin_harness =
        TestHarness::single_repo("my-project")
            .with_provider_data(providers, items)
            .with_theme(Theme::catppuccin_mocha());
    let catppuccin_buf = catppuccin_harness.render_to_buffer();

    // Find a cell that uses a themed colour by scanning for the checkout icon "○".
    // Classic uses Color::Green, catppuccin uses a different RGB green.
    let area = classic_buf.area;
    let mut classic_fg = None;
    let mut catppuccin_fg = None;
    for y in area.y..area.y + area.height {
        for x in area.x..area.x + area.width {
            if classic_buf[(x, y)].symbol() == "○" {
                classic_fg = Some(classic_buf[(x, y)].fg);
                catppuccin_fg = Some(catppuccin_buf[(x, y)].fg);
                break;
            }
        }
        if classic_fg.is_some() {
            break;
        }
    }
    let classic_fg = classic_fg.expect("should find checkout icon in classic render");
    let catppuccin_fg = catppuccin_fg.expect("should find checkout icon in catppuccin render");
    assert_ne!(classic_fg, catppuccin_fg,
        "Themes should produce different colours: classic={classic_fg:?} vs catppuccin={catppuccin_fg:?}");
}
```

- [ ] **Step 4: Run the new test**

Run: `cargo test -p flotilla-tui --test snapshots theme_switching 2>&1 | tail -10`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-tui/tests/support/mod.rs crates/flotilla-tui/tests/snapshots.rs
git commit -m "test: update harness for Theme, add theme switching test"
```

---

### Task 11: Update help screen with theme keybinding

**Files:**
- Modify: `crates/flotilla-tui/src/ui.rs`

- [ ] **Step 1: Add T keybinding to help text**

In `render_help`, find the "General" section (around line 1088) and add:

```rust
Line::from(vec![
    Span::styled("  T", Style::default().bold()),
    Span::raw(format!("  Cycle theme (current: {})", theme.name)),
]),
```

- [ ] **Step 2: Run help screen snapshot test**

Run: `cargo test -p flotilla-tui --test snapshots help_screen 2>&1 | tail -10`
Expected: FAIL (snapshot mismatch due to new help text)

- [ ] **Step 3: Update the snapshot**

Run: `cargo test -p flotilla-tui --test snapshots help_screen -- --update-snapshots 2>&1 | tail -5`

Or delete the old snapshot and re-run to generate the new one:
Run: `INSTA_UPDATE=new cargo test -p flotilla-tui --test snapshots help_screen 2>&1 | tail -5`

Review the updated snapshot to confirm the new `T` line appears.

- [ ] **Step 4: Commit**

```bash
git add crates/flotilla-tui/src/ui.rs crates/flotilla-tui/tests/snapshots/
git commit -m "feat: show theme name and T keybinding in help screen"
```

---

### Task 12: Final verification

- [ ] **Step 1: Run full test suite**

Run: `cargo test --locked 2>&1 | tail -20`
Expected: all tests pass

- [ ] **Step 2: Run clippy**

Run: `cargo clippy --all-targets --locked -- -D warnings 2>&1 | tail -10`
Expected: no warnings

- [ ] **Step 3: Run formatter**

Run: `cargo +nightly-2026-03-12 fmt 2>&1`
Expected: clean (or auto-formatted)

- [ ] **Step 4: Commit any formatting changes**

```bash
git add -A
git commit -m "chore: rustfmt"
```

- [ ] **Step 5: Manual smoke test**

Run: `cargo run` and verify:
1. The TUI renders with catppuccin Mocha colours
2. Press `T` — colours switch to classic (familiar old palette)
3. Press `T` again — back to catppuccin
4. Run `cargo run -- --theme classic` — starts with the classic palette
