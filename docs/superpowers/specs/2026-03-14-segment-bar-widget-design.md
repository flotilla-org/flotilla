# SegmentBar Widget Design

## Goal

Extract a shared `SegmentBar` widget that renders a horizontal row of clickable segments with hit regions, used by both the tab bar and the status bar key ribbons. Visual style (separators, colors, capitalization) is provided by a `BarStyle` trait so both bars use the same rendering loop and styles can be swapped in future.

## Architecture

The widget is purely visual + hit-region. It does not know about `TabId`, `StatusBarAction`, or `KeyCode`. Callers build `SegmentItem`s from their domain data, render with a chosen `BarStyle`, and map returned hit regions back to their own action types.

### Core types

**`SegmentItem`** — client-provided data for one segment:
- `label: String` — display text
- `key_hint: Option<String>` — optional key label (e.g. `"q"`, `"ENT"`)
- `active: bool` — whether this segment is visually highlighted
- `dragging: bool` — whether this segment is being dragged (adds underline in tab style)
- `style_override: Option<Style>` — optional per-item ratatui `Style` override for one-offs like the `[+]` button (green fg) or the flotilla logo (cyan bg). When set, replaces the style from `BarStyle::item_style()` entirely.

**`BarStyle` trait** — provides all visual decisions. The rendering loop queries this rather than branching on style variants:
- `item_style(&self, active: bool, dragging: bool) -> Style` — default style for a segment label
- `render_item(&self, item: &SegmentItem) -> RenderedItem` — renders one segment into spans with its computed width. Returns a `RenderedItem { spans: Vec<Span>, width: usize }` to keep spans and width in sync (avoids the bug where `item_width` and `render_item` disagree).
- `separator(&self) -> RenderedItem` — separator between segments (styled spans + width). Returns empty for styles where separators are part of the item rendering (e.g. ribbon chevrons are baked into `render_item`).
- `background_fill(&self) -> Option<Style>` — `Some(style)` to pad remaining width with background, `None` to leave unfilled.

**`RenderedItem`** — bundles spans with their width:
- `spans: Vec<Span<'static>>` — styled text fragments
- `width: usize` — display width of all spans combined

**`HitRegion`** — output from rendering:
- `area: Rect` — clickable rectangle
- `index: usize` — which segment was hit

**`SegmentBar`** — the widget. Does not implement ratatui's `Widget` trait (which consumes `self` and cannot return hit regions). Instead provides a plain method:
- `fn render(items: &[SegmentItem], style: &dyn BarStyle, area: Rect, buf: &mut Buffer) -> Vec<HitRegion>` — renders all segments and returns hit regions. This matches the existing pattern in `ui.rs` where render functions are plain functions writing to the frame's buffer.

### Concrete styles

**`TabBarStyle`** — matches current tab bar:
- Separator: `" | "` in `DarkGray`
- Active items: bold cyan; dragging adds underline
- Inactive items: dark gray
- `render_item`: emits the label as a single span with `item_style()`, applying `style_override` when present
- No background fill

**`RibbonStyle`** — matches current status bar key ribbons:
- No separator between items (chevrons are part of the item)
- `render_item`: emits leading chevron + ` <key> LABEL ` + trailing chevron as multiple spans with the current color scheme (dark gray bg, orange key hint, black fg, chevron color transitions)
- Background: fills with black

### Integration points

**Tab bar (`render_tab_bar` in `ui.rs`):**
1. Builds `SegmentItem`s: flotilla logo (active when config mode, `style_override` for bg color), repo tabs (active when selected, `dragging` when being dragged), `[+]` button (green fg style override)
2. Renders with `TabBarStyle`
3. Maps hit regions to `tab_areas: BTreeMap<TabId, Rect>` — caller maintains a parallel `Vec<TabId>` to map `HitRegion::index` to `TabId`
4. Drag-to-reorder logic stays in the caller (it's behaviour, not rendering)
5. `TabId::Gear` stays outside the widget — it is rendered by `render_unified_table`, not the tab bar

**Status bar key ribbons (`render_status_bar` in `ui.rs`):**
1. Builds `SegmentItem`s from `KeyChip`s in `status_model.visible_keys`
2. Renders with `RibbonStyle` into the key ribbon zone (between `keys_start` and `task_start`)
3. Maps hit regions to `key_targets: Vec<StatusBarTarget>`
4. Status section and task section rendering stay outside the widget

### What stays outside the widget

- **Status bar three-zone layout** (`StatusBarModel::build` in `status_bar.rs`) — budget allocation between status/keys/task sections is higher-level layout, not segment rendering
- **Drag-to-reorder** — tab-specific behaviour that uses hit regions but adds mouse-move tracking
- **Loading/unseen indicators** (`⟳`, `*`) — baked into the segment label by the caller before passing to the widget
- **Shimmer animation** — applied to task text, not to segments
- **Overflow handling** — `SegmentBar` renders all items it is given without truncation. Callers handle overflow (status bar pops keys via `StatusBarModel::build`, tab bar currently does not truncate)

## File structure

- Create: `crates/flotilla-tui/src/segment_bar.rs` — trait, types, widget, two concrete styles
- Modify: `crates/flotilla-tui/src/lib.rs` — register module
- Modify: `crates/flotilla-tui/src/ui.rs` — refactor `render_tab_bar` and key ribbon section of `render_status_bar` to use `SegmentBar`
- Test: `crates/flotilla-tui/src/segment_bar.rs` (unit tests) + existing snapshot tests verify no visual regression

## Testing strategy

- Unit tests for `SegmentBar::render`: render into a test buffer, assert hit region positions and segment content
- Unit tests for each `BarStyle`: verify `render_item` output spans and width consistency
- Existing snapshot tests (`tests/snapshots.rs`) catch any visual regression in tab bar and status bar rendering — update snapshots after refactor
