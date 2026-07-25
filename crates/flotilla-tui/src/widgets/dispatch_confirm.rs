use std::any::Any;

use crossterm::event::KeyEvent;
use ratatui::{
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{Block, Clear, Paragraph, Wrap},
    Frame,
};
use tui_input::{backend::crossterm::EventHandler as InputEventHandler, Input};

use super::{AppAction, InteractiveWidget, Outcome, RenderContext, WidgetContext};
use crate::{
    binding_table::{BindingModeId, KeyBindingMode, StatusContent, StatusFragment},
    keymap::Action,
    table_view::{TableIntent, TableIssueStart},
    ui_helpers,
};

/// The single confirmation barrier for every issue-table convoy dispatch.
pub struct DispatchConfirmWidget {
    intent: TableIntent,
    instruction: Input,
}

impl DispatchConfirmWidget {
    pub fn new(intent: TableIntent) -> Self {
        assert!(
            matches!(intent, TableIntent::StartConvoy { .. } | TableIntent::StartConvoys { .. } | TableIntent::StartBatchConvoy { .. }),
            "dispatch confirmation requires a convoy-start table intent"
        );
        Self { intent, instruction: Input::default() }
    }

    fn dispatch_details(&self) -> (&str, &str, &[TableIssueStart], &str) {
        match &self.intent {
            TableIntent::StartConvoy { namespace, project, issue } => (namespace, project, std::slice::from_ref(issue), "One convoy"),
            TableIntent::StartConvoys { namespace, project, issues } => (namespace, project, issues, "One convoy per issue"),
            TableIntent::StartBatchConvoy { namespace, project, issues } => (namespace, project, issues, "One convoy for all issues"),
            _ => unreachable!("constructor validates the table intent"),
        }
    }

    pub fn instruction(&self) -> &str {
        self.instruction.value()
    }
}

impl InteractiveWidget for DispatchConfirmWidget {
    fn handle_action(&mut self, action: Action, ctx: &mut WidgetContext) -> Outcome {
        match action {
            Action::Confirm => {
                let instruction = self.instruction.value().trim();
                ctx.app_actions.push(AppAction::ConfirmConvoyDispatch {
                    intent: self.intent.clone(),
                    instruction: (!instruction.is_empty()).then(|| instruction.to_string()),
                });
                Outcome::Finished
            }
            Action::Dismiss => Outcome::Finished,
            _ => Outcome::Ignored,
        }
    }

    fn handle_raw_key(&mut self, key: KeyEvent, _ctx: &mut WidgetContext) -> Outcome {
        self.instruction.handle_event(&crossterm::event::Event::Key(key));
        Outcome::Consumed
    }

    fn render(&mut self, frame: &mut Frame, area: Rect, ctx: &mut RenderContext) {
        let (namespace, project, issues, dispatch_shape) = self.dispatch_details();
        let popup = ui_helpers::popup_area(area, 72, 58);
        frame.render_widget(Clear, popup);

        let theme = ctx.theme;
        let mut lines = vec![
            Line::from(vec![Span::raw("Target project: "), Span::styled(format!("{namespace}/{project}"), Style::default().bold())]),
            Line::from(vec![
                Span::raw("Placement:      "),
                Span::styled("Automatic (resolved on dispatch)", Style::default().fg(theme.muted)),
            ]),
            Line::from(vec![Span::raw("Workflow:       "), Span::styled("Project default", Style::default().fg(theme.muted))]),
            Line::from(vec![Span::raw("Dispatch:       "), Span::raw(dispatch_shape)]),
            Line::from(""),
        ];

        for issue in issues.iter().take(4) {
            lines.push(Line::from(vec![
                Span::styled(format!("#{} ", issue.issue.id), Style::default().bold()),
                Span::raw(issue.title.clone()),
            ]));
        }
        if issues.len() > 4 {
            lines.push(Line::from(format!("… and {} more", issues.len() - 4)));
        }
        if issues.iter().any(|issue| !issue.ready) {
            lines.push(Line::from(""));
            let warning = if issues.len() == 1 { "⚠ Issue is not marked ready" } else { "⚠ One or more issues are not marked ready" };
            lines.push(Line::from(Span::styled(warning, Style::default().fg(theme.status_error).bold())));
        }

        lines.extend([
            Line::from(""),
            Line::from("Optional guidance (appended to the crew brief):"),
            Line::from(vec![
                Span::styled("> ", Style::default().fg(theme.input_text)),
                Span::raw(self.instruction.value().to_string()),
                Span::styled("▏", Style::default().fg(theme.input_text)),
            ]),
            Line::from(""),
            Line::from(Span::styled("Enter: dispatch    Esc: cancel", Style::default().fg(theme.muted))),
        ]);

        let paragraph = Paragraph::new(lines)
            .block(Block::bordered().style(theme.block_style()).title(" Confirm convoy dispatch "))
            .wrap(Wrap { trim: false });
        frame.render_widget(paragraph, popup);
    }

    fn binding_mode(&self) -> KeyBindingMode {
        BindingModeId::DispatchConfirm.into()
    }

    fn captures_raw_keys(&self) -> bool {
        true
    }

    fn status_fragment(&self) -> StatusFragment {
        StatusFragment { status: Some(StatusContent::Label("CONFIRM DISPATCH".into())) }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}
