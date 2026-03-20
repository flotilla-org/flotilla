use std::any::Any;

use crossterm::event::{KeyEvent, MouseEvent};
use ratatui::{layout::Rect, Frame};

use super::{base_view::BaseView, AppAction, InteractiveWidget, Outcome, RenderContext, WidgetContext, WidgetStatusData};
use crate::keymap::{Action, ModeId};

/// Root widget that owns the base layer and the modal stack.
///
/// Renders the base view first, then any modals on top. Owns the
/// `has_modal()`, `dismiss_modals()`, and `apply_outcome()` helpers
/// that previously lived on `App`.
pub struct Screen {
    pub base_view: BaseView,
    pub modal_stack: Vec<Box<dyn InteractiveWidget>>,
}

impl Default for Screen {
    fn default() -> Self {
        Self::new()
    }
}

impl Screen {
    pub fn new() -> Self {
        Self { base_view: BaseView::new(), modal_stack: Vec::new() }
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
    /// If the outcome originated from the base_view (no modals), `Push`
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

    /// The mode of the topmost widget (modal or base_view).
    pub fn active_mode_id(&self) -> Option<ModeId> {
        self.modal_stack.last().map(|w| w.mode_id()).or(Some(self.base_view.mode_id()))
    }

    /// Extra status data from the topmost widget.
    pub fn active_status_data(&self) -> WidgetStatusData {
        self.modal_stack.last().map(|w| w.status_data()).unwrap_or_default()
    }
}

impl InteractiveWidget for Screen {
    fn handle_action(&mut self, action: Action, ctx: &mut WidgetContext) -> Outcome {
        // Phase 1: Global actions
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
        // Phase 2: Delegate to BaseView
        self.base_view.handle_action(action, ctx)
    }

    fn handle_raw_key(&mut self, key: KeyEvent, ctx: &mut WidgetContext) -> Outcome {
        self.base_view.handle_raw_key(key, ctx)
    }

    fn handle_mouse(&mut self, mouse: MouseEvent, ctx: &mut WidgetContext) -> Outcome {
        self.base_view.handle_mouse(mouse, ctx)
    }

    fn render(&mut self, frame: &mut Frame, area: Rect, ctx: &mut RenderContext) {
        self.base_view.render(frame, area, ctx);
        for modal in &mut self.modal_stack {
            modal.render(frame, area, ctx);
        }
    }

    fn mode_id(&self) -> ModeId {
        self.base_view.mode_id()
    }

    fn captures_raw_keys(&self) -> bool {
        self.base_view.captures_raw_keys()
    }

    fn status_data(&self) -> WidgetStatusData {
        self.base_view.status_data()
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}
