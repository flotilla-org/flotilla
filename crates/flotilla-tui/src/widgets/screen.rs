use std::{any::Any, collections::HashMap};

use crossterm::event::{KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use flotilla_protocol::RepoIdentity;
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    Frame,
};

use super::{
    overview_page::OverviewPage,
    repo_page::RepoPage,
    status_bar_widget::{self, StatusBarWidget},
    tabs::Tabs,
    AppAction, InteractiveWidget, Outcome, RenderContext, WidgetContext,
};
use crate::{
    app::collect_visible_status_items,
    binding_table::{BindingModeId, KeyBindingMode, StatusFragment},
    keymap::Action,
    status_bar::StatusBarAction,
    ui_helpers,
};

/// Root widget that owns the tab bar, page content, status bar, and modal stack.
///
/// Renders the tab bar (via `Tabs`), page content (repo pages or overview
/// page), status bar, and then any modals on top. Owns the `has_modal()`,
/// `dismiss_modals()`, and `apply_outcome()` helpers that previously lived
/// on `App`.
///
/// Modal dispatch is handled internally: `handle_action`, `handle_raw_key`,
/// and `handle_mouse` route events to the top modal when one exists, with
/// modals acting as focus barriers (unhandled events do NOT fall through
/// to the page layer).
pub struct Screen {
    pub tabs: Tabs,
    pub status_bar: StatusBarWidget,
    pub modal_stack: Vec<Box<dyn InteractiveWidget>>,
    pub repo_pages: HashMap<RepoIdentity, RepoPage>,
    pub overview_page: OverviewPage,
}

impl Default for Screen {
    fn default() -> Self {
        Self::new()
    }
}

impl Screen {
    pub fn new() -> Self {
        Self {
            tabs: Tabs::new(),
            status_bar: StatusBarWidget::new(),
            modal_stack: Vec::new(),
            repo_pages: HashMap::new(),
            overview_page: OverviewPage::new(),
        }
    }

    /// Returns true if a modal widget is on the stack above the base layer.
    pub fn has_modal(&self) -> bool {
        !self.modal_stack.is_empty()
    }

    /// Pop all modal widgets from the stack.
    /// Called when the user switches tabs or navigates away, so stale modals
    /// don't linger across context changes.
    pub fn dismiss_modals(&mut self) {
        self.modal_stack.clear();
    }

    /// Apply a widget outcome from event dispatch.
    ///
    /// Since modals are always on top, `Finished` pops the top modal,
    /// `Push` pushes a new modal, and `Swap` replaces the top modal.
    /// If the outcome originated from a page widget (no modals), `Push`
    /// still pushes onto the modal_stack.
    pub fn apply_outcome(&mut self, outcome: Outcome) {
        match outcome {
            Outcome::Consumed | Outcome::Ignored => {}
            Outcome::Finished => {
                self.modal_stack.pop();
            }
            Outcome::Push(widget) => {
                self.modal_stack.push(widget);
            }
            Outcome::Swap(widget) => {
                self.modal_stack.pop();
                self.modal_stack.push(widget);
            }
        }
    }

    /// Resolve the active repo identity from model state.
    ///
    /// Returns `Some(identity)` when the UI is on a repo tab (not Config mode),
    /// `None` when on the Flotilla (overview) tab.
    fn active_repo_identity<'a>(&self, repo_order: &'a [RepoIdentity], active_repo: usize, is_config: bool) -> Option<&'a RepoIdentity> {
        if is_config || repo_order.is_empty() {
            None
        } else {
            repo_order.get(active_repo)
        }
    }
}

impl InteractiveWidget for Screen {
    fn handle_action(&mut self, action: Action, ctx: &mut WidgetContext) -> Outcome {
        // Phase 1: Modal dispatch — modals are focus barriers that trap all input,
        // including global actions like tab switching and theme cycling.
        if let Some(modal) = self.modal_stack.last_mut() {
            let outcome = modal.handle_action(action, ctx);
            if !matches!(outcome, Outcome::Ignored) {
                self.apply_outcome(outcome);
                return Outcome::Consumed;
            }
            // Modal is a focus barrier — don't fall through to globals or base
            return Outcome::Ignored;
        }

        // Phase 2: Global actions (only when no modal is open)
        match action {
            Action::PrevTab => {
                ctx.app_actions.push(AppAction::PrevTab);
                return Outcome::Consumed;
            }
            Action::NextTab => {
                ctx.app_actions.push(AppAction::NextTab);
                return Outcome::Consumed;
            }
            Action::MoveTabLeft => {
                ctx.app_actions.push(AppAction::MoveTabLeft);
                return Outcome::Consumed;
            }
            Action::MoveTabRight => {
                ctx.app_actions.push(AppAction::MoveTabRight);
                return Outcome::Consumed;
            }
            Action::CycleTheme => {
                ctx.app_actions.push(AppAction::CycleTheme);
                return Outcome::Consumed;
            }
            Action::CycleHost => {
                ctx.app_actions.push(AppAction::CycleHost);
                return Outcome::Consumed;
            }
            Action::ToggleDebug => {
                ctx.app_actions.push(AppAction::ToggleDebug);
                return Outcome::Consumed;
            }
            Action::ToggleStatusBarKeys => {
                ctx.app_actions.push(AppAction::ToggleStatusBarKeys);
                return Outcome::Consumed;
            }
            Action::Refresh => {
                ctx.app_actions.push(AppAction::Refresh);
                return Outcome::Consumed;
            }
            _ => {}
        }

        // Phase 3: No modal — dispatch to overview page or repo page
        let is_config = *ctx.is_config;
        let active_identity = self.active_repo_identity(ctx.repo_order, ctx.active_repo, is_config).cloned();
        let outcome = if let Some(ref identity) = active_identity {
            if let Some(page) = self.repo_pages.get_mut(identity) {
                page.handle_action(action, ctx)
            } else {
                self.overview_page.handle_action(action, ctx)
            }
        } else {
            self.overview_page.handle_action(action, ctx)
        };
        if !matches!(outcome, Outcome::Ignored) {
            self.apply_outcome(outcome);
            return Outcome::Consumed;
        }
        Outcome::Ignored
    }

    fn handle_raw_key(&mut self, key: KeyEvent, ctx: &mut WidgetContext) -> Outcome {
        // Modal dispatch first
        if let Some(modal) = self.modal_stack.last_mut() {
            let outcome = modal.handle_raw_key(key, ctx);
            if !matches!(outcome, Outcome::Ignored) {
                self.apply_outcome(outcome);
                return Outcome::Consumed;
            }
            return Outcome::Ignored;
        }

        // No modal — dispatch to overview page or repo page
        let is_config = *ctx.is_config;
        let active_identity = self.active_repo_identity(ctx.repo_order, ctx.active_repo, is_config).cloned();
        let outcome = if let Some(ref identity) = active_identity {
            if let Some(page) = self.repo_pages.get_mut(identity) {
                page.handle_raw_key(key, ctx)
            } else {
                self.overview_page.handle_raw_key(key, ctx)
            }
        } else {
            self.overview_page.handle_raw_key(key, ctx)
        };
        if !matches!(outcome, Outcome::Ignored) {
            self.apply_outcome(outcome);
            return Outcome::Consumed;
        }
        Outcome::Ignored
    }

    fn handle_mouse(&mut self, mouse: MouseEvent, ctx: &mut WidgetContext) -> Outcome {
        // Modal dispatch first — modals are focus barriers
        if let Some(modal) = self.modal_stack.last_mut() {
            let outcome = modal.handle_mouse(mouse, ctx);
            if !matches!(outcome, Outcome::Ignored) {
                self.apply_outcome(outcome);
                return Outcome::Consumed;
            }
            return Outcome::Ignored;
        }

        // No modal — handle tab bar mouse events first
        let x = mouse.column;
        let y = mouse.row;

        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                // Tab bar click
                let tab_actions = self.tabs.handle_mouse(mouse);
                if !tab_actions.is_empty() {
                    ctx.app_actions.extend(tab_actions);
                    return Outcome::Consumed;
                }

                // Status bar click
                if let Some(sb_action) = self.status_bar.handle_click(x, y) {
                    match sb_action {
                        StatusBarAction::KeyPress { code, modifiers } => {
                            ctx.app_actions.push(AppAction::StatusBarKeyPress { code, modifiers });
                        }
                        StatusBarAction::ClearError(id) => {
                            ctx.app_actions.push(AppAction::ClearError(id));
                        }
                        StatusBarAction::None => {}
                    }
                    return Outcome::Consumed;
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if self.tabs.drag.dragging_tab.is_some() {
                    let tab_actions = self.tabs.handle_mouse(mouse);
                    if !tab_actions.is_empty() {
                        ctx.app_actions.extend(tab_actions);
                    }
                    return Outcome::Consumed;
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                if self.tabs.drag.dragging_tab.is_some() {
                    let tab_actions = self.tabs.handle_mouse(mouse);
                    if !tab_actions.is_empty() {
                        ctx.app_actions.extend(tab_actions);
                    }
                    return Outcome::Consumed;
                }
            }
            _ => {}
        }

        // Dispatch to overview page or repo page for content area mouse events
        let is_config = *ctx.is_config;
        let active_identity = self.active_repo_identity(ctx.repo_order, ctx.active_repo, is_config).cloned();
        let outcome = if let Some(ref identity) = active_identity {
            if let Some(page) = self.repo_pages.get_mut(identity) {
                page.handle_mouse(mouse, ctx)
            } else {
                self.overview_page.handle_mouse(mouse, ctx)
            }
        } else {
            self.overview_page.handle_mouse(mouse, ctx)
        };
        if !matches!(outcome, Outcome::Ignored) {
            self.apply_outcome(outcome);
            return Outcome::Consumed;
        }
        Outcome::Ignored
    }

    fn render(&mut self, frame: &mut Frame, _area: Rect, ctx: &mut RenderContext) {
        let constraints = vec![Constraint::Length(1), Constraint::Min(0), Constraint::Length(1)];
        let chunks = Layout::default().direction(Direction::Vertical).constraints(constraints).split(frame.area());

        // 1. Tab bar
        self.tabs.render(ctx.model, ctx.ui, ctx.theme, frame, chunks[0]);

        // 2. Content: dispatch to repo page for repo tabs, overview page otherwise
        let is_config = ctx.ui.is_config;
        let active_identity = self.active_repo_identity(&ctx.model.repo_order, ctx.model.active_repo, is_config).cloned();
        if let Some(ref identity) = active_identity {
            if let Some(page) = self.repo_pages.get_mut(identity) {
                page.render(frame, chunks[1], ctx);
            } else {
                self.overview_page.render(frame, chunks[1], ctx);
            }
        } else {
            self.overview_page.render(frame, chunks[1], ctx);
        }

        // 3. Status bar — resolve all content, then call the pure renderer.

        // 3a. Resolve binding mode and status fragment.
        // Modal stack takes priority; otherwise ask the active page directly.
        let (binding_mode, fragment) = if let Some(modal) = self.modal_stack.last() {
            (modal.binding_mode(), modal.status_fragment())
        } else if is_config {
            (self.overview_page.binding_mode(), self.overview_page.status_fragment())
        } else if let Some(page) = active_identity.as_ref().and_then(|id| self.repo_pages.get(id)) {
            (page.binding_mode(), page.status_fragment())
        } else {
            (BindingModeId::Normal.into(), StatusFragment::default())
        };

        let active_mode = binding_mode.primary();

        // 3b. Resolve key chips from binding mode via compiled binding table.
        //     Progress fragments suppress key chips (user can't interact during progress).
        let key_chips = if matches!(fragment.status, Some(crate::binding_table::StatusContent::Progress { .. })) {
            vec![]
        } else {
            ctx.keymap.hints_for(&binding_mode)
        };

        // 3c. Resolve status section from fragment (with fallback)
        let fallback_label = "/ for commands";
        let status = status_bar_widget::resolve_status_section(&fragment, fallback_label);

        // 3d. Task spinner — fragment progress takes priority over in-flight commands.
        //     Only Normal/Overview modes show in-flight tasks.
        let task = status_bar_widget::resolve_task_from_fragment(&fragment).or_else(|| {
            if self.modal_stack.is_empty() {
                status_bar_widget::active_task(ctx.model, ctx.in_flight)
            } else {
                None
            }
        });

        // 3e. Error items — only override status in Normal mode (no modals)
        let error_items = if active_mode == BindingModeId::Normal && self.modal_stack.is_empty() {
            collect_visible_status_items(ctx.model, ctx.ui)
        } else {
            vec![]
        };

        // 3f. Mode indicators — only for Normal mode (no modals, not config or issue search)
        let mode_indicators = if active_mode == BindingModeId::Normal && self.modal_stack.is_empty() {
            status_bar_widget::normal_mode_indicators(ctx.ui)
        } else if active_mode == BindingModeId::CommandPalette {
            // CommandPalette keeps mode indicators
            status_bar_widget::normal_mode_indicators(ctx.ui)
        } else {
            vec![]
        };

        // 3g. show_keys flag
        let show_keys = ctx.ui.status_bar.show_keys;

        // 3h. Status bar area — CommandPalette moves it to the overlay position
        let is_command_palette = self.modal_stack.last().map(|w| w.binding_mode().primary()) == Some(BindingModeId::CommandPalette);
        let status_bar_area = if is_command_palette {
            ui_helpers::bottom_anchored_overlay(frame.area(), 1, crate::palette::MAX_PALETTE_ROWS as u16).status_row
        } else {
            chunks[2]
        };

        self.status_bar.render_bespoke(status, key_chips, task, error_items, mode_indicators, show_keys, ctx.theme, frame, status_bar_area);

        // 4. Modals on top
        for modal in &mut self.modal_stack {
            modal.render(frame, frame.area(), ctx);
        }
    }

    fn binding_mode(&self) -> KeyBindingMode {
        self.modal_stack.last().map(|w| w.binding_mode()).unwrap_or_else(|| BindingModeId::Normal.into())
    }

    fn captures_raw_keys(&self) -> bool {
        self.modal_stack.last().map(|w| w.captures_raw_keys()).unwrap_or(false)
    }

    fn status_fragment(&self) -> StatusFragment {
        self.modal_stack.last().map(|w| w.status_fragment()).unwrap_or_default()
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}
