# SegmentBar Widget Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Extract a shared `SegmentBar` widget for rendering horizontal rows of clickable segments, used by both the tab bar and status bar key ribbons.

**Architecture:** A `BarStyle` trait provides visual decisions (separators, colors, item rendering). `SegmentBar::render()` is a plain function that renders segments into a buffer and returns hit regions. Two concrete styles (`TabBarStyle`, `RibbonStyle`) match the current visuals. Callers map hit region indices to their domain actions.

**Tech Stack:** Rust, ratatui 0.30, unicode-width

**Spec:** `docs/superpowers/specs/2026-03-14-segment-bar-widget-design.md`

---

## Chunk 1: Core types and TabBarStyle

### Task 1: Core types and BarStyle trait

**Files:**
- Create: `crates/flotilla-tui/src/segment_bar.rs`
- Modify: `crates/flotilla-tui/src/lib.rs`

- [ ] **Step 1: Write unit test for RenderedItem width**

In `segment_bar.rs`, add:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rendered_item_from_span() {
        let item = RenderedItem::from_spans(vec![
            Span::raw("hello"),
            Span::raw(" world"),
        ]);
        assert_eq!(item.width, 11);
        assert_eq!(item.spans.len(), 2);
    }
}
```

- [ ] **Step 2: Write the core types**

```rust
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::Style,
    text::Span,
};
use unicode_width::UnicodeWidthStr;

/// Client-provided data for one segment in a bar.
pub struct SegmentItem {
    pub label: String,
    pub key_hint: Option<String>,
    pub active: bool,
    pub dragging: bool,
    pub style_override: Option<Style>,
}

/// Bundles rendered spans with their computed display width.
pub struct RenderedItem {
    pub spans: Vec<Span<'static>>,
    pub width: usize,
}

impl RenderedItem {
    pub fn from_spans(spans: Vec<Span<'static>>) -> Self {
        let width = spans.iter().map(|s| s.content.as_ref().width()).sum();
        Self { spans, width }
    }

    pub fn empty() -> Self {
        Self { spans: vec![], width: 0 }
    }
}

/// A clickable region produced by rendering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HitRegion {
    pub area: Rect,
    pub index: usize,
}

/// Provides all visual decisions for a segment bar.
pub trait BarStyle {
    /// Render a single segment item into styled spans.
    fn render_item(&self, item: &SegmentItem) -> RenderedItem;

    /// Separator between segments (empty if separators are part of items).
    fn separator(&self) -> RenderedItem;

    /// Style for filling unused width. None = don't fill.
    fn background_fill(&self) -> Option<Style>;
}
```

- [ ] **Step 3: Register module in lib.rs**

Add `pub mod segment_bar;` to `crates/flotilla-tui/src/lib.rs`.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p flotilla-tui -- segment_bar`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-tui/src/segment_bar.rs crates/flotilla-tui/src/lib.rs
git commit -m "feat: add segment_bar core types and BarStyle trait"
```

### Task 2: SegmentBar::render function

**Files:**
- Modify: `crates/flotilla-tui/src/segment_bar.rs`

- [ ] **Step 1: Write unit test for render**

```rust
#[test]
fn render_produces_hit_regions() {
    let items = vec![
        SegmentItem { label: "Alpha".into(), key_hint: None, active: true, dragging: false, style_override: None },
        SegmentItem { label: "Beta".into(), key_hint: None, active: false, dragging: false, style_override: None },
    ];

    struct TestStyle;
    impl BarStyle for TestStyle {
        fn render_item(&self, item: &SegmentItem) -> RenderedItem {
            RenderedItem::from_spans(vec![Span::raw(item.label.clone())])
        }
        fn separator(&self) -> RenderedItem {
            RenderedItem::from_spans(vec![Span::raw(" | ")])
        }
        fn background_fill(&self) -> Option<Style> { None }
    }

    let mut buf = Buffer::empty(Rect::new(0, 0, 40, 1));
    let hits = render(&items, &TestStyle, Rect::new(0, 0, 40, 1), &mut buf);

    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].index, 0);
    assert_eq!(hits[0].area, Rect::new(0, 0, 5, 1)); // "Alpha"
    assert_eq!(hits[1].index, 1);
    assert_eq!(hits[1].area, Rect::new(8, 0, 4, 1)); // " | " then "Beta"
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p flotilla-tui -- segment_bar::tests::render_produces_hit_regions`
Expected: FAIL — `render` function not found

- [ ] **Step 3: Implement render function**

```rust
/// Render a segment bar into the buffer and return hit regions.
pub fn render(items: &[SegmentItem], style: &dyn BarStyle, area: Rect, buf: &mut Buffer) -> Vec<HitRegion> {
    let mut hits = Vec::with_capacity(items.len());
    let mut x = area.x;
    let max_x = area.x + area.width;

    for (i, item) in items.iter().enumerate() {
        // Separator before all items except the first
        if i > 0 {
            let sep = style.separator();
            for span in &sep.spans {
                let w = span.content.as_ref().width() as u16;
                if x + w > max_x { break; }
                buf.set_span(x, area.y, span, w);
                x += w;
            }
        }

        let rendered = style.render_item(item);
        let item_start = x;
        for span in &rendered.spans {
            let w = span.content.as_ref().width() as u16;
            if x + w > max_x { break; }
            buf.set_span(x, area.y, span, w);
            x += w;
        }
        let item_end = x;
        if item_end > item_start {
            hits.push(HitRegion {
                area: Rect::new(item_start, area.y, item_end - item_start, 1),
                index: i,
            });
        }
    }

    // Fill remaining width with background
    if let Some(bg_style) = style.background_fill() {
        while x < max_x {
            buf[(x, area.y)].set_style(bg_style);
            buf[(x, area.y)].set_symbol(" ");
            x += 1;
        }
    }

    hits
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p flotilla-tui -- segment_bar`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-tui/src/segment_bar.rs
git commit -m "feat: implement SegmentBar::render with hit regions"
```

### Task 3: TabBarStyle

**Files:**
- Modify: `crates/flotilla-tui/src/segment_bar.rs`

- [ ] **Step 1: Write unit test for TabBarStyle**

```rust
#[test]
fn tab_style_renders_active_and_inactive() {
    let style = TabBarStyle;
    let active = SegmentItem { label: "active".into(), key_hint: None, active: true, dragging: false, style_override: None };
    let inactive = SegmentItem { label: "inactive".into(), key_hint: None, active: false, dragging: false, style_override: None };

    let a = style.render_item(&active);
    let i = style.render_item(&inactive);

    assert_eq!(a.width, 6);
    assert_eq!(a.spans.len(), 1);
    assert_eq!(i.width, 8);
    assert_eq!(i.spans.len(), 1);
    // Active should be bold cyan
    assert!(a.spans[0].style.add_modifier.contains(ratatui::style::Modifier::BOLD));
}

#[test]
fn tab_style_applies_style_override() {
    let style = TabBarStyle;
    let item = SegmentItem {
        label: "[+]".into(), key_hint: None, active: false, dragging: false,
        style_override: Some(Style::default().fg(Color::Green)),
    };
    let rendered = style.render_item(&item);
    assert_eq!(rendered.spans[0].style.fg, Some(Color::Green));
}

#[test]
fn tab_style_separator() {
    let sep = TabBarStyle.separator();
    assert_eq!(sep.width, 3);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p flotilla-tui -- segment_bar::tests::tab_style`
Expected: FAIL — `TabBarStyle` not found

- [ ] **Step 3: Implement TabBarStyle**

```rust
use ratatui::style::{Color, Modifier};

/// Tab bar style: pipe separators, cyan active, dark gray inactive.
pub struct TabBarStyle;

impl BarStyle for TabBarStyle {
    fn render_item(&self, item: &SegmentItem) -> RenderedItem {
        let style = if let Some(override_style) = item.style_override {
            override_style
        } else if item.active && item.dragging {
            Style::default().bold().fg(Color::Cyan).add_modifier(Modifier::UNDERLINED)
        } else if item.active {
            Style::default().bold().fg(Color::Cyan)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        RenderedItem::from_spans(vec![Span::styled(item.label.clone(), style)])
    }

    fn separator(&self) -> RenderedItem {
        RenderedItem::from_spans(vec![Span::styled(" | ", Style::default().fg(Color::DarkGray))])
    }

    fn background_fill(&self) -> Option<Style> {
        None
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p flotilla-tui -- segment_bar`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-tui/src/segment_bar.rs
git commit -m "feat: add TabBarStyle for pipe-separated tab rendering"
```

## Chunk 2: RibbonStyle and integration

### Task 4: RibbonStyle

**Files:**
- Modify: `crates/flotilla-tui/src/segment_bar.rs`

- [ ] **Step 1: Write unit test for RibbonStyle**

```rust
#[test]
fn ribbon_style_renders_with_key_hint() {
    let style = RibbonStyle;
    let item = SegmentItem {
        label: "OPEN".into(), key_hint: Some("ENT".into()),
        active: false, dragging: false, style_override: None,
    };
    let rendered = style.render_item(&item);
    // Leading chevron + " " + "<" + key + ">" + " LABEL " + trailing chevron
    assert!(rendered.spans.len() >= 5);
    // Width should match KeyChip::ribbon_width logic
    let text: String = rendered.spans.iter().map(|s| s.content.as_ref().to_string()).collect();
    assert!(text.contains("ENT"));
    assert!(text.contains("OPEN"));
}

#[test]
fn ribbon_style_separator_is_empty() {
    let sep = RibbonStyle.separator();
    assert_eq!(sep.width, 0);
}

#[test]
fn ribbon_style_fills_background() {
    assert!(RibbonStyle.background_fill().is_some());
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p flotilla-tui -- segment_bar::tests::ribbon_style`
Expected: FAIL — `RibbonStyle` not found

- [ ] **Step 3: Implement RibbonStyle**

```rust
/// Chevron separator glyph used in ribbon-style bars.
const CHEVRON: &str = "\u{e0b0}"; //

/// Status bar ribbon style: chevron-delimited key chips.
pub struct RibbonStyle;

impl BarStyle for RibbonStyle {
    fn render_item(&self, item: &SegmentItem) -> RenderedItem {
        let key = item.key_hint.as_deref().unwrap_or("");
        let label = &item.label;

        let spans = vec![
            Span::styled(CHEVRON, Style::default().fg(Color::Black).bg(Color::DarkGray)),
            Span::styled(" ", Style::default().fg(Color::Black).bg(Color::DarkGray)),
            Span::styled("<", Style::default().fg(Color::Black).bg(Color::DarkGray).bold()),
            Span::styled(key.to_string(), Style::default().fg(Color::Indexed(208)).bg(Color::DarkGray).bold()),
            Span::styled(">", Style::default().fg(Color::Black).bg(Color::DarkGray).bold()),
            Span::styled(format!(" {label} "), Style::default().fg(Color::Black).bg(Color::DarkGray).bold()),
            Span::styled(CHEVRON, Style::default().fg(Color::DarkGray).bg(Color::Black)),
        ];
        RenderedItem::from_spans(spans)
    }

    fn separator(&self) -> RenderedItem {
        RenderedItem::empty()
    }

    fn background_fill(&self) -> Option<Style> {
        Some(Style::default().fg(Color::White).bg(Color::Black))
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p flotilla-tui -- segment_bar`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-tui/src/segment_bar.rs
git commit -m "feat: add RibbonStyle for chevron key chip rendering"
```

### Task 5: Integrate SegmentBar into tab bar rendering

**Files:**
- Modify: `crates/flotilla-tui/src/ui.rs`

- [ ] **Step 1: Refactor render_tab_bar to use SegmentBar**

Replace the body of `render_tab_bar` with:

```rust
fn render_tab_bar(model: &TuiModel, ui: &mut UiState, frame: &mut Frame, area: Rect) {
    // Build segment items
    let mut items = Vec::new();
    let mut tab_ids = Vec::new();

    // Flotilla logo tab
    let flotilla_style = if ui.mode.is_config() {
        Style::default().bold().fg(Color::Black).bg(Color::White)
    } else {
        Style::default().bold().fg(Color::Black).bg(Color::Cyan)
    };
    items.push(segment_bar::SegmentItem {
        label: TabId::FLOTILLA_LABEL.to_string(),
        key_hint: None,
        active: ui.mode.is_config(),
        dragging: false,
        style_override: Some(flotilla_style),
    });
    tab_ids.push(TabId::Flotilla);

    // Repo tabs
    for (i, path) in model.repo_order.iter().enumerate() {
        let rm = &model.repos[path];
        let rui = &ui.repo_ui[path];
        let name = TuiModel::repo_name(path);
        let is_active = !ui.mode.is_config() && i == model.active_repo;
        let loading = if rm.loading { " ⟳" } else { "" };
        let changed = if rui.has_unseen_changes { "*" } else { "" };
        let label = format!("{name}{changed}{loading}");

        items.push(segment_bar::SegmentItem {
            label,
            key_hint: None,
            active: is_active,
            dragging: is_active && ui.drag.active,
            style_override: None,
        });
        tab_ids.push(TabId::Repo(i));
    }

    // [+] button
    items.push(segment_bar::SegmentItem {
        label: "[+]".to_string(),
        key_hint: None,
        active: false,
        dragging: false,
        style_override: Some(Style::default().fg(Color::Green)),
    });
    tab_ids.push(TabId::Add);

    // Render
    let hits = segment_bar::render(&items, &segment_bar::TabBarStyle, area, frame.buffer_mut());

    // Map hit regions to tab areas
    ui.layout.tab_areas.clear();
    for hit in hits {
        if let Some(tab_id) = tab_ids.get(hit.index) {
            ui.layout.tab_areas.insert(tab_id.clone(), hit.area);
        }
    }
}
```

- [ ] **Step 2: Add segment_bar import to ui.rs**

Add `use crate::segment_bar;` to the imports.

- [ ] **Step 3: Run clippy and tests**

Run: `cargo clippy --all-targets --locked -- -D warnings && cargo test --locked`
Expected: PASS (snapshot tests may need updating)

- [ ] **Step 4: Update any failing snapshots**

If snapshot tests fail due to minor rendering differences:
Run: `cargo insta review` or update snapshot files.
The visual output should be identical or negligibly different.

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-tui/src/ui.rs
git commit -m "refactor: use SegmentBar for tab bar rendering"
```

### Task 6: Integrate SegmentBar into status bar key ribbons

**Files:**
- Modify: `crates/flotilla-tui/src/ui.rs`

- [ ] **Step 1: Refactor the key ribbon section of render_status_bar**

Replace the key chip loop (the `for chip in &status_model.visible_keys` block) with:

```rust
    // Render key ribbons via SegmentBar
    if !status_model.visible_keys.is_empty() {
        let ribbon_items: Vec<segment_bar::SegmentItem> = status_model.visible_keys.iter().map(|chip| {
            segment_bar::SegmentItem {
                label: chip.label.clone(),
                key_hint: Some(chip.key.clone()),
                active: false,
                dragging: false,
                style_override: None,
            }
        }).collect();

        let ribbon_area = Rect::new(
            area.x + status_model.keys_start as u16,
            area.y,
            (status_model.task_start.saturating_sub(status_model.keys_start)) as u16,
            1,
        );
        let hits = segment_bar::render(&ribbon_items, &segment_bar::RibbonStyle, ribbon_area, frame.buffer_mut());

        for hit in hits {
            if let Some(chip) = status_model.visible_keys.get(hit.index) {
                ui.layout.status_bar.key_targets.push(StatusBarTarget::new(hit.area, chip.action.clone()));
            }
        }
    }
```

Also remove the manual span building for key chips (the `spans.push(Span::styled(CHEVRON_SEPARATOR, ...))` block and the `x += chip.ribbon_width()` tracking for key ribbons). The status section and task section span building stay.

- [ ] **Step 2: Remove unused CHEVRON_SEPARATOR import if no longer needed**

Check if `CHEVRON_SEPARATOR` is still used elsewhere in `ui.rs`. If not, remove from imports.

- [ ] **Step 3: Run clippy and tests**

Run: `cargo clippy --all-targets --locked -- -D warnings && cargo test --locked`
Expected: PASS (snapshot tests may need updating)

- [ ] **Step 4: Update any failing snapshots**

If snapshot tests fail, review diffs for visual correctness and update.

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-tui/src/ui.rs
git commit -m "refactor: use SegmentBar for status bar key ribbons"
```

### Task 7: Cleanup and final verification

**Files:**
- Modify: `crates/flotilla-tui/src/ui.rs` (remove dead code)
- Modify: `crates/flotilla-tui/src/segment_bar.rs` (formatting)

- [ ] **Step 1: Remove dead imports and unused code**

Check for unused imports in `ui.rs` related to the old tab bar / ribbon rendering (e.g. `CHEVRON_SEPARATOR` from status_bar, any unused `Span` construction helpers).

- [ ] **Step 2: Run full check suite**

```bash
cargo +nightly fmt
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
```

Expected: all pass, no warnings.

- [ ] **Step 3: Commit**

```bash
git add -A
git commit -m "refactor: clean up dead code after segment bar extraction"
```
