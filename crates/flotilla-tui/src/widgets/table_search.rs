use std::any::Any;

use crossterm::event::KeyEvent;
use ratatui::{layout::Rect, Frame};
use tui_input::{backend::crossterm::EventHandler as InputEventHandler, Input};

use super::{AppAction, InteractiveWidget, Outcome, RenderContext, WidgetContext};
use crate::{
    binding_table::{BindingModeId, KeyBindingMode, StatusContent, StatusFragment},
    keymap::Action,
};

pub struct TableSearchWidget {
    input: Input,
}

impl TableSearchWidget {
    pub fn find(current: &str) -> Self {
        Self { input: Input::from(current) }
    }
}

impl InteractiveWidget for TableSearchWidget {
    fn handle_action(&mut self, action: Action, ctx: &mut WidgetContext) -> Outcome {
        match action {
            Action::Confirm => {
                let value = self.input.value().trim().to_string();
                ctx.app_actions.push(AppAction::SetTableFilter(value));
                Outcome::Finished
            }
            Action::Dismiss => Outcome::Finished,
            _ => Outcome::Ignored,
        }
    }

    fn handle_raw_key(&mut self, key: KeyEvent, _ctx: &mut WidgetContext) -> Outcome {
        self.input.handle_event(&crossterm::event::Event::Key(key));
        Outcome::Consumed
    }

    fn render(&mut self, _frame: &mut Frame, _area: Rect, _ctx: &mut RenderContext) {}

    fn binding_mode(&self) -> KeyBindingMode {
        BindingModeId::FindInput.into()
    }

    fn status_fragment(&self) -> StatusFragment {
        StatusFragment { status: Some(StatusContent::ActiveInput { prefix: "FIND ".into(), text: self.input.value().to_string() }) }
    }

    fn captures_raw_keys(&self) -> bool {
        true
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
    use super::*;

    #[test]
    fn find_status_identifies_the_interaction() {
        let widget = TableSearchWidget::find("needs triage");

        let Some(StatusContent::ActiveInput { prefix, text }) = widget.status_fragment().status else {
            panic!("source search should display an active input status");
        };
        assert_eq!(prefix, "FIND ");
        assert_eq!(text, "needs triage");
    }
}
