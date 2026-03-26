use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use flotilla_protocol::{Command, CommandAction, WorkItem};
use ratatui::{
    layout::Rect,
    style::Style,
    widgets::{Block, Clear, List, ListItem, ListState},
    Frame,
};

use super::{
    branch_input::BranchInputWidget, close_confirm::CloseConfirmWidget, delete_confirm::DeleteConfirmWidget, InteractiveWidget, Outcome,
    RenderContext, WidgetContext,
};
use crate::{
    app::{
        intent::Intent,
        ui_state::{BranchInputKind, PendingActionContext},
    },
    binding_table::{BindingModeId, KeyBindingMode, StatusContent, StatusFragment},
    keymap::Action,
    ui_helpers,
};

/// A pre-resolved menu entry: the intent and the concrete command it resolved to.
pub struct MenuEntry {
    pub intent: Intent,
    pub command: Command,
}

pub struct ActionMenuWidget {
    entries: Vec<MenuEntry>,
    index: usize,
    item: WorkItem,
    menu_area: Rect,
}

impl ActionMenuWidget {
    pub fn new(entries: Vec<MenuEntry>, item: WorkItem) -> Self {
        Self { entries, index: 0, item, menu_area: Rect::default() }
    }

    fn confirm(&self, ctx: &mut WidgetContext) -> Outcome {
        let Some(entry) = self.entries.get(self.index) else {
            return Outcome::Finished;
        };

        let repo_identity = ctx.model.active_repo_identity().clone();
        let labels = ctx.model.active_labels();

        match entry.intent {
            Intent::RemoveCheckout => {
                let remote_host = match ctx.model.my_host() {
                    Some(my_host) if self.item.host != *my_host => Some(self.item.host.clone()),
                    _ => None,
                };
                let checkout_path = self.item.checkout_key().map(|hp| hp.path.clone());
                let widget = DeleteConfirmWidget::new(self.item.identity.clone(), remote_host, checkout_path);
                let pending_ctx =
                    PendingActionContext { identity: self.item.identity.clone(), description: entry.intent.label(labels), repo_identity };
                ctx.commands.push_with_context(entry.command.clone(), Some(pending_ctx));
                return Outcome::Swap(Box::new(widget));
            }
            Intent::GenerateBranchName => {
                let pending_ctx =
                    PendingActionContext { identity: self.item.identity.clone(), description: entry.intent.label(labels), repo_identity };
                ctx.commands.push_with_context(entry.command.clone(), Some(pending_ctx));
                let widget = BranchInputWidget::new(BranchInputKind::Generating);
                return Outcome::Swap(Box::new(widget));
            }
            Intent::CloseChangeRequest => {
                let id = match &entry.command {
                    Command { action: CommandAction::CloseChangeRequest { id }, .. } => id.clone(),
                    _ => return Outcome::Finished,
                };
                let widget = CloseConfirmWidget::new(id, self.item.description.clone(), self.item.identity.clone(), entry.command.clone());
                // CloseConfirm defers the command push to the confirm dialog itself.
                return Outcome::Swap(Box::new(widget));
            }
            _ => {
                let pending_ctx =
                    PendingActionContext { identity: self.item.identity.clone(), description: entry.intent.label(labels), repo_identity };
                ctx.commands.push_with_context(entry.command.clone(), Some(pending_ctx));
            }
        }

        Outcome::Finished
    }
}

impl InteractiveWidget for ActionMenuWidget {
    fn handle_action(&mut self, action: Action, ctx: &mut WidgetContext) -> Outcome {
        match action {
            Action::SelectNext => {
                if self.index < self.entries.len().saturating_sub(1) {
                    self.index += 1;
                }
                Outcome::Consumed
            }
            Action::SelectPrev => {
                self.index = self.index.saturating_sub(1);
                Outcome::Consumed
            }
            Action::Dismiss => Outcome::Finished,
            Action::Confirm => self.confirm(ctx),
            _ => Outcome::Ignored,
        }
    }

    fn handle_mouse(&mut self, mouse: MouseEvent, ctx: &mut WidgetContext) -> Outcome {
        if mouse.kind != MouseEventKind::Down(MouseButton::Left) {
            return Outcome::Ignored;
        }

        let x = mouse.column;
        let y = mouse.row;
        let a = self.menu_area;

        // Click outside dismisses
        if x < a.x || x >= a.x + a.width || y < a.y || y >= a.y + a.height {
            return Outcome::Finished;
        }

        let row = (y - a.y) as usize;
        // Row 0 is the border
        if row < 1 {
            return Outcome::Consumed;
        }
        let item_idx = row - 1;
        if item_idx < self.entries.len() {
            self.index = item_idx;
            return self.confirm(ctx);
        }

        Outcome::Consumed
    }

    fn render(&mut self, frame: &mut Frame, area: Rect, ctx: &mut RenderContext) {
        let popup = ui_helpers::popup_area(area, 40, 40);
        self.menu_area = popup;
        frame.render_widget(Clear, popup);

        let labels = ctx.model.active_labels();
        let list_items: Vec<ListItem> = self
            .entries
            .iter()
            .enumerate()
            .map(|(i, entry)| ListItem::new(format!(" {}: {}", i + 1, entry.intent.label(labels))))
            .collect();

        let list = List::new(list_items)
            .block(Block::bordered().style(ctx.theme.block_style()).title(" Actions "))
            .highlight_style(Style::default().bg(ctx.theme.action_highlight).bold())
            .highlight_symbol("\u{25b8} ");

        let mut state = ListState::default();
        state.select(Some(self.index));
        frame.render_stateful_widget(list, popup, &mut state);
    }

    fn binding_mode(&self) -> KeyBindingMode {
        BindingModeId::ActionMenu.into()
    }

    fn status_fragment(&self) -> StatusFragment {
        StatusFragment { status: Some(StatusContent::Label("ACTIONS".into())) }
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use flotilla_protocol::{CommandAction, HostName};

    use super::*;
    use crate::app::test_support::{checkout_item, pr_item, TestWidgetHarness};

    fn make_checkout_entry() -> (WorkItem, Vec<MenuEntry>) {
        let item = checkout_item("feat/a", "/tmp/a", false);
        let command = Command {
            host: None,
            environment: None,
            context_repo: None,
            action: CommandAction::FetchCheckoutStatus {
                branch: "feat/a".into(),
                checkout_path: Some("/tmp/a".into()),
                change_request_id: None,
            },
        };
        let entries = vec![MenuEntry { intent: Intent::RemoveCheckout, command }];
        (item, entries)
    }

    fn make_simple_entry(intent: Intent) -> (WorkItem, Vec<MenuEntry>) {
        let item = checkout_item("feat/b", "/tmp/b", false);
        let command = Command {
            host: None,
            environment: None,
            context_repo: None,
            action: CommandAction::CreateWorkspaceForCheckout { checkout_path: "/tmp/b".into(), label: "feat/b".into() },
        };
        let entries = vec![MenuEntry { intent, command }];
        (item, entries)
    }

    #[test]
    fn mode_id_is_action_menu() {
        let (item, entries) = make_simple_entry(Intent::CreateWorkspace);
        let widget = ActionMenuWidget::new(entries, item);
        assert_eq!(widget.binding_mode(), KeyBindingMode::from(BindingModeId::ActionMenu));
    }

    #[test]
    fn select_next_increments_index() {
        let item = checkout_item("feat/a", "/tmp/a", false);
        let entries = vec![
            MenuEntry {
                intent: Intent::CreateWorkspace,
                command: Command {
                    host: None,
                    environment: None,
                    context_repo: None,
                    action: CommandAction::CreateWorkspaceForCheckout { checkout_path: "/tmp/a".into(), label: "feat/a".into() },
                },
            },
            MenuEntry {
                intent: Intent::RemoveCheckout,
                command: Command {
                    host: None,
                    environment: None,
                    context_repo: None,
                    action: CommandAction::FetchCheckoutStatus {
                        branch: "feat/a".into(),
                        checkout_path: Some("/tmp/a".into()),
                        change_request_id: None,
                    },
                },
            },
        ];
        let mut widget = ActionMenuWidget::new(entries, item);
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::SelectNext, &mut ctx);
        assert!(matches!(outcome, Outcome::Consumed));
        assert_eq!(widget.index, 1);
    }

    #[test]
    fn select_next_clamps_at_end() {
        let item = checkout_item("feat/a", "/tmp/a", false);
        let entries = vec![MenuEntry {
            intent: Intent::CreateWorkspace,
            command: Command {
                host: None,
                environment: None,
                context_repo: None,
                action: CommandAction::CreateWorkspaceForCheckout { checkout_path: "/tmp/a".into(), label: "feat/a".into() },
            },
        }];
        let mut widget = ActionMenuWidget::new(entries, item);
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::SelectNext, &mut ctx);
        assert!(matches!(outcome, Outcome::Consumed));
        assert_eq!(widget.index, 0);
    }

    #[test]
    fn select_prev_decrements_index() {
        let item = checkout_item("feat/a", "/tmp/a", false);
        let entries = vec![
            MenuEntry {
                intent: Intent::CreateWorkspace,
                command: Command {
                    host: None,
                    environment: None,
                    context_repo: None,
                    action: CommandAction::CreateWorkspaceForCheckout { checkout_path: "/tmp/a".into(), label: "feat/a".into() },
                },
            },
            MenuEntry {
                intent: Intent::RemoveCheckout,
                command: Command {
                    host: None,
                    environment: None,
                    context_repo: None,
                    action: CommandAction::FetchCheckoutStatus {
                        branch: "feat/a".into(),
                        checkout_path: Some("/tmp/a".into()),
                        change_request_id: None,
                    },
                },
            },
        ];
        let mut widget = ActionMenuWidget::new(entries, item);
        widget.index = 1;
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::SelectPrev, &mut ctx);
        assert!(matches!(outcome, Outcome::Consumed));
        assert_eq!(widget.index, 0);
    }

    #[test]
    fn select_prev_stays_at_zero() {
        let (item, entries) = make_simple_entry(Intent::CreateWorkspace);
        let mut widget = ActionMenuWidget::new(entries, item);
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::SelectPrev, &mut ctx);
        assert!(matches!(outcome, Outcome::Consumed));
        assert_eq!(widget.index, 0);
    }

    #[test]
    fn dismiss_returns_finished() {
        let (item, entries) = make_simple_entry(Intent::CreateWorkspace);
        let mut widget = ActionMenuWidget::new(entries, item);
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::Dismiss, &mut ctx);
        assert!(matches!(outcome, Outcome::Finished));
    }

    #[test]
    fn confirm_simple_action_pushes_command_and_finishes() {
        let (item, entries) = make_simple_entry(Intent::CreateWorkspace);
        let mut widget = ActionMenuWidget::new(entries, item);
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::Confirm, &mut ctx);
        assert!(matches!(outcome, Outcome::Finished));

        let (cmd, pending) = harness.commands.take_next().expect("expected command");
        assert!(matches!(cmd.action, CommandAction::CreateWorkspaceForCheckout { .. }));
        assert!(pending.is_some());
    }

    #[test]
    fn confirm_remove_checkout_swaps_to_delete_confirm_widget() {
        let (item, entries) = make_checkout_entry();
        let mut widget = ActionMenuWidget::new(entries, item);
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::Confirm, &mut ctx);
        assert!(matches!(outcome, Outcome::Swap(_)));

        let (cmd, _) = harness.commands.take_next().expect("expected command");
        assert!(matches!(cmd.action, CommandAction::FetchCheckoutStatus { .. }));
    }

    #[test]
    fn confirm_close_change_request_swaps_to_close_confirm_widget() {
        let item = pr_item("42");
        let command =
            Command { host: None, environment: None, context_repo: None, action: CommandAction::CloseChangeRequest { id: "42".into() } };
        let entries = vec![MenuEntry { intent: Intent::CloseChangeRequest, command }];
        let mut widget = ActionMenuWidget::new(entries, item);
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::Confirm, &mut ctx);
        assert!(matches!(outcome, Outcome::Swap(_)));
        // CloseConfirm defers the command — no immediate push
        assert!(harness.commands.take_next().is_none());
    }

    #[test]
    fn confirm_generate_branch_name_sets_branch_input_mode() {
        let mut item = checkout_item("feat/c", "/tmp/c", false);
        item.issue_keys = vec!["123".into()];
        let command = Command {
            host: None,
            environment: None,
            context_repo: None,
            action: CommandAction::GenerateBranchName { issue_keys: vec!["123".into()] },
        };
        let entries = vec![MenuEntry { intent: Intent::GenerateBranchName, command }];
        let mut widget = ActionMenuWidget::new(entries, item);
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::Confirm, &mut ctx);
        assert!(matches!(outcome, Outcome::Swap(_)), "expected Swap(BranchInputWidget)");

        let (cmd, _) = harness.commands.take_next().expect("expected command");
        assert!(matches!(cmd.action, CommandAction::GenerateBranchName { .. }));
    }

    #[test]
    fn unhandled_action_returns_ignored() {
        let (item, entries) = make_simple_entry(Intent::CreateWorkspace);
        let mut widget = ActionMenuWidget::new(entries, item);
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::Quit, &mut ctx);
        assert!(matches!(outcome, Outcome::Ignored));
    }

    #[test]
    fn mouse_click_outside_dismisses() {
        let (item, entries) = make_simple_entry(Intent::CreateWorkspace);
        let mut widget = ActionMenuWidget::new(entries, item);
        widget.menu_area = Rect::new(10, 10, 20, 5);
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let mouse = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row: 5,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        let outcome = widget.handle_mouse(mouse, &mut ctx);
        assert!(matches!(outcome, Outcome::Finished));
    }

    #[test]
    fn mouse_click_on_item_confirms() {
        let item = checkout_item("feat/a", "/tmp/a", false);
        let entries = vec![
            MenuEntry {
                intent: Intent::CreateWorkspace,
                command: Command {
                    host: None,
                    environment: None,
                    context_repo: None,
                    action: CommandAction::CreateWorkspaceForCheckout { checkout_path: "/tmp/a".into(), label: "feat/a".into() },
                },
            },
            MenuEntry {
                intent: Intent::RemoveCheckout,
                command: Command {
                    host: None,
                    environment: None,
                    context_repo: None,
                    action: CommandAction::FetchCheckoutStatus {
                        branch: "feat/a".into(),
                        checkout_path: Some("/tmp/a".into()),
                        change_request_id: None,
                    },
                },
            },
        ];
        let mut widget = ActionMenuWidget::new(entries, item);
        widget.menu_area = Rect::new(10, 10, 20, 5);
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        // Click on second item (row 2 = border row 0 + item row 1 + item row 2)
        let mouse = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 15,
            row: 12, // y=10 is border, y=11 is first item, y=12 is second item
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        let outcome = widget.handle_mouse(mouse, &mut ctx);
        assert!(matches!(outcome, Outcome::Swap(_)));
        assert_eq!(widget.index, 1);
        // Second entry is RemoveCheckout which swaps to DeleteConfirmWidget
    }

    #[test]
    fn mouse_non_left_click_ignored() {
        let (item, entries) = make_simple_entry(Intent::CreateWorkspace);
        let mut widget = ActionMenuWidget::new(entries, item);
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let mouse = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Right),
            column: 15,
            row: 12,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        let outcome = widget.handle_mouse(mouse, &mut ctx);
        assert!(matches!(outcome, Outcome::Ignored));
    }

    #[test]
    fn remove_checkout_remote_item_sets_remote_host() {
        let mut item = checkout_item("feat/r", "/tmp/r", false);
        let remote = HostName::new("remote-host");
        item.host = remote.clone();

        let command = Command {
            host: Some(remote.clone()),
            environment: None,
            context_repo: None,
            action: CommandAction::FetchCheckoutStatus {
                branch: "feat/r".into(),
                checkout_path: Some("/tmp/r".into()),
                change_request_id: None,
            },
        };
        let entries = vec![MenuEntry { intent: Intent::RemoveCheckout, command }];
        let mut widget = ActionMenuWidget::new(entries, item);
        let mut harness = TestWidgetHarness::new();

        // Set up the local host so the widget can detect remote items
        let local_host = HostName::new("local-host");
        harness.model.hosts.insert(local_host.clone(), crate::app::TuiHostState {
            host_name: local_host,
            is_local: true,
            status: crate::app::PeerStatus::Connected,
            summary: flotilla_protocol::HostSummary {
                host_name: HostName::new("local-host"),
                system: flotilla_protocol::SystemInfo::default(),
                inventory: flotilla_protocol::ToolInventory::default(),
                providers: vec![],
                environments: vec![],
            },
        });

        let mut ctx = harness.ctx();
        let outcome = widget.handle_action(Action::Confirm, &mut ctx);
        match outcome {
            Outcome::Swap(mut swapped) => {
                let dcw = swapped.as_any_mut().downcast_ref::<super::DeleteConfirmWidget>().expect("expected DeleteConfirmWidget");
                assert_eq!(dcw.remote_host.as_ref(), Some(&HostName::new("remote-host")));
            }
            other => panic!("expected Swap, got {:?}", std::mem::discriminant(&other)),
        }
    }
}
