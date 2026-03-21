use std::{
    any::Any,
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::Arc,
    time::Instant,
};

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use flotilla_core::data::{GroupEntry, SectionLabels};
use flotilla_protocol::{ProviderData, RepoIdentity, RepoLabels, WorkItem, WorkItemIdentity};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    Frame,
};

use super::{
    preview_panel::PreviewPanel, work_item_table::WorkItemTable, AppAction, InteractiveWidget, Outcome, RenderContext, WidgetContext,
};
use crate::{
    app::{
        ui_state::{PendingAction, UiMode},
        RepoViewLayout,
    },
    keymap::{Action, ModeId},
    shared::Shared,
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

// ── RepoData ──

/// Daemon-sourced data for a single repository. Written by the event loop
/// via `Shared::mutate()` and read by `RepoPage` during reconciliation and
/// rendering.
pub struct RepoData {
    pub path: PathBuf,
    pub providers: Arc<ProviderData>,
    pub labels: RepoLabels,
    pub provider_names: HashMap<String, Vec<String>>,
    pub provider_health: HashMap<String, HashMap<String, bool>>,
    pub work_items: Vec<WorkItem>,
    pub issue_has_more: bool,
    pub issue_total: Option<usize>,
    pub issue_search_active: bool,
    pub loading: bool,
}

// ── DoubleClickState ──

#[derive(Default)]
struct DoubleClickState {
    last_time: Option<Instant>,
    last_selectable_idx: Option<usize>,
}

// ── RepoPage ──

/// Per-repo content widget that owns the work-item table, preview panel,
/// and associated UI state (multi-selection, pending actions, layout).
///
/// Each repo tab gets its own `RepoPage` instance with its own
/// `Shared<RepoData>` handle. The page reconciles daemon data changes into
/// the table on each action/render cycle.
pub struct RepoPage {
    repo_identity: RepoIdentity,
    repo_data: Shared<RepoData>,
    pub table: WorkItemTable,
    pub preview: PreviewPanel,
    pub multi_selected: HashSet<WorkItemIdentity>,
    pub pending_actions: HashMap<WorkItemIdentity, PendingAction>,
    pub layout: RepoViewLayout,
    pub show_providers: bool,
    pub active_search_query: Option<String>,
    last_seen_generation: u64,
    double_click: DoubleClickState,
}

impl RepoPage {
    pub fn new(repo_identity: RepoIdentity, repo_data: Shared<RepoData>, layout: RepoViewLayout) -> Self {
        Self {
            repo_identity,
            repo_data,
            table: WorkItemTable::new(),
            preview: PreviewPanel::new(),
            multi_selected: HashSet::new(),
            pending_actions: HashMap::new(),
            layout,
            show_providers: false,
            active_search_query: None,
            last_seen_generation: 0,
            double_click: DoubleClickState::default(),
        }
    }

    /// Identity of the repo this page represents.
    pub fn repo_identity(&self) -> &RepoIdentity {
        &self.repo_identity
    }

    /// Shared data handle — callers can read or mutate the daemon data.
    pub fn repo_data(&self) -> &Shared<RepoData> {
        &self.repo_data
    }

    /// Check whether the daemon data has changed since last reconciliation
    /// and, if so, rebuild the table and prune stale selections.
    pub fn reconcile_if_changed(&mut self) {
        if let Some(data) = self.repo_data.changed(&mut self.last_seen_generation) {
            let section_labels = SectionLabels {
                checkouts: data.labels.checkouts.section.clone(),
                change_requests: data.labels.change_requests.section.clone(),
                issues: data.labels.issues.section.clone(),
                sessions: data.labels.cloud_agents.section.clone(),
            };
            let grouped = flotilla_core::data::group_work_items(&data.work_items, &data.providers, &section_labels, &data.path);
            self.table.update_items(grouped);

            // Prune stale multi_selected and pending_actions
            let current_identities: HashSet<WorkItemIdentity> = self
                .table
                .grouped_items
                .table_entries
                .iter()
                .filter_map(|e| match e {
                    GroupEntry::Item(item) => Some(item.identity.clone()),
                    _ => None,
                })
                .collect();
            self.multi_selected.retain(|id| current_identities.contains(id));
            self.pending_actions.retain(|id, _| current_identities.contains(id));
        }
    }

    /// Cycle the per-page layout (Auto -> Zoom -> Right -> Below -> Auto).
    pub fn cycle_layout(&mut self) {
        self.layout = match self.layout {
            RepoViewLayout::Auto => RepoViewLayout::Zoom,
            RepoViewLayout::Zoom => RepoViewLayout::Right,
            RepoViewLayout::Right => RepoViewLayout::Below,
            RepoViewLayout::Below => RepoViewLayout::Auto,
        };
    }

    // ── Rendering helpers ──

    fn render_content(&mut self, frame: &mut Frame, area: Rect, ctx: &mut RenderContext) {
        let Some(position) = resolve_preview_position(area, self.layout) else {
            self.render_table(frame, area, ctx);
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

        self.render_table(frame, chunks[0], ctx);
        let selected_item = self.table.selected_work_item();
        self.preview.render_with_item(ctx.model, ctx.ui, selected_item, ctx.theme, frame, chunks[1]);
    }

    /// Render the table using RepoPage-owned state, bypassing RepoUiState.
    fn render_table(&mut self, frame: &mut Frame, area: Rect, ctx: &mut RenderContext) {
        self.table.table_area = area;
        self.table.render_table_owned(
            ctx.model,
            ctx.ui,
            ctx.theme,
            frame,
            area,
            self.show_providers,
            &self.multi_selected,
            &self.pending_actions,
        );
    }

    // ── Action helpers ──

    fn dismiss(&mut self, ctx: &mut WidgetContext) -> Outcome {
        // Cancellation takes priority while a command is running.
        if let Some(&command_id) = ctx.in_flight.keys().next() {
            ctx.app_actions.push(AppAction::CancelCommand(command_id));
            return Outcome::Consumed;
        }

        if self.active_search_query.is_some() {
            let data = self.repo_data.read();
            let repo_path = data.path.clone();
            drop(data);
            ctx.commands.push(flotilla_protocol::Command {
                host: None,
                context_repo: None,
                action: flotilla_protocol::CommandAction::ClearIssueSearch { repo: flotilla_protocol::RepoSelector::Path(repo_path) },
            });
            self.active_search_query = None;
            // Also clear on rui so the status bar sees it immediately
            // (status bar reads rui.active_search_query).
            let repo_key = &ctx.repo_order[ctx.active_repo];
            if let Some(rui) = ctx.repo_ui.get_mut(repo_key) {
                rui.active_search_query = None;
            }
        } else if self.show_providers {
            self.show_providers = false;
        } else if !self.multi_selected.is_empty() {
            self.multi_selected.clear();
        } else {
            ctx.app_actions.push(AppAction::Quit);
        }
        Outcome::Consumed
    }

    fn toggle_multi_select(&mut self) {
        if let Some(si) = self.table.selected_selectable_idx {
            if let Some(&table_idx) = self.table.grouped_items.selectable_indices.get(si) {
                if let Some(GroupEntry::Item(item)) = self.table.grouped_items.table_entries.get(table_idx) {
                    let identity = item.identity.clone();
                    if !self.multi_selected.remove(&identity) {
                        self.multi_selected.insert(identity);
                    }
                }
            }
        }
    }

    pub fn select_all(&mut self) {
        for entry in &self.table.grouped_items.table_entries {
            if let GroupEntry::Item(item) = entry {
                self.multi_selected.insert(item.identity.clone());
            }
        }
    }
}

impl InteractiveWidget for RepoPage {
    fn handle_action(&mut self, action: Action, ctx: &mut WidgetContext) -> Outcome {
        self.reconcile_if_changed();

        match action {
            Action::SelectNext => {
                self.table.select_next_self();
                Outcome::Consumed
            }
            Action::SelectPrev => {
                self.table.select_prev_self();
                Outcome::Consumed
            }
            Action::ToggleMultiSelect => {
                self.toggle_multi_select();
                Outcome::Consumed
            }
            Action::ToggleProviders => {
                self.show_providers = !self.show_providers;
                Outcome::Consumed
            }
            Action::CycleLayout => {
                self.cycle_layout();
                ctx.app_actions.push(AppAction::CycleLayout);
                Outcome::Consumed
            }
            Action::Dismiss => self.dismiss(ctx),
            Action::Quit => {
                ctx.app_actions.push(AppAction::Quit);
                Outcome::Consumed
            }
            Action::ToggleHelp => Outcome::Push(Box::new(super::help::HelpWidget::new())),
            Action::OpenBranchInput => {
                Outcome::Push(Box::new(super::branch_input::BranchInputWidget::new(crate::app::ui_state::BranchInputKind::Manual)))
            }
            Action::OpenIssueSearch => {
                *ctx.mode = UiMode::IssueSearch { input: tui_input::Input::default() };
                Outcome::Push(Box::new(super::issue_search::IssueSearchWidget::new()))
            }
            Action::OpenCommandPalette => Outcome::Push(Box::new(super::command_palette::CommandPaletteWidget::new())),
            // Actions handled at the App level — return Ignored so they bubble up.
            Action::Confirm | Action::OpenActionMenu | Action::OpenFilePicker | Action::Dispatch(_) => Outcome::Ignored,
            _ => Outcome::Ignored,
        }
    }

    fn handle_mouse(&mut self, mouse: MouseEvent, ctx: &mut WidgetContext) -> Outcome {
        self.reconcile_if_changed();

        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if matches!(*ctx.mode, UiMode::Normal) {
                    let x = mouse.column;
                    let y = mouse.row;

                    // Double-click detection using owned table state
                    if let Some(si) = self.table.row_at_mouse_self(x, y) {
                        let now = Instant::now();
                        let is_double_click = self.double_click.last_time.map(|t| now.duration_since(t).as_millis() < 400).unwrap_or(false)
                            && self.double_click.last_selectable_idx == Some(si);

                        if is_double_click {
                            // Select the row, then trigger double-click action
                            self.table.select_row_self(si);
                            ctx.app_actions.push(AppAction::ActionEnter);
                            self.double_click.last_time = None;
                            self.double_click.last_selectable_idx = None;
                            return Outcome::Consumed;
                        }

                        self.double_click.last_time = Some(now);
                        self.double_click.last_selectable_idx = Some(si);
                    }

                    // Gear icon click (still needs ctx for the AppAction)
                    if let Some(gear_area) = self.table.gear_area {
                        if x >= gear_area.x && x < gear_area.x + gear_area.width && y >= gear_area.y && y < gear_area.y + gear_area.height {
                            ctx.app_actions.push(AppAction::ToggleProviders);
                            return Outcome::Consumed;
                        }
                    }

                    // Single click: select row using owned state
                    if let Some(si) = self.table.row_at_mouse_self(x, y) {
                        self.table.select_row_self(si);
                        return Outcome::Consumed;
                    }
                }

                Outcome::Ignored
            }

            MouseEventKind::Down(MouseButton::Right) => {
                if matches!(*ctx.mode, UiMode::Normal) {
                    // Right-click: select row using owned state, then open action menu
                    if let Some(si) = self.table.row_at_mouse_self(mouse.column, mouse.row) {
                        self.table.select_row_self(si);
                        ctx.app_actions.push(AppAction::OpenActionMenu);
                        return Outcome::Consumed;
                    }
                }
                Outcome::Ignored
            }

            MouseEventKind::ScrollDown => {
                if matches!(*ctx.mode, UiMode::Normal) {
                    self.table.select_next_self();
                    return Outcome::Consumed;
                }
                Outcome::Ignored
            }

            MouseEventKind::ScrollUp => {
                if matches!(*ctx.mode, UiMode::Normal) {
                    self.table.select_prev_self();
                    return Outcome::Consumed;
                }
                Outcome::Ignored
            }

            _ => Outcome::Ignored,
        }
    }

    fn render(&mut self, frame: &mut Frame, area: Rect, ctx: &mut RenderContext) {
        self.reconcile_if_changed();
        self.render_content(frame, area, ctx);
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
    use flotilla_protocol::{ProviderData, RepoLabels, WorkItemIdentity};

    use super::*;
    use crate::app::test_support::{issue_item, TestWidgetHarness};

    fn test_repo_identity() -> RepoIdentity {
        RepoIdentity { authority: "local".into(), path: "/tmp/test-repo".into() }
    }

    fn test_repo_data(items: Vec<WorkItem>) -> Shared<RepoData> {
        Shared::new(RepoData {
            path: PathBuf::from("/tmp/test-repo"),
            providers: Arc::new(ProviderData::default()),
            labels: RepoLabels::default(),
            provider_names: HashMap::new(),
            provider_health: HashMap::new(),
            work_items: items,
            issue_has_more: false,
            issue_total: None,
            issue_search_active: false,
            loading: false,
        })
    }

    fn page_with_items(items: Vec<WorkItem>) -> RepoPage {
        let data = test_repo_data(items);
        let mut page = RepoPage::new(test_repo_identity(), data, RepoViewLayout::Auto);
        page.reconcile_if_changed();
        page
    }

    // ── reconcile_if_changed ──

    #[test]
    fn reconcile_rebuilds_table_on_data_change() {
        let data = test_repo_data(vec![issue_item("1"), issue_item("2")]);
        let mut page = RepoPage::new(test_repo_identity(), data.clone(), RepoViewLayout::Auto);

        // First reconciliation should pick up initial data.
        page.reconcile_if_changed();
        assert_eq!(page.table.grouped_items.selectable_indices.len(), 2);

        // Mutate the shared data to add a third item.
        data.mutate(|d| d.work_items.push(issue_item("3")));

        page.reconcile_if_changed();
        assert_eq!(page.table.grouped_items.selectable_indices.len(), 3);
    }

    #[test]
    fn reconcile_is_noop_when_unchanged() {
        let data = test_repo_data(vec![issue_item("1")]);
        let mut page = RepoPage::new(test_repo_identity(), data, RepoViewLayout::Auto);

        page.reconcile_if_changed();
        let gen_after_first = page.last_seen_generation;

        // Second call should not update generation.
        page.reconcile_if_changed();
        assert_eq!(page.last_seen_generation, gen_after_first);
    }

    #[test]
    fn reconcile_prunes_stale_multi_select() {
        let data = test_repo_data(vec![issue_item("1"), issue_item("2"), issue_item("3")]);
        let mut page = RepoPage::new(test_repo_identity(), data.clone(), RepoViewLayout::Auto);
        page.reconcile_if_changed();

        // Multi-select items 1 and 3.
        page.multi_selected.insert(WorkItemIdentity::Issue("1".into()));
        page.multi_selected.insert(WorkItemIdentity::Issue("3".into()));
        assert_eq!(page.multi_selected.len(), 2);

        // Remove item 3 from the data.
        data.mutate(|d| d.work_items.retain(|i| i.identity != WorkItemIdentity::Issue("3".into())));

        page.reconcile_if_changed();
        assert_eq!(page.multi_selected.len(), 1);
        assert!(page.multi_selected.contains(&WorkItemIdentity::Issue("1".into())));
        assert!(!page.multi_selected.contains(&WorkItemIdentity::Issue("3".into())));
    }

    #[test]
    fn reconcile_prunes_stale_pending_actions() {
        let data = test_repo_data(vec![issue_item("1"), issue_item("2")]);
        let mut page = RepoPage::new(test_repo_identity(), data.clone(), RepoViewLayout::Auto);
        page.reconcile_if_changed();

        page.pending_actions.insert(WorkItemIdentity::Issue("1".into()), PendingAction {
            command_id: 1,
            status: crate::app::ui_state::PendingStatus::InFlight,
            description: "test".into(),
        });
        page.pending_actions.insert(WorkItemIdentity::Issue("2".into()), PendingAction {
            command_id: 2,
            status: crate::app::ui_state::PendingStatus::InFlight,
            description: "test".into(),
        });

        // Remove item 2.
        data.mutate(|d| d.work_items.retain(|i| i.identity != WorkItemIdentity::Issue("2".into())));

        page.reconcile_if_changed();
        assert!(page.pending_actions.contains_key(&WorkItemIdentity::Issue("1".into())));
        assert!(!page.pending_actions.contains_key(&WorkItemIdentity::Issue("2".into())));
    }

    // ── dismiss cascade ──

    #[test]
    fn dismiss_cascade_cancels_in_flight_first() {
        let mut page = page_with_items(vec![issue_item("1")]);
        let mut harness = TestWidgetHarness::new();
        harness.in_flight.insert(42, crate::app::InFlightCommand {
            repo_identity: harness.model.repo_order[0].clone(),
            repo: PathBuf::from("/tmp/test-repo"),
            description: "test".into(),
        });
        let mut ctx = harness.ctx();

        let outcome = page.handle_action(Action::Dismiss, &mut ctx);
        assert!(matches!(outcome, Outcome::Consumed));
        assert!(ctx.app_actions.iter().any(|a| matches!(a, AppAction::CancelCommand(42))));
        assert!(!ctx.app_actions.iter().any(|a| matches!(a, AppAction::Quit)));
    }

    #[test]
    fn dismiss_cascade_clears_search_second() {
        let mut page = page_with_items(vec![issue_item("1")]);
        page.active_search_query = Some("test".into());

        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let outcome = page.handle_action(Action::Dismiss, &mut ctx);
        assert!(matches!(outcome, Outcome::Consumed));
        assert!(!ctx.app_actions.iter().any(|a| matches!(a, AppAction::Quit)));
        assert!(page.active_search_query.is_none());
    }

    #[test]
    fn dismiss_cascade_clears_providers_third() {
        let mut page = page_with_items(vec![issue_item("1")]);
        page.show_providers = true;

        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let outcome = page.handle_action(Action::Dismiss, &mut ctx);
        assert!(matches!(outcome, Outcome::Consumed));
        assert!(!ctx.app_actions.iter().any(|a| matches!(a, AppAction::Quit)));
        assert!(!page.show_providers);
    }

    #[test]
    fn dismiss_cascade_clears_multi_select_fourth() {
        let mut page = page_with_items(vec![issue_item("1")]);
        page.multi_selected.insert(WorkItemIdentity::Issue("1".into()));

        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let outcome = page.handle_action(Action::Dismiss, &mut ctx);
        assert!(matches!(outcome, Outcome::Consumed));
        assert!(!ctx.app_actions.iter().any(|a| matches!(a, AppAction::Quit)));
        assert!(page.multi_selected.is_empty());
    }

    #[test]
    fn dismiss_cascade_quits_when_nothing_to_clear() {
        let mut page = page_with_items(vec![issue_item("1")]);

        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let outcome = page.handle_action(Action::Dismiss, &mut ctx);
        assert!(matches!(outcome, Outcome::Consumed));
        assert!(ctx.app_actions.iter().any(|a| matches!(a, AppAction::Quit)));
    }

    // ── cycle_layout ──

    #[test]
    fn cycle_layout_is_page_scoped() {
        let mut page = page_with_items(vec![]);
        assert_eq!(page.layout, RepoViewLayout::Auto);

        page.cycle_layout();
        assert_eq!(page.layout, RepoViewLayout::Zoom);

        page.cycle_layout();
        assert_eq!(page.layout, RepoViewLayout::Right);

        page.cycle_layout();
        assert_eq!(page.layout, RepoViewLayout::Below);

        page.cycle_layout();
        assert_eq!(page.layout, RepoViewLayout::Auto);
    }

    #[test]
    fn cycle_layout_action_emits_app_action() {
        let mut page = page_with_items(vec![]);
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let outcome = page.handle_action(Action::CycleLayout, &mut ctx);
        assert!(matches!(outcome, Outcome::Consumed));
        assert!(ctx.app_actions.iter().any(|a| matches!(a, AppAction::CycleLayout)));
        assert_eq!(page.layout, RepoViewLayout::Zoom);
    }

    // ── select_next / select_prev ──

    #[test]
    fn select_next_advances() {
        let mut page = page_with_items(vec![issue_item("1"), issue_item("2"), issue_item("3")]);
        let mut harness = TestWidgetHarness::new();

        {
            let mut ctx = harness.ctx();
            page.handle_action(Action::SelectNext, &mut ctx);
        }

        assert_eq!(page.table.selected_selectable_idx, Some(1));
    }

    #[test]
    fn select_prev_decrements() {
        let mut page = page_with_items(vec![issue_item("1"), issue_item("2"), issue_item("3")]);
        let mut harness = TestWidgetHarness::new();

        // Move to index 2.
        {
            let mut ctx = harness.ctx();
            page.handle_action(Action::SelectNext, &mut ctx);
        }
        {
            let mut ctx = harness.ctx();
            page.handle_action(Action::SelectNext, &mut ctx);
        }
        assert_eq!(page.table.selected_selectable_idx, Some(2));

        // Move back to 1.
        {
            let mut ctx = harness.ctx();
            page.handle_action(Action::SelectPrev, &mut ctx);
        }
        assert_eq!(page.table.selected_selectable_idx, Some(1));
    }

    // ── toggle_multi_select ──

    #[test]
    fn toggle_multi_select_adds_and_removes() {
        let mut page = page_with_items(vec![issue_item("1"), issue_item("2")]);
        let mut harness = TestWidgetHarness::new();

        // Toggle on.
        {
            let mut ctx = harness.ctx();
            page.handle_action(Action::ToggleMultiSelect, &mut ctx);
        }
        assert!(page.multi_selected.contains(&WorkItemIdentity::Issue("1".into())));

        // Toggle off.
        {
            let mut ctx = harness.ctx();
            page.handle_action(Action::ToggleMultiSelect, &mut ctx);
        }
        assert!(page.multi_selected.is_empty());
    }

    // ── select_all ──

    #[test]
    fn select_all_selects_all_items() {
        let mut page = page_with_items(vec![issue_item("1"), issue_item("2"), issue_item("3")]);

        page.select_all();

        assert_eq!(page.multi_selected.len(), 3);
        assert!(page.multi_selected.contains(&WorkItemIdentity::Issue("1".into())));
        assert!(page.multi_selected.contains(&WorkItemIdentity::Issue("2".into())));
        assert!(page.multi_selected.contains(&WorkItemIdentity::Issue("3".into())));
    }

    // ── toggle_providers ──

    #[test]
    fn toggle_providers_toggles() {
        let mut page = page_with_items(vec![]);
        let mut harness = TestWidgetHarness::new();

        {
            let mut ctx = harness.ctx();
            page.handle_action(Action::ToggleProviders, &mut ctx);
        }
        assert!(page.show_providers);

        {
            let mut ctx = harness.ctx();
            page.handle_action(Action::ToggleProviders, &mut ctx);
        }
        assert!(!page.show_providers);
    }

    // ── quit ──

    #[test]
    fn quit_pushes_app_action() {
        let mut page = page_with_items(vec![]);
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let outcome = page.handle_action(Action::Quit, &mut ctx);
        assert!(matches!(outcome, Outcome::Consumed));
        assert!(ctx.app_actions.iter().any(|a| matches!(a, AppAction::Quit)));
    }

    // ── push modal widgets ──

    #[test]
    fn toggle_help_pushes_widget() {
        let mut page = page_with_items(vec![]);
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let outcome = page.handle_action(Action::ToggleHelp, &mut ctx);
        assert!(matches!(outcome, Outcome::Push(_)));
    }

    #[test]
    fn open_branch_input_pushes_widget() {
        let mut page = page_with_items(vec![]);
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let outcome = page.handle_action(Action::OpenBranchInput, &mut ctx);
        assert!(matches!(outcome, Outcome::Push(_)));
    }

    #[test]
    fn open_issue_search_pushes_widget() {
        let mut page = page_with_items(vec![]);
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let outcome = page.handle_action(Action::OpenIssueSearch, &mut ctx);
        assert!(matches!(outcome, Outcome::Push(_)));
        assert!(matches!(harness.mode, UiMode::IssueSearch { .. }));
    }

    #[test]
    fn open_command_palette_pushes_widget() {
        let mut page = page_with_items(vec![]);
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let outcome = page.handle_action(Action::OpenCommandPalette, &mut ctx);
        assert!(matches!(outcome, Outcome::Push(_)));
    }

    // ── ignored actions ──

    #[test]
    fn confirm_returns_ignored() {
        let mut page = page_with_items(vec![]);
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let outcome = page.handle_action(Action::Confirm, &mut ctx);
        assert!(matches!(outcome, Outcome::Ignored));
    }

    #[test]
    fn open_action_menu_returns_ignored() {
        let mut page = page_with_items(vec![]);
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let outcome = page.handle_action(Action::OpenActionMenu, &mut ctx);
        assert!(matches!(outcome, Outcome::Ignored));
    }

    // ── mode_id ──

    #[test]
    fn mode_id_is_normal() {
        let page = page_with_items(vec![]);
        assert_eq!(page.mode_id(), ModeId::Normal);
    }

    // ── preview position resolution ──

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

    // ── Mouse selection regression tests ──

    fn page_with_table_area(items: Vec<WorkItem>) -> RepoPage {
        let mut page = page_with_items(items);
        // Set table_area so row_at_mouse_self can hit-test.
        // Row 0-1 are header, data rows start at row 2.
        page.table.table_area = Rect::new(0, 0, 80, 20);
        page
    }

    #[test]
    fn left_click_selects_row_via_owned_state() {
        let mut page = page_with_table_area(vec![issue_item("1"), issue_item("2"), issue_item("3")]);
        assert_eq!(page.table.selected_selectable_idx, Some(0));

        // Figure out the actual row index for the last selectable item
        let last_si = page.table.grouped_items.selectable_indices.len() - 1;
        let last_table_idx = page.table.grouped_items.selectable_indices[last_si];
        // table_area header is 2 rows, so visual row = table_idx + 2
        let click_row = last_table_idx as u16 + 2;

        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let mouse = crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            column: 5,
            row: click_row,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        let outcome = page.handle_mouse(mouse, &mut ctx);
        assert!(matches!(outcome, Outcome::Consumed));
        assert_eq!(page.table.selected_selectable_idx, Some(last_si), "owned selection should move to clicked row");
    }

    #[test]
    fn right_click_selects_row_and_opens_action_menu() {
        let mut page = page_with_table_area(vec![issue_item("1"), issue_item("2")]);
        assert_eq!(page.table.selected_selectable_idx, Some(0));

        // Figure out the row index for the second selectable item
        let target_si = 1;
        let target_table_idx = page.table.grouped_items.selectable_indices[target_si];
        let click_row = target_table_idx as u16 + 2;

        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let mouse = crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Right),
            column: 5,
            row: click_row,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        let outcome = page.handle_mouse(mouse, &mut ctx);
        assert!(matches!(outcome, Outcome::Consumed));
        assert_eq!(page.table.selected_selectable_idx, Some(target_si), "owned selection should move to right-clicked row");
        assert!(ctx.app_actions.iter().any(|a| matches!(a, AppAction::OpenActionMenu)), "right-click should open action menu");
    }
}
