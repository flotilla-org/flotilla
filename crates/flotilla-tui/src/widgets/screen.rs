use std::{any::Any, collections::HashMap};

use crossterm::event::{KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use flotilla_protocol::{RepoIdentity, ViewAddress};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    text::Line,
    widgets::Paragraph,
    Frame,
};

use super::{
    overview_page::OverviewPage,
    repo_page::RepoPage,
    status_bar_widget::{self, StatusBarWidget},
    table::TableWidget,
    tabs::Tabs,
    AppAction, InteractiveWidget, Outcome, RenderContext, WidgetContext,
};
use crate::{
    app::{collect_visible_status_items, view_kind, ViewTarget},
    binding_table::{BindingModeId, KeyBindingMode, StatusFragment},
    keymap::Action,
    status_bar::StatusBarAction,
    ui_helpers,
};

/// What the active tab dispatches to. Derived from the active View each
/// event/frame — the View's kind, not stateful flags, decides the page.
enum ActivePage {
    Overview,
    Table,
    Repo(RepoIdentity),
    /// A repo view whose repo is no longer tracked, or an address that
    /// failed to parse: renders its own error, handles only shell actions.
    Error {
        title: String,
        detail: String,
    },
}

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
    pub table: TableWidget,
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
            table: TableWidget::default(),
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

    /// Resolve what the active View dispatches to, applying the loud-failure
    /// rule: dangling repos and unparseable addresses become error pages,
    /// never a silent fallback to another view (ADR 0013).
    fn active_page(&self, view: &crate::app::OpenView, repos: &HashMap<RepoIdentity, crate::app::TuiRepoModel>) -> ActivePage {
        match &view.target {
            ViewTarget::View(ViewAddress::Overview) => ActivePage::Overview,
            ViewTarget::View(
                ViewAddress::Convoys { .. }
                | ViewAddress::Independents
                | ViewAddress::Project { .. }
                | ViewAddress::Convoy { .. }
                | ViewAddress::Vessel { .. },
            ) => ActivePage::Table,
            ViewTarget::View(ViewAddress::Repo(identity)) => {
                if repos.contains_key(identity) {
                    ActivePage::Repo(identity.clone())
                } else {
                    ActivePage::Error {
                        title: format!("repo not tracked: {}/{}", identity.authority, identity.path),
                        detail: format!("view address: {}", ViewAddress::Repo(identity.clone())),
                    }
                }
            }
            ViewTarget::Broken { raw, error } => ActivePage::Error { title: format!("invalid view address: {raw}"), detail: error.clone() },
        }
    }

    /// Shell-level actions available on pages with no interactive widget
    /// (convoys and error pages).
    fn fallback_page_action(action: Action, ctx: &mut WidgetContext) -> Outcome {
        match action {
            Action::Quit => {
                ctx.app_actions.push(AppAction::Quit);
                Outcome::Consumed
            }
            Action::ToggleHelp => Outcome::Push(Box::new(super::help::HelpWidget::new())),
            Action::OpenCommandPalette | Action::OpenContextualPalette => {
                Outcome::Push(Box::new(super::command_palette::CommandPaletteWidget::new()))
            }
            _ => Outcome::Ignored,
        }
    }

    fn render_error_page(frame: &mut Frame, area: Rect, theme: &crate::theme::Theme, title: &str, detail: &str) {
        let lines = vec![
            Line::raw(""),
            Line::styled(format!("⚠ {title}"), ratatui::style::Style::default().fg(theme.status_error)),
            Line::raw(""),
            Line::raw(detail.to_string()),
            Line::raw(""),
            Line::raw("This tab failed on its own — other tabs are unaffected. Close it, or fix open-views.toml."),
        ];
        frame.render_widget(Paragraph::new(lines).alignment(ratatui::layout::Alignment::Center), area);
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

        // Phase 2: Global actions (only when no modal is open).
        // These are the TabPage-level bindings that apply on every top-level tab.
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
            Action::CloseTab => {
                ctx.app_actions.push(AppAction::CloseActiveTab);
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

        // Phase 3: No modal — dispatch to the active View's page.
        let outcome = match self.active_page(ctx.views.active(), &ctx.model.repos) {
            ActivePage::Overview => self.overview_page.handle_action(action, ctx),
            ActivePage::Table => {
                let outcome = self.table.handle_action(action, ctx);
                if matches!(outcome, Outcome::Ignored) {
                    Self::fallback_page_action(action, ctx)
                } else {
                    outcome
                }
            }
            ActivePage::Error { .. } => Self::fallback_page_action(action, ctx),
            ActivePage::Repo(identity) => match self.repo_pages.get_mut(&identity) {
                Some(page) => page.handle_action(action, ctx),
                None => Self::fallback_page_action(action, ctx),
            },
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

        // No modal — dispatch to the active View's page
        let outcome = match self.active_page(ctx.views.active(), &ctx.model.repos) {
            ActivePage::Overview => self.overview_page.handle_raw_key(key, ctx),
            ActivePage::Table | ActivePage::Error { .. } => Outcome::Ignored,
            ActivePage::Repo(identity) => match self.repo_pages.get_mut(&identity) {
                Some(page) => page.handle_raw_key(key, ctx),
                None => Outcome::Ignored,
            },
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
            MouseEventKind::Drag(MouseButton::Left) if self.tabs.drag.dragging_tab.is_some() => {
                let tab_actions = self.tabs.handle_mouse(mouse);
                if !tab_actions.is_empty() {
                    ctx.app_actions.extend(tab_actions);
                }
                return Outcome::Consumed;
            }
            MouseEventKind::Up(MouseButton::Left) if self.tabs.drag.dragging_tab.is_some() => {
                let tab_actions = self.tabs.handle_mouse(mouse);
                if !tab_actions.is_empty() {
                    ctx.app_actions.extend(tab_actions);
                }
                return Outcome::Consumed;
            }
            _ => {}
        }

        // Dispatch to the active View's page for content area mouse events
        let outcome = match self.active_page(ctx.views.active(), &ctx.model.repos) {
            ActivePage::Overview => self.overview_page.handle_mouse(mouse, ctx),
            ActivePage::Table => self.table.handle_mouse(mouse, ctx),
            ActivePage::Error { .. } => Outcome::Ignored,
            ActivePage::Repo(identity) => match self.repo_pages.get_mut(&identity) {
                Some(page) => page.handle_mouse(mouse, ctx),
                None => Outcome::Ignored,
            },
        };
        if !matches!(outcome, Outcome::Ignored) {
            self.apply_outcome(outcome);
            return Outcome::Consumed;
        }
        Outcome::Ignored
    }

    fn render(&mut self, frame: &mut Frame, _area: Rect, ctx: &mut RenderContext) {
        // Scoped mode: the pane is one View — no tab bar row.
        let constraints = if ctx.views.is_scoped() {
            vec![Constraint::Length(0), Constraint::Min(0), Constraint::Length(1)]
        } else {
            vec![Constraint::Length(1), Constraint::Min(0), Constraint::Length(1)]
        };
        let chunks = Layout::default().direction(Direction::Vertical).constraints(constraints).split(frame.area());

        // 1. Tab bar
        if !ctx.views.is_scoped() {
            self.tabs.render(ctx.views, ctx.model, ctx.ui, ctx.theme, frame, chunks[0]);
        }

        // 2. Content: dispatch to the active View's page
        let active_page = self.active_page(ctx.views.active(), &ctx.model.repos);
        match &active_page {
            ActivePage::Table => {
                let address = ctx.views.active_address().cloned().expect("table page has a parsed address");
                let filter = ctx.views.active_table_state().filter.clone();
                let rows = crate::app::table_rows(ctx.namespaces);
                match crate::table_view::project(&address, &rows).map(|view| view.filtered(&filter)) {
                    Ok(view) => {
                        let breadcrumbs =
                            ctx.views.active().breadcrumb_addresses().into_iter().map(ToString::to_string).collect::<Vec<_>>();
                        self.table.render_table(frame, chunks[1], ctx.theme, &view, ctx.views.active_table_state_mut(), &breadcrumbs);
                    }
                    Err(detail) => Self::render_error_page(frame, chunks[1], ctx.theme, "table view unavailable", &detail),
                }
            }
            ActivePage::Overview => self.overview_page.render(frame, chunks[1], ctx),
            ActivePage::Repo(identity) => {
                let identity = identity.clone();
                match self.repo_pages.get_mut(&identity) {
                    Some(page) => page.render(frame, chunks[1], ctx),
                    None => Self::render_error_page(
                        frame,
                        chunks[1],
                        ctx.theme,
                        &format!("no page for repo: {}/{}", identity.authority, identity.path),
                        "",
                    ),
                }
            }
            ActivePage::Error { title, detail } => Self::render_error_page(frame, chunks[1], ctx.theme, title, detail),
        }

        // 3. Status bar — resolve all content, then call the pure renderer.

        // 3a. Resolve binding mode and status fragment.
        // Modal stack takes priority; otherwise the active View's kind
        // supplies its mode stack.
        let scoped = ctx.views.is_scoped();
        let address = ctx.views.active().address();
        let (binding_mode, fragment) = if let Some(modal) = self.modal_stack.last() {
            (modal.binding_mode(), modal.status_fragment())
        } else {
            // Kind-level modes come from the page widget when it carries
            // state (overview, repo pages), from `view_kind` otherwise; the
            // shell layer (TabPage, TabShell unless scoped) composes here.
            match &active_page {
                ActivePage::Overview => {
                    (view_kind::compose_with_shell(scoped, self.overview_page.binding_mode().modes()), self.overview_page.status_fragment())
                }
                ActivePage::Repo(identity) => match self.repo_pages.get(identity) {
                    Some(page) => (view_kind::compose_with_shell(scoped, page.binding_mode().modes()), page.status_fragment()),
                    None => (view_kind::compose_with_shell(scoped, []), StatusFragment::default()),
                },
                ActivePage::Table => (view_kind::binding_mode(address, scoped), StatusFragment::default()),
                ActivePage::Error { .. } => (view_kind::compose_with_shell(scoped, []), StatusFragment::default()),
            }
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

        self.status_bar.render_bespoke(
            status,
            key_chips,
            task,
            error_items,
            mode_indicators,
            show_keys,
            ctx.ui.command_echo.as_deref(),
            ctx.theme,
            frame,
            status_bar_area,
        );

        // 4. Modals on top
        for modal in &mut self.modal_stack {
            modal.render(frame, frame.area(), ctx);
        }
    }

    fn binding_mode(&self) -> KeyBindingMode {
        self.modal_stack
            .last()
            .map(|w| w.binding_mode())
            .unwrap_or_else(|| KeyBindingMode::Composed(vec![BindingModeId::TabPage, BindingModeId::Normal]))
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
