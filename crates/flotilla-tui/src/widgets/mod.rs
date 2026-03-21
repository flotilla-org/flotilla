pub mod action_menu;
pub mod branch_input;
pub mod close_confirm;
pub mod command_palette;
pub mod delete_confirm;
pub mod event_log;
pub mod file_picker;
pub mod help;
pub mod issue_search;
pub mod overview_page;
pub mod preview_panel;
pub mod repo_page;
pub mod screen;
pub mod status_bar_widget;
pub mod tabs;
pub mod work_item_table;

use std::{any::Any, collections::HashMap};

use crossterm::event::{KeyEvent, MouseEvent};
use flotilla_core::config::ConfigStore;
use flotilla_protocol::{HostName, RepoIdentity};
use ratatui::{layout::Rect, Frame};

use crate::{
    app::{ui_state::UiMode, CommandQueue, InFlightCommand, RepoUiState, TuiModel, UiState},
    binding_table::{BindingModeId, KeyBindingMode, StatusFragment},
    keymap::{Action, Keymap},
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
    ActionEnter,
    StatusBarKeyPress { code: crossterm::event::KeyCode, modifiers: crossterm::event::KeyModifiers },
    ClearError(usize),
    SwitchToConfig,
    SwitchToRepo(usize),
    SaveTabOrder,
    OpenFilePicker,
    PrevTab,
    NextTab,
    MoveTabLeft,
    MoveTabRight,
    Refresh,
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

/// Context provided to widgets during rendering.
///
/// Mutable fields (`ui`) are needed because the base layer rendering updates
/// table state and layout areas.
pub struct RenderContext<'a> {
    pub model: &'a TuiModel,
    pub ui: &'a mut UiState,
    pub theme: &'a Theme,
    pub keymap: &'a Keymap,
    pub in_flight: &'a HashMap<u64, InFlightCommand>,
    /// The mode of the topmost widget on the stack. Used by the status bar
    /// to show the correct key hints.
    pub active_widget_mode: Option<BindingModeId>,
    /// Extra data from the active widget for status bar rendering.
    pub active_widget_data: StatusFragment,
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
    fn render(&mut self, frame: &mut Frame, area: Rect, ctx: &mut RenderContext);

    /// The binding mode for keymap resolution.
    fn binding_mode(&self) -> KeyBindingMode;

    /// Whether this widget needs raw key events instead of resolved actions.
    fn captures_raw_keys(&self) -> bool {
        false
    }

    /// Widget-provided status content for the status bar.
    ///
    /// The default returns an empty `StatusFragment`. Modal widgets should
    /// override this to provide mode-specific labels or input text.
    fn status_fragment(&self) -> StatusFragment {
        StatusFragment::default()
    }

    /// Downcast support for reading widget state from outside the trait.
    fn as_any(&self) -> &dyn Any;

    /// Downcast support for updating widget state from outside the trait.
    fn as_any_mut(&mut self) -> &mut dyn Any;
}
