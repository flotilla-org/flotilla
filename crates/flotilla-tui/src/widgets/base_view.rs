use std::{any::Any, time::Instant};

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    Frame,
};

use super::{
    event_log::EventLogWidget, preview_panel::PreviewPanel, status_bar_widget::StatusBarWidget, tab_bar::TabBar,
    work_item_table::WorkItemTable, AppAction, InteractiveWidget, Outcome, RenderContext, WidgetContext,
};
use crate::{
    app::{
        ui_state::{DragState, UiMode},
        RepoViewLayout, TabId, TuiModel, UiState,
    },
    keymap::{Action, ModeId},
    status_bar::StatusBarAction,
    theme::Theme,
    ui_helpers,
};

// ── Preview layout constants ──

const PREVIEW_SPLIT_RIGHT_PERCENT: u16 = 40;
const PREVIEW_SPLIT_BELOW_PERCENT: u16 = 40;
const MIN_TABLE_WIDTH: u16 = 50;
const MIN_PREVIEW_WIDTH: u16 = 32;
const MIN_TABLE_HEIGHT: u16 = 8;
const MIN_PREVIEW_HEIGHT: u16 = 6;
const PREVIEW_BELOW_ASPECT_RATIO_THRESHOLD: f32 = 2.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ResolvedPreviewPosition {
    Right,
    Below,
}

fn resolve_preview_position(area: Rect, layout: RepoViewLayout) -> Option<ResolvedPreviewPosition> {
    match layout {
        RepoViewLayout::Right => Some(ResolvedPreviewPosition::Right),
        RepoViewLayout::Below => Some(ResolvedPreviewPosition::Below),
        RepoViewLayout::Auto => Some(resolve_auto_preview_position(area)),
        RepoViewLayout::Zoom => None,
    }
}

fn resolve_auto_preview_position(area: Rect) -> ResolvedPreviewPosition {
    let right_preview_width = area.width.saturating_mul(PREVIEW_SPLIT_RIGHT_PERCENT) / 100;
    let right_table_width = area.width.saturating_sub(right_preview_width);
    let below_preview_height = area.height.saturating_mul(PREVIEW_SPLIT_BELOW_PERCENT) / 100;
    let below_table_height = area.height.saturating_sub(below_preview_height);

    let right_viable = right_table_width >= MIN_TABLE_WIDTH && right_preview_width >= MIN_PREVIEW_WIDTH;
    let below_viable = below_table_height >= MIN_TABLE_HEIGHT && below_preview_height >= MIN_PREVIEW_HEIGHT;

    match (right_viable, below_viable) {
        (true, false) => ResolvedPreviewPosition::Right,
        (false, true) => ResolvedPreviewPosition::Below,
        (false, false) => ResolvedPreviewPosition::Right,
        (true, true) => {
            let aspect_ratio = area.width as f32 / area.height as f32;
            if aspect_ratio < PREVIEW_BELOW_ASPECT_RATIO_THRESHOLD {
                ResolvedPreviewPosition::Below
            } else {
                ResolvedPreviewPosition::Right
            }
        }
    }
}

/// Root widget that composes the base layer: tab bar, content area (table +
/// preview), status bar, and event log.
///
/// Sits at `widget_stack[0]` and handles all Normal-mode actions that the
/// previous `WorkItemTable` widget handled. Modal widgets are pushed on top
/// and rendered after BaseView.
/// Double-click detection state for table row clicks.
#[derive(Default)]
pub struct DoubleClickState {
    pub last_time: Option<Instant>,
    pub last_selectable_idx: Option<usize>,
}

pub struct BaseView {
    pub tab_bar: TabBar,
    pub status_bar: StatusBarWidget,
    pub table: WorkItemTable,
    pub preview: PreviewPanel,
    pub event_log: EventLogWidget,
    /// Stored from render for mouse click hit-testing.
    table_area: Rect,
    /// Gear icon area, captured from layout after table render.
    pub(crate) gear_area: Option<Rect>,
    /// Double-click detection for table rows.
    double_click: DoubleClickState,
    /// Tab drag-reorder state.
    pub drag: DragState,
}

impl Default for BaseView {
    fn default() -> Self {
        Self::new()
    }
}

impl BaseView {
    pub fn new() -> Self {
        Self {
            tab_bar: TabBar::new(),
            status_bar: StatusBarWidget::new(),
            table: WorkItemTable::new(),
            preview: PreviewPanel::new(),
            event_log: EventLogWidget::new(),
            table_area: Rect::default(),
            gear_area: None,
            double_click: DoubleClickState::default(),
            drag: DragState::default(),
        }
    }

    // ── Rendering helpers ──

    fn render_content(&mut self, model: &TuiModel, ui: &mut UiState, theme: &Theme, frame: &mut Frame, area: Rect) {
        if ui.mode.is_config() {
            self.table_area = Rect::default();
            ui.layout.table_area = Rect::default();
            self.event_log.render_config_screen(model, theme, frame, area);
            return;
        }

        let Some(position) = resolve_preview_position(area, ui.view_layout) else {
            self.table_area = area;
            ui.layout.table_area = area;
            self.table.render(model, ui, theme, frame, area);
            return;
        };

        let chunks = match position {
            ResolvedPreviewPosition::Right => Layout::default()
                .direction(Direction::Horizontal)
                .constraints([
                    Constraint::Percentage(100 - PREVIEW_SPLIT_RIGHT_PERCENT),
                    Constraint::Percentage(PREVIEW_SPLIT_RIGHT_PERCENT),
                ])
                .split(area),
            ResolvedPreviewPosition::Below => Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Percentage(100 - PREVIEW_SPLIT_BELOW_PERCENT),
                    Constraint::Percentage(PREVIEW_SPLIT_BELOW_PERCENT),
                ])
                .split(area),
        };

        self.table_area = chunks[0];
        ui.layout.table_area = chunks[0];
        self.table.render(model, ui, theme, frame, chunks[0]);
        self.preview.render(model, ui, theme, frame, chunks[1]);
    }

    // ── Mouse helpers ──

    /// Hit-test a mouse position against the table area to find which
    /// selectable row (if any) was clicked.
    fn row_at_mouse(&self, x: u16, y: u16, ctx: &WidgetContext) -> Option<usize> {
        let ta = self.table_area;
        if x >= ta.x && x < ta.x + ta.width && y >= ta.y && y < ta.y + ta.height {
            let row_in_table = (y - ta.y) as usize;
            if row_in_table < 2 {
                return None;
            }
            let data_row = row_in_table - 2;
            let repo_key = &ctx.repo_order[ctx.active_repo];
            let rui = &ctx.repo_ui[repo_key];
            let offset = rui.table_state.offset();
            let actual_row = data_row + offset;
            rui.table_view.selectable_indices.iter().position(|&idx| idx == actual_row)
        } else {
            None
        }
    }

    // ── Action helpers ──

    fn toggle_providers(ctx: &mut WidgetContext) -> Outcome {
        let repo_key = &ctx.repo_order[ctx.active_repo];
        let rui = ctx.repo_ui.get_mut(repo_key).expect("active repo must have UI state");
        rui.show_providers = !rui.show_providers;
        Outcome::Consumed
    }

    fn dismiss(ctx: &mut WidgetContext) -> Outcome {
        // Cancellation takes priority over other dismiss actions while a command is running.
        if let Some(&command_id) = ctx.in_flight.keys().next() {
            ctx.app_actions.push(AppAction::CancelCommand(command_id));
            return Outcome::Consumed;
        }

        let repo_key = &ctx.repo_order[ctx.active_repo];
        let rui = ctx.repo_ui.get_mut(repo_key).expect("active repo must have UI state");

        if rui.active_search_query.is_some() {
            let repo_path = ctx.model.active_repo_root().clone();
            ctx.commands.push(flotilla_protocol::Command {
                host: None,
                context_repo: None,
                action: flotilla_protocol::CommandAction::ClearIssueSearch { repo: flotilla_protocol::RepoSelector::Path(repo_path) },
            });
            rui.active_search_query = None;
        } else if rui.show_providers {
            rui.show_providers = false;
        } else if !rui.multi_selected.is_empty() {
            rui.multi_selected.clear();
        } else {
            ctx.app_actions.push(AppAction::Quit);
        }
        Outcome::Consumed
    }
}

impl InteractiveWidget for BaseView {
    fn handle_action(&mut self, action: Action, ctx: &mut WidgetContext) -> Outcome {
        // Config mode: handle event-log navigation and dismiss.
        if matches!(*ctx.mode, UiMode::Config) {
            return match action {
                Action::SelectNext => {
                    self.event_log.select_next();
                    Outcome::Consumed
                }
                Action::SelectPrev => {
                    self.event_log.select_prev();
                    Outcome::Consumed
                }
                Action::Dismiss => {
                    *ctx.mode = UiMode::Normal;
                    Outcome::Consumed
                }
                _ => Outcome::Ignored,
            };
        }

        // Normal mode: handle table actions, modal opens, and toggles.
        if !matches!(*ctx.mode, UiMode::Normal) {
            return Outcome::Ignored;
        }

        match action {
            Action::SelectNext => {
                self.table.select_next(ctx);
                Outcome::Consumed
            }
            Action::SelectPrev => {
                self.table.select_prev(ctx);
                Outcome::Consumed
            }
            Action::ToggleMultiSelect => {
                self.table.toggle_multi_select(ctx);
                Outcome::Consumed
            }
            Action::ToggleProviders => Self::toggle_providers(ctx),
            Action::Dismiss => Self::dismiss(ctx),
            Action::Quit => {
                ctx.app_actions.push(AppAction::Quit);
                Outcome::Consumed
            }
            // Open modal widgets -- return Push outcomes
            Action::ToggleHelp => Outcome::Push(Box::new(super::help::HelpWidget::new())),

            Action::OpenBranchInput => {
                Outcome::Push(Box::new(super::branch_input::BranchInputWidget::new(crate::app::BranchInputKind::Manual)))
            }

            Action::OpenIssueSearch => {
                *ctx.mode = UiMode::IssueSearch { input: tui_input::Input::default() };
                Outcome::Push(Box::new(super::issue_search::IssueSearchWidget::new()))
            }

            Action::OpenCommandPalette => Outcome::Push(Box::new(super::command_palette::CommandPaletteWidget::new())),

            // Actions that need &App context -- fall through to legacy dispatch
            Action::Confirm | Action::OpenActionMenu | Action::OpenFilePicker | Action::Dispatch(_) => Outcome::Ignored,

            // Global actions (tab nav, theme, layout, host, debug, status bar keys,
            // refresh) are pre-dispatched before the widget stack. If they somehow
            // reach here, ignore them.
            _ => Outcome::Ignored,
        }
    }

    fn handle_mouse(&mut self, mouse: MouseEvent, ctx: &mut WidgetContext) -> Outcome {
        let x = mouse.column;
        let y = mouse.row;

        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                // 1. Event log filter area (only active in Config mode —
                // the filter area Rect is stale when not in Config mode)
                if ctx.mode.is_config() && self.event_log.handle_click(x, y) {
                    return Outcome::Consumed;
                }

                // 2. Tab bar
                let is_config = ctx.mode.is_config();
                let tab_action = self.tab_bar.handle_click(x, y, is_config);
                match tab_action {
                    super::tab_bar::TabBarAction::SwitchToConfig => {
                        self.drag.dragging_tab = None;
                        ctx.app_actions.push(AppAction::SwitchToConfig);
                        return Outcome::Consumed;
                    }
                    super::tab_bar::TabBarAction::SwitchToRepo(i) => {
                        ctx.app_actions.push(AppAction::SwitchToRepo(i));
                        // Start potential drag
                        self.drag.dragging_tab = Some(i);
                        self.drag.start_x = x;
                        self.drag.active = false;
                        return Outcome::Consumed;
                    }
                    super::tab_bar::TabBarAction::OpenFilePicker => {
                        ctx.app_actions.push(AppAction::OpenFilePicker);
                        return Outcome::Consumed;
                    }
                    super::tab_bar::TabBarAction::None => {}
                }

                // 3. Status bar
                if let Some(sb_action) = self.status_bar.handle_click(x, y) {
                    match sb_action {
                        StatusBarAction::KeyPress { code, modifiers } => {
                            ctx.app_actions.push(AppAction::StatusBarKeyPress { code, modifiers });
                        }
                        StatusBarAction::ClearError(id) => {
                            ctx.app_actions.push(AppAction::ClearError(id));
                        }
                    }
                    return Outcome::Consumed;
                }

                // Clear drag if click didn't hit tab bar
                self.drag.dragging_tab = None;

                // 4. Table area (Normal mode only)
                if matches!(*ctx.mode, UiMode::Normal) {
                    // Gear icon in the table border area
                    if let Some(gear_area) = self.gear_area {
                        if x >= gear_area.x && x < gear_area.x + gear_area.width && y >= gear_area.y && y < gear_area.y + gear_area.height {
                            ctx.app_actions.push(AppAction::ToggleProviders);
                            return Outcome::Consumed;
                        }
                    }

                    if let Some(si) = self.row_at_mouse(x, y, ctx) {
                        let now = Instant::now();
                        let is_double_click = self.double_click.last_time.map(|t| now.duration_since(t).as_millis() < 400).unwrap_or(false)
                            && self.double_click.last_selectable_idx == Some(si);

                        let repo_key = &ctx.repo_order[ctx.active_repo];
                        let rui = ctx.repo_ui.get_mut(repo_key).expect("active repo must have UI state");
                        let table_idx = rui.table_view.selectable_indices[si];
                        rui.selected_selectable_idx = Some(si);
                        rui.table_state.select(Some(table_idx));

                        if is_double_click {
                            ctx.app_actions.push(AppAction::ActionEnter);
                            self.double_click.last_time = None;
                            self.double_click.last_selectable_idx = None;
                        } else {
                            self.double_click.last_time = Some(now);
                            self.double_click.last_selectable_idx = Some(si);
                        }
                        return Outcome::Consumed;
                    }
                }

                Outcome::Ignored
            }

            MouseEventKind::Down(MouseButton::Right) => {
                if matches!(*ctx.mode, UiMode::Normal) {
                    if let Some(si) = self.row_at_mouse(x, y, ctx) {
                        let repo_key = &ctx.repo_order[ctx.active_repo];
                        let rui = ctx.repo_ui.get_mut(repo_key).expect("active repo must have UI state");
                        let table_idx = rui.table_view.selectable_indices[si];
                        rui.selected_selectable_idx = Some(si);
                        rui.table_state.select(Some(table_idx));
                        ctx.app_actions.push(AppAction::OpenActionMenu);
                        return Outcome::Consumed;
                    }
                }
                Outcome::Ignored
            }

            MouseEventKind::Drag(MouseButton::Left) => {
                if self.drag.dragging_tab.is_some() {
                    // Tab drag — we can't mutate model.repo_order through ctx
                    // (it's read-only), so we need to signal the App.
                    // But actually, we CAN read tab_bar areas and compute the swap.
                    // The actual repo_order mutation will be done via AppAction.

                    if !self.drag.active {
                        let dx = (x as i16 - self.drag.start_x as i16).unsigned_abs();
                        if dx >= 2 {
                            self.drag.active = true;
                        }
                    }

                    // Note: actual repo_order swap happens in App because model is read-only.
                    // We emit a TabDragMove that the App will process.
                    // For now, keep it simple: the drag visual is handled by the active flag,
                    // and tab_bar.handle_drag does the swap in App context.
                    // We return Consumed to prevent other handlers from processing.
                    return Outcome::Consumed;
                }
                Outcome::Ignored
            }

            MouseEventKind::Up(MouseButton::Left) => {
                if self.drag.dragging_tab.take().is_some() {
                    if self.drag.active {
                        ctx.app_actions.push(AppAction::SaveTabOrder);
                    }
                    self.drag.active = false;
                    return Outcome::Consumed;
                }
                Outcome::Ignored
            }

            MouseEventKind::ScrollDown => {
                if matches!(*ctx.mode, UiMode::Normal | UiMode::Config) {
                    if matches!(*ctx.mode, UiMode::Config) {
                        self.event_log.select_next();
                    } else {
                        self.table.select_next(ctx);
                    }
                    return Outcome::Consumed;
                }
                Outcome::Ignored
            }

            MouseEventKind::ScrollUp => {
                if matches!(*ctx.mode, UiMode::Normal | UiMode::Config) {
                    if matches!(*ctx.mode, UiMode::Config) {
                        self.event_log.select_prev();
                    } else {
                        self.table.select_prev(ctx);
                    }
                    return Outcome::Consumed;
                }
                Outcome::Ignored
            }

            _ => Outcome::Ignored,
        }
    }

    fn render(&mut self, frame: &mut Frame, _area: Rect, ctx: &mut RenderContext) {
        let constraints = vec![Constraint::Length(1), Constraint::Min(0), Constraint::Length(1)];
        let chunks = Layout::default().direction(Direction::Vertical).constraints(constraints).split(frame.area());

        self.tab_bar.render(ctx.model, ctx.ui, self.drag.active, ctx.theme, frame, chunks[0]);
        self.render_content(ctx.model, ctx.ui, ctx.theme, frame, chunks[1]);

        // Capture gear icon area from layout (set by work_item_table render).
        self.gear_area = ctx.ui.layout.tab_areas.get(&TabId::Gear).copied();

        // When the palette is active, move the status bar to the top of the overlay so the
        // input sits above the results instead of being pinned to the bottom of the screen.
        let status_bar_area = if ctx.active_widget_mode == Some(ModeId::CommandPalette) {
            ui_helpers::bottom_anchored_overlay(frame.area(), 1, crate::palette::MAX_PALETTE_ROWS as u16).status_row
        } else {
            chunks[2]
        };
        self.status_bar.render(
            ctx.model,
            ctx.ui,
            ctx.in_flight,
            ctx.theme,
            frame,
            status_bar_area,
            ctx.active_widget_mode,
            ctx.active_widget_data.clone(),
        );
    }

    fn mode_id(&self) -> ModeId {
        ModeId::Normal
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use flotilla_protocol::WorkItemIdentity;
    use ratatui::layout::Rect;

    use super::*;
    use crate::app::{
        test_support::{issue_table_entries, TestWidgetHarness},
        RepoViewLayout,
    };

    fn harness_with_items(count: usize) -> TestWidgetHarness {
        let mut harness = TestWidgetHarness::new();
        let repo_key = harness.model.repo_order[0].clone();
        harness.repo_ui.get_mut(&repo_key).expect("repo ui exists").table_view = issue_table_entries(count);
        harness
    }

    fn harness_with_selected_items(count: usize) -> TestWidgetHarness {
        let mut harness = harness_with_items(count);
        if count > 0 {
            let repo_key = harness.model.repo_order[0].clone();
            let rui = harness.repo_ui.get_mut(&repo_key).expect("repo ui exists");
            rui.selected_selectable_idx = Some(0);
            rui.table_state.select(Some(0));
        }
        harness
    }

    // -- mode_id --

    #[test]
    fn mode_id_is_normal() {
        let widget = BaseView::new();
        assert_eq!(widget.mode_id(), ModeId::Normal);
    }

    // -- SelectNext / SelectPrev --

    #[test]
    fn select_next_from_none_selects_first() {
        let mut widget = BaseView::new();
        let mut harness = harness_with_items(5);
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::SelectNext, &mut ctx);
        assert!(matches!(outcome, Outcome::Consumed));

        let repo_key = &harness.model.repo_order[0];
        assert_eq!(harness.repo_ui[repo_key].selected_selectable_idx, Some(0));
    }

    #[test]
    fn select_next_advances() {
        let mut widget = BaseView::new();
        let mut harness = harness_with_selected_items(5);
        let mut ctx = harness.ctx();

        widget.handle_action(Action::SelectNext, &mut ctx);

        let repo_key = &harness.model.repo_order[0];
        assert_eq!(harness.repo_ui[repo_key].selected_selectable_idx, Some(1));
    }

    #[test]
    fn select_next_stays_at_end() {
        let mut widget = BaseView::new();
        let mut harness = harness_with_selected_items(2);

        {
            let mut ctx = harness.ctx();
            widget.handle_action(Action::SelectNext, &mut ctx); // 0 -> 1
        }
        {
            let mut ctx = harness.ctx();
            widget.handle_action(Action::SelectNext, &mut ctx); // 1 -> 1 (stays)
        }

        let repo_key = &harness.model.repo_order[0];
        assert_eq!(harness.repo_ui[repo_key].selected_selectable_idx, Some(1));
    }

    #[test]
    fn select_next_noop_on_empty() {
        let mut widget = BaseView::new();
        let mut harness = harness_with_items(0);
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::SelectNext, &mut ctx);
        assert!(matches!(outcome, Outcome::Consumed));

        let repo_key = &harness.model.repo_order[0];
        assert_eq!(harness.repo_ui[repo_key].selected_selectable_idx, None);
    }

    #[test]
    fn select_prev_from_none_selects_first() {
        let mut widget = BaseView::new();
        let mut harness = harness_with_items(5);
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::SelectPrev, &mut ctx);
        assert!(matches!(outcome, Outcome::Consumed));

        let repo_key = &harness.model.repo_order[0];
        assert_eq!(harness.repo_ui[repo_key].selected_selectable_idx, Some(0));
    }

    #[test]
    fn select_prev_decrements() {
        let mut widget = BaseView::new();
        let mut harness = harness_with_selected_items(5);

        {
            let mut ctx = harness.ctx();
            widget.handle_action(Action::SelectNext, &mut ctx); // 0 -> 1
        }
        {
            let mut ctx = harness.ctx();
            widget.handle_action(Action::SelectNext, &mut ctx); // 1 -> 2
        }
        {
            let mut ctx = harness.ctx();
            widget.handle_action(Action::SelectPrev, &mut ctx); // 2 -> 1
        }

        let repo_key = &harness.model.repo_order[0];
        assert_eq!(harness.repo_ui[repo_key].selected_selectable_idx, Some(1));
    }

    #[test]
    fn select_prev_stays_at_zero() {
        let mut widget = BaseView::new();
        let mut harness = harness_with_selected_items(5);
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::SelectPrev, &mut ctx);
        assert!(matches!(outcome, Outcome::Consumed));

        let repo_key = &harness.model.repo_order[0];
        assert_eq!(harness.repo_ui[repo_key].selected_selectable_idx, Some(0));
    }

    // -- ToggleMultiSelect --

    #[test]
    fn toggle_multi_select_adds() {
        let mut widget = BaseView::new();
        let mut harness = harness_with_selected_items(3);
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::ToggleMultiSelect, &mut ctx);
        assert!(matches!(outcome, Outcome::Consumed));

        let repo_key = &harness.model.repo_order[0];
        assert!(harness.repo_ui[repo_key].multi_selected.contains(&WorkItemIdentity::Issue("0".into())));
    }

    #[test]
    fn toggle_multi_select_removes() {
        let mut widget = BaseView::new();
        let mut harness = harness_with_selected_items(3);

        {
            let mut ctx = harness.ctx();
            widget.handle_action(Action::ToggleMultiSelect, &mut ctx); // add
        }
        {
            let mut ctx = harness.ctx();
            widget.handle_action(Action::ToggleMultiSelect, &mut ctx); // remove
        }

        let repo_key = &harness.model.repo_order[0];
        assert!(harness.repo_ui[repo_key].multi_selected.is_empty());
    }

    #[test]
    fn toggle_multi_select_noop_when_no_selection() {
        let mut widget = BaseView::new();
        let mut harness = harness_with_items(3);
        let mut ctx = harness.ctx();

        widget.handle_action(Action::ToggleMultiSelect, &mut ctx);

        let repo_key = &harness.model.repo_order[0];
        assert!(harness.repo_ui[repo_key].multi_selected.is_empty());
    }

    // -- ToggleProviders --

    #[test]
    fn toggle_providers_toggles() {
        let mut widget = BaseView::new();
        let mut harness = harness_with_items(1);
        let repo_key = harness.model.repo_order[0].clone();

        {
            let mut ctx = harness.ctx();
            widget.handle_action(Action::ToggleProviders, &mut ctx);
        }

        assert!(harness.repo_ui[&repo_key].show_providers);

        {
            let mut ctx = harness.ctx();
            widget.handle_action(Action::ToggleProviders, &mut ctx);
        }

        assert!(!harness.repo_ui[&repo_key].show_providers);
    }

    // -- Dismiss cascade --

    #[test]
    fn dismiss_cancels_in_flight_first() {
        let mut widget = BaseView::new();
        let mut harness = harness_with_items(1);
        harness.in_flight.insert(42, crate::app::InFlightCommand {
            repo_identity: harness.model.repo_order[0].clone(),
            repo: std::path::PathBuf::from("/tmp/test-repo"),
            description: "test".into(),
        });
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::Dismiss, &mut ctx);
        assert!(matches!(outcome, Outcome::Consumed));
        assert!(ctx.app_actions.iter().any(|a| matches!(a, AppAction::CancelCommand(42))));
        assert!(!ctx.app_actions.iter().any(|a| matches!(a, AppAction::Quit)));
    }

    #[test]
    fn dismiss_clears_search_second() {
        let mut widget = BaseView::new();
        let mut harness = harness_with_items(1);
        let repo_key = harness.model.repo_order[0].clone();
        harness.repo_ui.get_mut(&repo_key).expect("repo ui").active_search_query = Some("test".into());
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::Dismiss, &mut ctx);
        assert!(matches!(outcome, Outcome::Consumed));
        assert!(!ctx.app_actions.iter().any(|a| matches!(a, AppAction::Quit)));

        assert!(harness.repo_ui[&repo_key].active_search_query.is_none());
        let (cmd, _) = harness.commands.take_next().expect("expected ClearIssueSearch command");
        assert!(matches!(cmd.action, flotilla_protocol::CommandAction::ClearIssueSearch { .. }));
    }

    #[test]
    fn dismiss_clears_providers_third() {
        let mut widget = BaseView::new();
        let mut harness = harness_with_items(1);
        let repo_key = harness.model.repo_order[0].clone();
        harness.repo_ui.get_mut(&repo_key).expect("repo ui").show_providers = true;
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::Dismiss, &mut ctx);
        assert!(matches!(outcome, Outcome::Consumed));
        assert!(!ctx.app_actions.iter().any(|a| matches!(a, AppAction::Quit)));

        assert!(!harness.repo_ui[&repo_key].show_providers);
    }

    #[test]
    fn dismiss_clears_multi_select_fourth() {
        let mut widget = BaseView::new();
        let mut harness = harness_with_selected_items(3);
        let repo_key = harness.model.repo_order[0].clone();
        harness.repo_ui.get_mut(&repo_key).expect("repo ui").multi_selected.insert(WorkItemIdentity::Issue("0".into()));

        let mut ctx = harness.ctx();
        let outcome = widget.handle_action(Action::Dismiss, &mut ctx);
        assert!(matches!(outcome, Outcome::Consumed));
        assert!(!ctx.app_actions.iter().any(|a| matches!(a, AppAction::Quit)));

        assert!(harness.repo_ui[&repo_key].multi_selected.is_empty());
    }

    #[test]
    fn dismiss_quits_when_nothing_to_clear() {
        let mut widget = BaseView::new();
        let mut harness = harness_with_items(1);
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::Dismiss, &mut ctx);
        assert!(matches!(outcome, Outcome::Consumed));
        assert!(ctx.app_actions.iter().any(|a| matches!(a, AppAction::Quit)));
    }

    // -- Quit --

    #[test]
    fn quit_pushes_app_action() {
        let mut widget = BaseView::new();
        let mut harness = harness_with_items(1);
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::Quit, &mut ctx);
        assert!(matches!(outcome, Outcome::Consumed));
        assert!(ctx.app_actions.iter().any(|a| matches!(a, AppAction::Quit)));
    }

    // -- Push modal widgets --

    #[test]
    fn toggle_help_pushes_help_widget() {
        let mut widget = BaseView::new();
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::ToggleHelp, &mut ctx);
        assert!(matches!(outcome, Outcome::Push(_)));
    }

    #[test]
    fn open_branch_input_pushes_widget() {
        let mut widget = BaseView::new();
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::OpenBranchInput, &mut ctx);
        assert!(matches!(outcome, Outcome::Push(_)));
    }

    #[test]
    fn open_issue_search_pushes_widget_and_sets_mode() {
        let mut widget = BaseView::new();
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::OpenIssueSearch, &mut ctx);
        assert!(matches!(outcome, Outcome::Push(_)));
        assert!(matches!(harness.mode, UiMode::IssueSearch { .. }));
    }

    #[test]
    fn open_command_palette_pushes_widget() {
        let mut widget = BaseView::new();
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::OpenCommandPalette, &mut ctx);
        assert!(matches!(outcome, Outcome::Push(_)));
    }

    // -- Ignored actions --

    #[test]
    fn confirm_returns_ignored() {
        let mut widget = BaseView::new();
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::Confirm, &mut ctx);
        assert!(matches!(outcome, Outcome::Ignored));
    }

    #[test]
    fn open_action_menu_returns_ignored() {
        let mut widget = BaseView::new();
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::OpenActionMenu, &mut ctx);
        assert!(matches!(outcome, Outcome::Ignored));
    }

    #[test]
    fn tab_navigation_returns_ignored() {
        let mut widget = BaseView::new();
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        assert!(matches!(widget.handle_action(Action::PrevTab, &mut ctx), Outcome::Ignored));
        assert!(matches!(widget.handle_action(Action::NextTab, &mut ctx), Outcome::Ignored));
    }

    #[test]
    fn global_actions_return_ignored() {
        let mut widget = BaseView::new();
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        // Global actions are pre-dispatched by App before reaching the widget
        // stack, so BaseView should return Ignored for all of them.
        let globals =
            [Action::CycleTheme, Action::CycleLayout, Action::CycleHost, Action::ToggleDebug, Action::ToggleStatusBarKeys, Action::Refresh];
        for action in globals {
            let outcome = widget.handle_action(action, &mut ctx);
            assert!(matches!(outcome, Outcome::Ignored), "expected Ignored for {action:?}");
        }
    }

    // -- Config mode --

    #[test]
    fn config_select_next_navigates_event_log() {
        let mut widget = BaseView::new();
        widget.event_log.count = 5;
        widget.event_log.selected = Some(0);

        let mut harness = TestWidgetHarness::new();
        harness.mode = UiMode::Config;
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::SelectNext, &mut ctx);
        assert!(matches!(outcome, Outcome::Consumed));
        assert_eq!(widget.event_log.selected, Some(1));
    }

    #[test]
    fn config_select_prev_navigates_event_log() {
        let mut widget = BaseView::new();
        widget.event_log.count = 5;
        widget.event_log.selected = Some(3);

        let mut harness = TestWidgetHarness::new();
        harness.mode = UiMode::Config;
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::SelectPrev, &mut ctx);
        assert!(matches!(outcome, Outcome::Consumed));
        assert_eq!(widget.event_log.selected, Some(2));
    }

    #[test]
    fn config_dismiss_returns_to_normal() {
        let mut widget = BaseView::new();
        let mut harness = TestWidgetHarness::new();
        harness.mode = UiMode::Config;
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::Dismiss, &mut ctx);
        assert!(matches!(outcome, Outcome::Consumed));
        assert!(matches!(harness.mode, UiMode::Normal));
    }

    #[test]
    fn config_unhandled_action_returns_ignored() {
        let mut widget = BaseView::new();
        let mut harness = TestWidgetHarness::new();
        harness.mode = UiMode::Config;
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::Confirm, &mut ctx);
        assert!(matches!(outcome, Outcome::Ignored));
    }

    // -- Preview position resolution --

    #[test]
    fn auto_layout_prefers_right_when_wide() {
        let position = resolve_preview_position(Rect::new(0, 0, 160, 40), RepoViewLayout::Auto);
        assert_eq!(position, Some(ResolvedPreviewPosition::Right));
    }

    #[test]
    fn auto_layout_prefers_below_when_tall() {
        let position = resolve_preview_position(Rect::new(0, 0, 90, 50), RepoViewLayout::Auto);
        assert_eq!(position, Some(ResolvedPreviewPosition::Below));
    }

    #[test]
    fn explicit_right_layout() {
        let position = resolve_preview_position(Rect::new(0, 0, 90, 50), RepoViewLayout::Right);
        assert_eq!(position, Some(ResolvedPreviewPosition::Right));
    }

    #[test]
    fn explicit_below_layout() {
        let position = resolve_preview_position(Rect::new(0, 0, 160, 40), RepoViewLayout::Below);
        assert_eq!(position, Some(ResolvedPreviewPosition::Below));
    }

    #[test]
    fn zoom_layout_returns_none() {
        let position = resolve_preview_position(Rect::new(0, 0, 160, 40), RepoViewLayout::Zoom);
        assert_eq!(position, None);
    }

    #[test]
    fn auto_neither_viable_falls_back_to_right() {
        // 60x10: right_preview_width = 24 (< MIN_PREVIEW_WIDTH 32),
        //        below_preview_height = 4 (< MIN_PREVIEW_HEIGHT 6)
        // Both layouts are non-viable, so fallback to Right.
        let result = resolve_auto_preview_position(Rect::new(0, 0, 60, 10));
        assert_eq!(result, ResolvedPreviewPosition::Right);
    }

    #[test]
    fn auto_only_right_viable() {
        // 210x10: right_preview_width = 84 (>= 32), right_table_width = 126 (>= 50) -> viable
        //         below_preview_height = 4 (< 6) -> not viable
        let result = resolve_auto_preview_position(Rect::new(0, 0, 210, 10));
        assert_eq!(result, ResolvedPreviewPosition::Right);
    }

    #[test]
    fn auto_only_below_viable() {
        // 60x40: right_preview_width = 24 (< 32) -> not viable
        //        below_preview_height = 16 (>= 6), below_table_height = 24 (>= 8) -> viable
        let result = resolve_auto_preview_position(Rect::new(0, 0, 60, 40));
        assert_eq!(result, ResolvedPreviewPosition::Below);
    }

    #[test]
    fn auto_both_viable_wide_prefers_right() {
        // 160x40: both viable, aspect_ratio = 4.0 (>= 2.0) -> Right
        let result = resolve_auto_preview_position(Rect::new(0, 0, 160, 40));
        assert_eq!(result, ResolvedPreviewPosition::Right);
    }

    #[test]
    fn auto_both_viable_tall_prefers_below() {
        // 90x50: both viable, aspect_ratio = 1.8 (< 2.0) -> Below
        let result = resolve_auto_preview_position(Rect::new(0, 0, 90, 50));
        assert_eq!(result, ResolvedPreviewPosition::Below);
    }
}
