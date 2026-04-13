use std::{
    any::Any,
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::Arc,
    time::Instant,
};

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use flotilla_core::data::SectionLabels;
use flotilla_protocol::{ProviderData, RepoIdentity, RepoLabels, WorkItem, WorkItemIdentity};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    Frame,
};

use super::{
    preview_panel::PreviewPanel,
    section_table::IssueRow,
    split_table::{SelectedRow, SplitTable},
    AppAction, InteractiveWidget, Outcome, RenderContext, WidgetContext,
};
use crate::{
    app::{ui_state::PendingAction, RepoViewLayout},
    binding_table::{BindingModeId, KeyBindingMode, StatusContent, StatusFragment},
    keymap::Action,
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
#[derive(Clone)]
pub struct RepoData {
    pub path: PathBuf,
    pub providers: Arc<ProviderData>,
    pub labels: RepoLabels,
    pub provider_names: HashMap<String, Vec<String>>,
    pub provider_health: HashMap<String, HashMap<String, bool>>,
    pub work_items: Vec<WorkItem>,
    /// Native issue rows from `IssueViewState`. Rendered in a dedicated
    /// `SectionTable<IssueRow>` section with columns that read labels and
    /// provider name directly from the `Issue` data.
    pub issue_rows: Vec<IssueRow>,
    /// Label for the issue section header.
    pub issue_section_label: String,
    pub loading: bool,
}

// ── DoubleClickState ──

#[derive(Default)]
struct DoubleClickState {
    last_time: Option<Instant>,
    last_selectable_idx: Option<(usize, usize)>,
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
    pub table: SplitTable,
    pub preview: PreviewPanel,
    pub multi_selected: HashSet<WorkItemIdentity>,
    pub pending_actions: HashMap<WorkItemIdentity, PendingAction>,
    pub layout: RepoViewLayout,
    pub show_providers: bool,
    pub show_archived: bool,
    pub active_search_query: Option<String>,
    last_seen_generation: u64,
    double_click: DoubleClickState,
}

impl RepoPage {
    pub fn new(repo_identity: RepoIdentity, repo_data: Shared<RepoData>, layout: RepoViewLayout) -> Self {
        Self {
            repo_identity,
            repo_data,
            table: SplitTable::new(),
            preview: PreviewPanel::new(),
            multi_selected: HashSet::new(),
            pending_actions: HashMap::new(),
            layout,
            show_providers: false,
            show_archived: false,
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
        let data = self.repo_data.changed(&mut self.last_seen_generation).map(|guard| guard.clone());
        if let Some(data) = data {
            self.rebuild_table(&data);
        }
    }

    /// Rebuild the table from current data, applying the archived filter as needed.
    fn rebuild_table(&mut self, data: &RepoData) {
        let section_labels = SectionLabels {
            checkouts: data.labels.checkouts.section.clone(),
            change_requests: data.labels.change_requests.section.clone(),
            issues: data.labels.issues.section.clone(),
            sessions: data.labels.cloud_agents.section.clone(),
        };
        // Work-item sections from correlation (no longer merges issue_items).
        let sections = flotilla_core::data::group_work_items_split(&data.work_items, &data.providers, &section_labels, &data.path);
        let sections = if self.show_archived { sections } else { flotilla_core::data::filter_archived_sections(sections, &data.providers) };
        self.table.update_sections(sections);

        // Query-driven issues go into a native IssueRow section.
        self.table.update_issue_section(data.issue_section_label.clone(), data.issue_rows.clone());

        let current_identities: HashSet<WorkItemIdentity> = self.table.all_identities().collect();
        self.multi_selected.retain(|id| current_identities.contains(id));
        self.pending_actions.retain(|id, _| current_identities.contains(id));
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
        let selected_row = self.table.selected_row();
        self.preview.render_with_row(ctx.model, ctx.ui, selected_row.as_ref(), ctx.theme, frame, chunks[1]);
    }

    /// Render the table using RepoPage-owned state,
    fn render_table(&mut self, frame: &mut Frame, area: Rect, ctx: &mut RenderContext) {
        self.table.table_area = area;
        self.table.render(ctx.model, ctx.ui, ctx.theme, frame, area, self.show_providers, &self.multi_selected, &self.pending_actions);
    }

    // ── Action helpers ──

    fn dismiss(&mut self, ctx: &mut WidgetContext) -> Outcome {
        // Cancellation takes priority while a command is running for this repo.
        let active_repo = &ctx.repo_order[ctx.active_repo];
        if let Some(command_id) = ctx.in_flight.iter().filter(|(_, cmd)| &cmd.repo_identity == active_repo).map(|(id, _)| *id).max() {
            ctx.app_actions.push(AppAction::CancelCommand(command_id));
            return Outcome::Consumed;
        }

        if self.active_search_query.is_some() {
            let repo_identity = ctx.repo_order[ctx.active_repo].clone();
            self.active_search_query = None;
            ctx.app_actions.push(AppAction::ClearSearchQuery { repo: repo_identity });
        } else if self.show_providers {
            self.show_providers = false;
        } else if self.show_archived {
            self.show_archived = false;
            let data = self.repo_data.read().clone();
            self.rebuild_table(&data);
        } else if !self.multi_selected.is_empty() {
            self.multi_selected.clear();
        } else if self.table.selected_row().is_some() {
            self.table.clear_selection();
        } else {
            ctx.app_actions.push(AppAction::Quit);
        }
        Outcome::Consumed
    }

    fn toggle_multi_select(&mut self) {
        if let Some(identity) = self.table.selected_identity() {
            if !self.multi_selected.remove(&identity) {
                self.multi_selected.insert(identity);
            }
        }
    }

    pub fn select_all(&mut self) {
        for identity in self.table.all_identities() {
            self.multi_selected.insert(identity);
        }
    }
}

impl InteractiveWidget for RepoPage {
    fn handle_action(&mut self, action: Action, ctx: &mut WidgetContext) -> Outcome {
        self.reconcile_if_changed();

        match action {
            Action::SelectNext => {
                self.table.select_next();
                Outcome::Consumed
            }
            Action::SelectPrev => {
                self.table.select_prev();
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
            Action::ToggleArchived => {
                self.show_archived = !self.show_archived;
                let data = self.repo_data.read().clone();
                self.rebuild_table(&data);
                Outcome::Consumed
            }
            Action::CycleLayout => {
                // Don't cycle here — let process_app_actions be the single
                // source of truth. This handles both direct key press and
                // command palette paths.
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
            Action::OpenIssueSearch => Outcome::Push(Box::new(super::issue_search::IssueSearchWidget::new())),
            Action::OpenCommandPalette => Outcome::Push(Box::new(super::command_palette::CommandPaletteWidget::new())),
            Action::OpenContextualPalette => {
                let widget = match self.table.selected_row() {
                    Some(SelectedRow::WorkItem(item)) => match super::command_palette::palette_prefill(item) {
                        Some(prefill) => super::command_palette::CommandPaletteWidget::with_prefill(prefill, Some(item.clone())),
                        None => super::command_palette::CommandPaletteWidget::new(),
                    },
                    Some(SelectedRow::Issue(row)) => {
                        let prefill = format!("issue {} ", row.id);
                        super::command_palette::CommandPaletteWidget::with_prefill(prefill, None)
                    }
                    None => super::command_palette::CommandPaletteWidget::new(),
                };
                Outcome::Push(Box::new(widget))
            }
            // Actions handled at the App level — return Ignored so they bubble up.
            Action::Confirm | Action::OpenActionMenu | Action::OpenFilePicker | Action::Dispatch(_) => Outcome::Ignored,
            _ => Outcome::Ignored,
        }
    }

    fn handle_mouse(&mut self, mouse: MouseEvent, ctx: &mut WidgetContext) -> Outcome {
        self.reconcile_if_changed();

        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if !*ctx.is_config {
                    let x = mouse.column;
                    let y = mouse.row;

                    // Double-click detection using owned table state
                    if let Some(hit) = self.table.row_at_mouse(x, y) {
                        let now = Instant::now();
                        let is_double_click = self.double_click.last_time.map(|t| now.duration_since(t).as_millis() < 400).unwrap_or(false)
                            && self.double_click.last_selectable_idx == Some(hit);

                        if is_double_click {
                            // Select the row, then trigger double-click action
                            self.table.select_by_mouse(hit.0, hit.1);
                            ctx.app_actions.push(AppAction::ActionEnter);
                            self.double_click.last_time = None;
                            self.double_click.last_selectable_idx = None;
                            return Outcome::Consumed;
                        }

                        self.double_click.last_time = Some(now);
                        self.double_click.last_selectable_idx = Some(hit);
                    }

                    // Gear icon click (still needs ctx for the AppAction)
                    if let Some(gear_area) = self.table.gear_area {
                        if x >= gear_area.x && x < gear_area.x + gear_area.width && y >= gear_area.y && y < gear_area.y + gear_area.height {
                            ctx.app_actions.push(AppAction::ToggleProviders);
                            return Outcome::Consumed;
                        }
                    }

                    // Single click: select row using owned state
                    if let Some(hit) = self.table.row_at_mouse(x, y) {
                        self.table.select_by_mouse(hit.0, hit.1);
                        return Outcome::Consumed;
                    }
                }

                Outcome::Ignored
            }

            MouseEventKind::Down(MouseButton::Right) => {
                if !*ctx.is_config {
                    // Right-click: select row using owned state, then open action menu
                    if let Some(hit) = self.table.row_at_mouse(mouse.column, mouse.row) {
                        self.table.select_by_mouse(hit.0, hit.1);
                        ctx.app_actions.push(AppAction::OpenActionMenu);
                        return Outcome::Consumed;
                    }
                }
                Outcome::Ignored
            }

            MouseEventKind::ScrollDown => {
                if !*ctx.is_config {
                    self.table.select_next();
                    return Outcome::Consumed;
                }
                Outcome::Ignored
            }

            MouseEventKind::ScrollUp => {
                if !*ctx.is_config {
                    self.table.select_prev();
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

    fn binding_mode(&self) -> KeyBindingMode {
        if self.active_search_query.is_some() {
            KeyBindingMode::Composed(vec![BindingModeId::Normal, BindingModeId::SearchActive])
        } else {
            BindingModeId::Normal.into()
        }
    }

    fn status_fragment(&self) -> StatusFragment {
        let status = if self.show_providers {
            Some(StatusContent::Label("PROVIDERS".into()))
        } else if let Some(query) = &self.active_search_query {
            Some(StatusContent::Label(format!("SEARCH \"{query}\"")))
        } else if self.show_archived {
            Some(StatusContent::Label("ARCHIVED".into()))
        } else if !self.multi_selected.is_empty() {
            Some(StatusContent::Label(format!("{} SELECTED", self.multi_selected.len())))
        } else {
            None
        };
        StatusFragment { status }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

#[cfg(test)]
mod tests;
