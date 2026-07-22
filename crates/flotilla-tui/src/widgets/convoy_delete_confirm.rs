use flotilla_protocol::{Command, CommandAction};
use ratatui::{
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{Block, Clear, Paragraph, Wrap},
    Frame,
};

use super::{InteractiveWidget, Outcome, RenderContext, WidgetContext};
use crate::{
    binding_table::{BindingModeId, KeyBindingMode, StatusContent, StatusFragment},
    keymap::Action,
    ui_helpers,
};

pub struct ConvoyDeleteConfirmWidget {
    command: Command,
}

impl ConvoyDeleteConfirmWidget {
    pub fn new(command: Command) -> Self {
        assert!(
            matches!(&command.action, CommandAction::ConvoyDelete { namespace: Some(_), .. }),
            "convoy delete confirmation requires an explicitly namespaced convoy delete command"
        );
        Self { command }
    }

    fn target(&self) -> (&str, &str) {
        match &self.command.action {
            CommandAction::ConvoyDelete { namespace: Some(namespace), name, .. } => (namespace, name),
            _ => unreachable!("constructor validates the command action"),
        }
    }
}

impl InteractiveWidget for ConvoyDeleteConfirmWidget {
    fn handle_action(&mut self, action: Action, ctx: &mut WidgetContext) -> Outcome {
        match action {
            Action::Confirm => {
                ctx.commands.push(self.command.clone());
                Outcome::Finished
            }
            Action::Dismiss => Outcome::Finished,
            _ => Outcome::Ignored,
        }
    }

    fn render(&mut self, frame: &mut Frame, area: Rect, ctx: &mut RenderContext) {
        let (namespace, name) = self.target();
        let popup = ui_helpers::popup_area(area, 64, 34);
        frame.render_widget(Clear, popup);

        let lines = vec![
            Line::from(vec![Span::raw("Convoy: "), Span::styled(format!("{namespace}/{name}"), Style::default().bold())]),
            Line::from(""),
            Line::from("Managed vessels, environments, and terminal sessions will be torn down."),
            Line::from(""),
            Line::from("Teardown gate:"),
            Line::from("  Clean=True"),
            Line::from("  Pushed=True"),
            Line::from("  Landed=True"),
            Line::from(""),
            Line::from("Abandoned convoys bypass the gate. Force skips only this safety check."),
            Line::from(""),
            Line::from(Span::styled("y/Enter: confirm    n/Esc: cancel", Style::default().fg(ctx.theme.muted))),
        ];
        let paragraph = Paragraph::new(lines)
            .block(Block::bordered().style(ctx.theme.block_style()).title(" Delete convoy "))
            .wrap(Wrap { trim: true });
        frame.render_widget(paragraph, popup);
    }

    fn binding_mode(&self) -> KeyBindingMode {
        BindingModeId::DeleteConfirm.into()
    }

    fn status_fragment(&self) -> StatusFragment {
        StatusFragment { status: Some(StatusContent::Label("CONFIRM DELETE".into())) }
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}
