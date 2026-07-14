use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    layout::Rect,
    widgets::{Block, Clear, List, ListItem, ListState},
    Frame,
};

use super::{AppAction, InteractiveWidget, Outcome, RenderContext, WidgetContext};
use crate::{
    binding_table::{BindingModeId, KeyBindingMode, StatusContent, StatusFragment},
    keymap::Action,
    table_view::AvailableAction,
    ui_helpers,
};

pub struct TableActionMenuWidget {
    actions: Vec<AvailableAction>,
    index: usize,
    area: Rect,
}

impl TableActionMenuWidget {
    pub fn new(actions: Vec<AvailableAction>) -> Self {
        Self { actions, index: 0, area: Rect::default() }
    }

    fn confirm(&self, ctx: &mut WidgetContext) -> Outcome {
        if let Some(action) = self.actions.get(self.index) {
            ctx.app_actions.push(AppAction::ExecuteTableIntent(action.intent.clone()));
        }
        Outcome::Finished
    }
}

impl InteractiveWidget for TableActionMenuWidget {
    fn handle_action(&mut self, action: Action, ctx: &mut WidgetContext) -> Outcome {
        match action {
            Action::SelectNext => {
                self.index = (self.index + 1).min(self.actions.len().saturating_sub(1));
                Outcome::Consumed
            }
            Action::SelectPrev => {
                self.index = self.index.saturating_sub(1);
                Outcome::Consumed
            }
            Action::Confirm => self.confirm(ctx),
            Action::Dismiss => Outcome::Finished,
            _ => Outcome::Ignored,
        }
    }

    fn handle_raw_key(&mut self, key: KeyEvent, ctx: &mut WidgetContext) -> Outcome {
        match key.code {
            KeyCode::Char(shortcut) => match self.actions.iter().position(|action| action.key == shortcut) {
                Some(index) => {
                    self.index = index;
                    self.confirm(ctx)
                }
                None => Outcome::Ignored,
            },
            _ => Outcome::Ignored,
        }
    }

    fn handle_mouse(&mut self, mouse: MouseEvent, ctx: &mut WidgetContext) -> Outcome {
        if mouse.kind != MouseEventKind::Down(MouseButton::Left) {
            return Outcome::Ignored;
        }
        if mouse.column < self.area.x
            || mouse.column >= self.area.x.saturating_add(self.area.width)
            || mouse.row < self.area.y
            || mouse.row >= self.area.y.saturating_add(self.area.height)
        {
            return Outcome::Finished;
        }
        let index = mouse.row.saturating_sub(self.area.y).saturating_sub(1) as usize;
        if index < self.actions.len() {
            self.index = index;
            self.confirm(ctx)
        } else {
            Outcome::Consumed
        }
    }

    fn render(&mut self, frame: &mut Frame, area: Rect, ctx: &mut RenderContext) {
        self.area = ui_helpers::popup_area(area, 44, 36);
        frame.render_widget(Clear, self.area);
        let items = self.actions.iter().map(|action| ListItem::new(format!(" [{}] {}", action.key, action.label)));
        let list = List::new(items)
            .block(Block::bordered().style(ctx.theme.block_style()).title(" Actions "))
            .highlight_style(ratatui::style::Style::default().bg(ctx.theme.action_highlight));
        let mut state = ListState::default().with_selected(Some(self.index));
        frame.render_stateful_widget(list, self.area, &mut state);
    }

    fn binding_mode(&self) -> KeyBindingMode {
        BindingModeId::ActionMenu.into()
    }

    fn status_fragment(&self) -> StatusFragment {
        StatusFragment { status: Some(StatusContent::Label("TABLE ACTIONS".into())) }
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}
