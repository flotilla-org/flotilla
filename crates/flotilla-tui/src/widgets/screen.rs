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
    convoys_page::ConvoysPage,
    overview_page::OverviewPage,
    repo_page::RepoPage,
    status_bar_widget::{self, StatusBarWidget},
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
    /// A convoy list view: the convoys of a namespace, or of one project.
    Convoys {
        /// Set for project dashboards — scopes the list and its empty state.
        project: Option<String>,
    },
    /// One convoy's vessel tree, full width.
    Convoy {
        namespace: String,
        name: String,
    },
    /// One vessel's detail.
    Vessel {
        namespace: String,
        convoy: String,
        vessel: String,
    },
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

    /// Resolve what the active View dispatches to, applying the loud-failure
    /// rule: dangling repos and unparseable addresses become error pages,
    /// never a silent fallback to another view (ADR 0013).
    fn active_page(&self, view: &crate::app::OpenView, repos: &HashMap<RepoIdentity, crate::app::TuiRepoModel>) -> ActivePage {
        match &view.target {
            ViewTarget::View(ViewAddress::Overview) => ActivePage::Overview,
            ViewTarget::View(ViewAddress::Convoys { .. }) => ActivePage::Convoys { project: None },
            // Project dashboards render the convoy list scoped to the
            // project (the scope filter is applied upstream, in the
            // pre-filtered `RenderContext::convoys`).
            ViewTarget::View(ViewAddress::Project { name, .. }) => ActivePage::Convoys { project: Some(name.clone()) },
            ViewTarget::View(ViewAddress::Convoy { namespace, name }) => {
                ActivePage::Convoy { namespace: namespace.clone(), name: name.clone() }
            }
            ViewTarget::View(ViewAddress::Vessel { namespace, convoy, vessel }) => {
                ActivePage::Vessel { namespace: namespace.clone(), convoy: convoy.clone(), vessel: vessel.clone() }
            }
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
            // Convoy-family and error pages have no interactive widget —
            // only the shell-level actions apply here (convoy navigation is
            // App-level dispatch).
            ActivePage::Convoys { .. } | ActivePage::Convoy { .. } | ActivePage::Vessel { .. } | ActivePage::Error { .. } => {
                Self::fallback_page_action(action, ctx)
            }
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
            ActivePage::Convoys { .. } | ActivePage::Convoy { .. } | ActivePage::Vessel { .. } | ActivePage::Error { .. } => {
                Outcome::Ignored
            }
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
            ActivePage::Convoys { .. } | ActivePage::Convoy { .. } | ActivePage::Vessel { .. } | ActivePage::Error { .. } => {
                Outcome::Ignored
            }
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
            ActivePage::Convoys { project } => {
                let selected = ctx.convoys_selected.as_ref();
                ConvoysPage {
                    convoys: ctx.convoys.clone(),
                    selected,
                    selected_vessel: ctx.convoys_selected_vessel,
                    focus: ctx.convoys_focus,
                    filter: ctx.convoy_filter,
                    project: project.as_deref(),
                }
                .render(frame, chunks[1]);
            }
            ActivePage::Overview => self.overview_page.render(frame, chunks[1], ctx),
            ActivePage::Convoy { namespace, name } => {
                match ctx.namespaces.get(namespace).and_then(|m| m.convoys.values().find(|c| &c.name == name)) {
                    Some(convoy) => {
                        super::convoys_page::ConvoyDetail { convoy, selected_vessel: ctx.convoys_selected_vessel, focused: true }
                            .render(frame, chunks[1])
                    }
                    None => Self::render_error_page(
                        frame,
                        chunks[1],
                        ctx.theme,
                        &format!("convoy not found: {namespace}/{name}"),
                        "It may not have started yet, or it has been deleted.",
                    ),
                }
            }
            ActivePage::Vessel { namespace, convoy, vessel } => {
                let found = ctx
                    .namespaces
                    .get(namespace)
                    .and_then(|m| m.convoys.values().find(|c| &c.name == convoy))
                    .and_then(|c| c.vessels.iter().find(|v| &v.name == vessel).map(|v| (c, v)));
                match found {
                    Some((convoy, vessel)) => super::convoys_page::VesselDetail { convoy, vessel }.render(frame, chunks[1]),
                    None => Self::render_error_page(
                        frame,
                        chunks[1],
                        ctx.theme,
                        &format!("vessel not found: {namespace}/{convoy}/{vessel}"),
                        "It may not have materialised yet, or its convoy has been deleted.",
                    ),
                }
            }
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
                ActivePage::Convoys { .. } | ActivePage::Convoy { .. } | ActivePage::Vessel { .. } => {
                    (view_kind::binding_mode(address, ctx.convoys_focus, scoped), StatusFragment::default())
                }
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
