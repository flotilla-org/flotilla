pub mod action_menu;
pub mod branch_input;
pub mod close_confirm;
pub mod command_palette;
pub mod delete_confirm;
pub mod file_picker;
pub mod help;
pub mod issue_search;
pub mod work_item_table;

use std::{any::Any, collections::HashMap};

use crossterm::event::{KeyEvent, MouseEvent};
use flotilla_core::config::ConfigStore;
use flotilla_protocol::{HostName, RepoIdentity};
use ratatui::{layout::Rect, Frame};

use crate::{
    app::{ui_state::UiMode, CommandQueue, InFlightCommand, RepoUiState, TuiModel},
    keymap::{Action, Keymap, ModeId},
    theme::Theme,
};

/// App-level effects that widgets can request. Processed by the event
/// loop after widget dispatch — widgets declare intent, the app executes.
#[derive(Debug, Clone)]
pub enum AppAction {
    Quit,
    CancelCommand(u64),
    CycleTheme,
    CycleLayout,
    CycleHost,
    ToggleDebug,
    ToggleStatusBarKeys,
    ToggleProviders,
    ToggleMultiSelect,
    OpenActionMenu,
}

/// Result of handling an event in a widget.
pub enum Outcome {
    /// Event was handled; no further dispatch needed.
    Consumed,
    /// Event was not handled; try the next widget in the stack.
    Ignored,
    /// Widget is done; pop it from the stack.
    Finished,
    /// Push a new widget on top of the current one.
    Push(Box<dyn InteractiveWidget>),
    /// Pop the current widget and push a replacement.
    Swap(Box<dyn InteractiveWidget>),
}

/// Mutable context provided to widgets during event handling.
pub struct WidgetContext<'a> {
    pub model: &'a TuiModel,
    pub keymap: &'a Keymap,
    pub config: &'a ConfigStore,
    pub in_flight: &'a HashMap<u64, InFlightCommand>,
    pub target_host: Option<&'a HostName>,
    pub active_repo: usize,
    pub repo_order: &'a [RepoIdentity],
    pub commands: &'a mut CommandQueue,
    pub repo_ui: &'a mut HashMap<RepoIdentity, RepoUiState>,
    pub mode: &'a mut UiMode,
    pub app_actions: Vec<AppAction>,
}

/// Read-only context provided to widgets during rendering.
pub struct RenderContext<'a> {
    pub model: &'a TuiModel,
    pub theme: &'a Theme,
    pub keymap: &'a Keymap,
    pub in_flight: &'a HashMap<u64, InFlightCommand>,
}

/// A self-contained interactive widget that handles events and renders itself.
pub trait InteractiveWidget {
    /// Handle a resolved keymap action.
    fn handle_action(&mut self, action: Action, ctx: &mut WidgetContext) -> Outcome;

    /// Handle a raw key event (for text input widgets that need every keystroke).
    fn handle_raw_key(&mut self, _key: KeyEvent, _ctx: &mut WidgetContext) -> Outcome {
        Outcome::Ignored
    }

    /// Handle a mouse event.
    fn handle_mouse(&mut self, _mouse: MouseEvent, _ctx: &mut WidgetContext) -> Outcome {
        Outcome::Ignored
    }

    /// Render the widget into the given area.
    fn render(&mut self, frame: &mut Frame, area: Rect, ctx: &RenderContext);

    /// The mode identifier for keymap resolution.
    fn mode_id(&self) -> ModeId;

    /// Whether this widget needs raw key events instead of resolved actions.
    fn captures_raw_keys(&self) -> bool {
        false
    }

    /// Downcast support for updating widget state from outside the trait.
    fn as_any_mut(&mut self) -> &mut dyn Any;
}
