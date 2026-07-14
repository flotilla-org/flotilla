use ratatui::{
    layout::Rect,
    text::Line,
    widgets::{Block, Clear, Paragraph, Wrap},
    Frame,
};

use super::{InteractiveWidget, Outcome, RenderContext, WidgetContext};
use crate::{
    binding_table::{BindingModeId, KeyBindingMode, StatusContent, StatusFragment},
    keymap::Action,
    table_view::DetailField,
    ui_helpers,
};

pub struct DescribeWidget {
    title: String,
    fields: Vec<DetailField>,
    scroll: u16,
}

impl DescribeWidget {
    pub fn new(title: String, fields: Vec<DetailField>) -> Self {
        Self { title, fields, scroll: 0 }
    }
}

impl InteractiveWidget for DescribeWidget {
    fn handle_action(&mut self, action: Action, _ctx: &mut WidgetContext) -> Outcome {
        match action {
            Action::SelectNext => {
                self.scroll = self.scroll.saturating_add(1);
                Outcome::Consumed
            }
            Action::SelectPrev => {
                self.scroll = self.scroll.saturating_sub(1);
                Outcome::Consumed
            }
            Action::Dismiss | Action::Describe => Outcome::Finished,
            _ => Outcome::Ignored,
        }
    }

    fn render(&mut self, frame: &mut Frame, area: Rect, ctx: &mut RenderContext) {
        let popup = ui_helpers::popup_area(area, 70, 75);
        frame.render_widget(Clear, popup);
        let lines = self.fields.iter().map(|field| Line::raw(format!("{:<14} {}", field.label, field.value))).collect::<Vec<_>>();
        frame.render_widget(
            Paragraph::new(lines)
                .block(Block::bordered().style(ctx.theme.block_style()).title(format!(" Describe · {} ", self.title)))
                .scroll((self.scroll, 0))
                .wrap(Wrap { trim: false }),
            popup,
        );
    }

    fn binding_mode(&self) -> KeyBindingMode {
        BindingModeId::Help.into()
    }

    fn status_fragment(&self) -> StatusFragment {
        StatusFragment { status: Some(StatusContent::Label("DESCRIBE".into())) }
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}
