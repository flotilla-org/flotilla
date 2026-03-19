use std::any::Any;

use flotilla_core::data::GroupEntry;
use ratatui::{layout::Rect, Frame};

use super::{AppAction, InteractiveWidget, Outcome, RenderContext, WidgetContext};
use crate::{
    app::ui_state::UiMode,
    keymap::{Action, ModeId},
    ui,
};

/// Root widget that composes the base layer: tab bar, content area (table +
/// preview), status bar, and event log.
///
/// Sits at `widget_stack[0]` and handles all Normal-mode actions that the
/// previous `WorkItemTable` widget handled. Modal widgets are pushed on top
/// and rendered after BaseView.
///
/// Rendering delegates to `ui::render` which orchestrates layout across the
/// child components (TabBar, StatusBarWidget, EventLogWidget, PreviewPanel).
/// Those children live on `App` for now and are accessed through `RenderContext`.
pub struct BaseView;

impl BaseView {
    pub fn new() -> Self {
        Self
    }

    // ── Action helpers (absorbed from WorkItemTable) ──

    fn select_next(ctx: &mut WidgetContext) -> Outcome {
        let repo_key = &ctx.repo_order[ctx.active_repo];
        let rui = ctx.repo_ui.get_mut(repo_key).expect("active repo must have UI state");
        let indices = &rui.table_view.selectable_indices;
        if indices.is_empty() {
            return Outcome::Consumed;
        }
        let current_si = rui.selected_selectable_idx;
        let next = match current_si {
            Some(si) if si + 1 < indices.len() => si + 1,
            Some(si) => si,
            None => 0,
        };
        let table_idx = rui.table_view.selectable_indices[next];
        rui.selected_selectable_idx = Some(next);
        rui.table_state.select(Some(table_idx));

        Outcome::Consumed
    }

    fn select_prev(ctx: &mut WidgetContext) -> Outcome {
        let repo_key = &ctx.repo_order[ctx.active_repo];
        let rui = ctx.repo_ui.get_mut(repo_key).expect("active repo must have UI state");
        let indices = &rui.table_view.selectable_indices;
        if indices.is_empty() {
            return Outcome::Consumed;
        }
        let current_si = rui.selected_selectable_idx;
        let prev = match current_si {
            Some(si) if si > 0 => si - 1,
            Some(si) => si,
            None => 0,
        };
        let table_idx = rui.table_view.selectable_indices[prev];
        rui.selected_selectable_idx = Some(prev);
        rui.table_state.select(Some(table_idx));

        Outcome::Consumed
    }

    fn toggle_multi_select(ctx: &mut WidgetContext) -> Outcome {
        let repo_key = &ctx.repo_order[ctx.active_repo];
        let rui = ctx.repo_ui.get_mut(repo_key).expect("active repo must have UI state");
        if let Some(si) = rui.selected_selectable_idx {
            if let Some(&table_idx) = rui.table_view.selectable_indices.get(si) {
                if let Some(GroupEntry::Item(item)) = rui.table_view.table_entries.get(table_idx) {
                    let identity = item.identity.clone();
                    if !rui.multi_selected.remove(&identity) {
                        rui.multi_selected.insert(identity);
                    }
                }
            }
        }
        Outcome::Consumed
    }

    fn toggle_providers(ctx: &mut WidgetContext) -> Outcome {
        let repo_key = &ctx.repo_order[ctx.active_repo];
        let rui = ctx.repo_ui.get_mut(repo_key).expect("active repo must have UI state");
        rui.show_providers = !rui.show_providers;
        Outcome::Consumed
    }

    fn dismiss(ctx: &mut WidgetContext) -> Outcome {
        // Cancellation takes priority over other dismiss actions while a command is running.
        if let Some(&command_id) = ctx.in_flight.keys().next() {
            ctx.app_actions.push(AppAction::CancelCommand(command_id));
            return Outcome::Consumed;
        }

        let repo_key = &ctx.repo_order[ctx.active_repo];
        let rui = ctx.repo_ui.get_mut(repo_key).expect("active repo must have UI state");

        if rui.active_search_query.is_some() {
            let repo_path = ctx.model.active_repo_root().clone();
            ctx.commands.push(flotilla_protocol::Command {
                host: None,
                context_repo: None,
                action: flotilla_protocol::CommandAction::ClearIssueSearch { repo: flotilla_protocol::RepoSelector::Path(repo_path) },
            });
            rui.active_search_query = None;
        } else if rui.show_providers {
            rui.show_providers = false;
        } else if !rui.multi_selected.is_empty() {
            rui.multi_selected.clear();
        } else {
            ctx.app_actions.push(AppAction::Quit);
        }
        Outcome::Consumed
    }
}

impl Default for BaseView {
    fn default() -> Self {
        Self::new()
    }
}

impl InteractiveWidget for BaseView {
    fn handle_action(&mut self, action: Action, ctx: &mut WidgetContext) -> Outcome {
        // Only handle table actions when in Normal mode. Config/EventLog mode
        // actions fall through to the legacy dispatch_action path.
        if !matches!(*ctx.mode, UiMode::Normal) {
            return Outcome::Ignored;
        }

        match action {
            Action::SelectNext => Self::select_next(ctx),
            Action::SelectPrev => Self::select_prev(ctx),
            Action::ToggleMultiSelect => Self::toggle_multi_select(ctx),
            Action::ToggleProviders => Self::toggle_providers(ctx),
            Action::Dismiss => Self::dismiss(ctx),
            Action::Quit => {
                ctx.app_actions.push(AppAction::Quit);
                Outcome::Consumed
            }
            Action::Refresh => {
                let repo = ctx.model.active_repo_root().clone();
                ctx.commands.push(flotilla_protocol::Command {
                    host: None,
                    context_repo: None,
                    action: flotilla_protocol::CommandAction::Refresh { repo: Some(flotilla_protocol::RepoSelector::Path(repo)) },
                });
                Outcome::Consumed
            }

            // Open modal widgets -- return Push outcomes
            Action::ToggleHelp => Outcome::Push(Box::new(super::help::HelpWidget::new())),

            Action::OpenBranchInput => {
                Outcome::Push(Box::new(super::branch_input::BranchInputWidget::new(crate::app::BranchInputKind::Manual)))
            }

            Action::OpenIssueSearch => {
                *ctx.mode = UiMode::IssueSearch { input: tui_input::Input::default() };
                Outcome::Push(Box::new(super::issue_search::IssueSearchWidget::new()))
            }

            Action::OpenCommandPalette => Outcome::Push(Box::new(super::command_palette::CommandPaletteWidget::new())),

            // App-level toggles
            Action::ToggleDebug => {
                ctx.app_actions.push(AppAction::ToggleDebug);
                Outcome::Consumed
            }
            Action::ToggleStatusBarKeys => {
                ctx.app_actions.push(AppAction::ToggleStatusBarKeys);
                Outcome::Consumed
            }
            Action::CycleHost => {
                ctx.app_actions.push(AppAction::CycleHost);
                Outcome::Consumed
            }
            Action::CycleLayout => {
                ctx.app_actions.push(AppAction::CycleLayout);
                Outcome::Consumed
            }
            Action::CycleTheme => {
                ctx.app_actions.push(AppAction::CycleTheme);
                Outcome::Consumed
            }

            // Actions that need &App context -- fall through to legacy dispatch
            Action::Confirm
            | Action::OpenActionMenu
            | Action::OpenFilePicker
            | Action::Dispatch(_)
            | Action::PrevTab
            | Action::NextTab
            | Action::MoveTabLeft
            | Action::MoveTabRight => Outcome::Ignored,
        }
    }

    fn render(&mut self, frame: &mut Frame, _area: Rect, ctx: &mut RenderContext) {
        ui::render(
            ctx.model,
            ctx.ui,
            ctx.in_flight,
            ctx.theme,
            ctx.keymap,
            frame,
            ctx.active_widget_mode,
            ctx.active_widget_data.clone(),
            ctx.tab_bar,
            ctx.status_bar_widget,
            ctx.event_log_widget,
            ctx.preview_panel,
        );
    }

    fn mode_id(&self) -> ModeId {
        ModeId::Normal
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use flotilla_protocol::WorkItemIdentity;

    use super::*;
    use crate::app::test_support::{issue_table_entries, TestWidgetHarness};

    fn harness_with_items(count: usize) -> TestWidgetHarness {
        let mut harness = TestWidgetHarness::new();
        let repo_key = harness.model.repo_order[0].clone();
        harness.repo_ui.get_mut(&repo_key).expect("repo ui exists").table_view = issue_table_entries(count);
        harness
    }

    fn harness_with_selected_items(count: usize) -> TestWidgetHarness {
        let mut harness = harness_with_items(count);
        if count > 0 {
            let repo_key = harness.model.repo_order[0].clone();
            let rui = harness.repo_ui.get_mut(&repo_key).expect("repo ui exists");
            rui.selected_selectable_idx = Some(0);
            rui.table_state.select(Some(0));
        }
        harness
    }

    // -- mode_id --

    #[test]
    fn mode_id_is_normal() {
        let widget = BaseView::new();
        assert_eq!(widget.mode_id(), ModeId::Normal);
    }

    // -- SelectNext / SelectPrev --

    #[test]
    fn select_next_from_none_selects_first() {
        let mut widget = BaseView::new();
        let mut harness = harness_with_items(5);
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::SelectNext, &mut ctx);
        assert!(matches!(outcome, Outcome::Consumed));

        let repo_key = &harness.model.repo_order[0];
        assert_eq!(harness.repo_ui[repo_key].selected_selectable_idx, Some(0));
    }

    #[test]
    fn select_next_advances() {
        let mut widget = BaseView::new();
        let mut harness = harness_with_selected_items(5);
        let mut ctx = harness.ctx();

        widget.handle_action(Action::SelectNext, &mut ctx);

        let repo_key = &harness.model.repo_order[0];
        assert_eq!(harness.repo_ui[repo_key].selected_selectable_idx, Some(1));
    }

    #[test]
    fn select_next_stays_at_end() {
        let mut widget = BaseView::new();
        let mut harness = harness_with_selected_items(2);

        {
            let mut ctx = harness.ctx();
            widget.handle_action(Action::SelectNext, &mut ctx); // 0 -> 1
        }
        {
            let mut ctx = harness.ctx();
            widget.handle_action(Action::SelectNext, &mut ctx); // 1 -> 1 (stays)
        }

        let repo_key = &harness.model.repo_order[0];
        assert_eq!(harness.repo_ui[repo_key].selected_selectable_idx, Some(1));
    }

    #[test]
    fn select_next_noop_on_empty() {
        let mut widget = BaseView::new();
        let mut harness = harness_with_items(0);
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::SelectNext, &mut ctx);
        assert!(matches!(outcome, Outcome::Consumed));

        let repo_key = &harness.model.repo_order[0];
        assert_eq!(harness.repo_ui[repo_key].selected_selectable_idx, None);
    }

    #[test]
    fn select_prev_from_none_selects_first() {
        let mut widget = BaseView::new();
        let mut harness = harness_with_items(5);
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::SelectPrev, &mut ctx);
        assert!(matches!(outcome, Outcome::Consumed));

        let repo_key = &harness.model.repo_order[0];
        assert_eq!(harness.repo_ui[repo_key].selected_selectable_idx, Some(0));
    }

    #[test]
    fn select_prev_decrements() {
        let mut widget = BaseView::new();
        let mut harness = harness_with_selected_items(5);

        {
            let mut ctx = harness.ctx();
            widget.handle_action(Action::SelectNext, &mut ctx); // 0 -> 1
        }
        {
            let mut ctx = harness.ctx();
            widget.handle_action(Action::SelectNext, &mut ctx); // 1 -> 2
        }
        {
            let mut ctx = harness.ctx();
            widget.handle_action(Action::SelectPrev, &mut ctx); // 2 -> 1
        }

        let repo_key = &harness.model.repo_order[0];
        assert_eq!(harness.repo_ui[repo_key].selected_selectable_idx, Some(1));
    }

    #[test]
    fn select_prev_stays_at_zero() {
        let mut widget = BaseView::new();
        let mut harness = harness_with_selected_items(5);
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::SelectPrev, &mut ctx);
        assert!(matches!(outcome, Outcome::Consumed));

        let repo_key = &harness.model.repo_order[0];
        assert_eq!(harness.repo_ui[repo_key].selected_selectable_idx, Some(0));
    }

    // -- ToggleMultiSelect --

    #[test]
    fn toggle_multi_select_adds() {
        let mut widget = BaseView::new();
        let mut harness = harness_with_selected_items(3);
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::ToggleMultiSelect, &mut ctx);
        assert!(matches!(outcome, Outcome::Consumed));

        let repo_key = &harness.model.repo_order[0];
        assert!(harness.repo_ui[repo_key].multi_selected.contains(&WorkItemIdentity::Issue("0".into())));
    }

    #[test]
    fn toggle_multi_select_removes() {
        let mut widget = BaseView::new();
        let mut harness = harness_with_selected_items(3);

        {
            let mut ctx = harness.ctx();
            widget.handle_action(Action::ToggleMultiSelect, &mut ctx); // add
        }
        {
            let mut ctx = harness.ctx();
            widget.handle_action(Action::ToggleMultiSelect, &mut ctx); // remove
        }

        let repo_key = &harness.model.repo_order[0];
        assert!(harness.repo_ui[repo_key].multi_selected.is_empty());
    }

    #[test]
    fn toggle_multi_select_noop_when_no_selection() {
        let mut widget = BaseView::new();
        let mut harness = harness_with_items(3);
        let mut ctx = harness.ctx();

        widget.handle_action(Action::ToggleMultiSelect, &mut ctx);

        let repo_key = &harness.model.repo_order[0];
        assert!(harness.repo_ui[repo_key].multi_selected.is_empty());
    }

    // -- ToggleProviders --

    #[test]
    fn toggle_providers_toggles() {
        let mut widget = BaseView::new();
        let mut harness = harness_with_items(1);
        let repo_key = harness.model.repo_order[0].clone();

        {
            let mut ctx = harness.ctx();
            widget.handle_action(Action::ToggleProviders, &mut ctx);
        }

        assert!(harness.repo_ui[&repo_key].show_providers);

        {
            let mut ctx = harness.ctx();
            widget.handle_action(Action::ToggleProviders, &mut ctx);
        }

        assert!(!harness.repo_ui[&repo_key].show_providers);
    }

    // -- Dismiss cascade --

    #[test]
    fn dismiss_cancels_in_flight_first() {
        let mut widget = BaseView::new();
        let mut harness = harness_with_items(1);
        harness.in_flight.insert(42, crate::app::InFlightCommand {
            repo_identity: harness.model.repo_order[0].clone(),
            repo: std::path::PathBuf::from("/tmp/test-repo"),
            description: "test".into(),
        });
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::Dismiss, &mut ctx);
        assert!(matches!(outcome, Outcome::Consumed));
        assert!(ctx.app_actions.iter().any(|a| matches!(a, AppAction::CancelCommand(42))));
        assert!(!ctx.app_actions.iter().any(|a| matches!(a, AppAction::Quit)));
    }

    #[test]
    fn dismiss_clears_search_second() {
        let mut widget = BaseView::new();
        let mut harness = harness_with_items(1);
        let repo_key = harness.model.repo_order[0].clone();
        harness.repo_ui.get_mut(&repo_key).expect("repo ui").active_search_query = Some("test".into());
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::Dismiss, &mut ctx);
        assert!(matches!(outcome, Outcome::Consumed));
        assert!(!ctx.app_actions.iter().any(|a| matches!(a, AppAction::Quit)));

        assert!(harness.repo_ui[&repo_key].active_search_query.is_none());
        let (cmd, _) = harness.commands.take_next().expect("expected ClearIssueSearch command");
        assert!(matches!(cmd.action, flotilla_protocol::CommandAction::ClearIssueSearch { .. }));
    }

    #[test]
    fn dismiss_clears_providers_third() {
        let mut widget = BaseView::new();
        let mut harness = harness_with_items(1);
        let repo_key = harness.model.repo_order[0].clone();
        harness.repo_ui.get_mut(&repo_key).expect("repo ui").show_providers = true;
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::Dismiss, &mut ctx);
        assert!(matches!(outcome, Outcome::Consumed));
        assert!(!ctx.app_actions.iter().any(|a| matches!(a, AppAction::Quit)));

        assert!(!harness.repo_ui[&repo_key].show_providers);
    }

    #[test]
    fn dismiss_clears_multi_select_fourth() {
        let mut widget = BaseView::new();
        let mut harness = harness_with_selected_items(3);
        let repo_key = harness.model.repo_order[0].clone();
        harness.repo_ui.get_mut(&repo_key).expect("repo ui").multi_selected.insert(WorkItemIdentity::Issue("0".into()));

        let mut ctx = harness.ctx();
        let outcome = widget.handle_action(Action::Dismiss, &mut ctx);
        assert!(matches!(outcome, Outcome::Consumed));
        assert!(!ctx.app_actions.iter().any(|a| matches!(a, AppAction::Quit)));

        assert!(harness.repo_ui[&repo_key].multi_selected.is_empty());
    }

    #[test]
    fn dismiss_quits_when_nothing_to_clear() {
        let mut widget = BaseView::new();
        let mut harness = harness_with_items(1);
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::Dismiss, &mut ctx);
        assert!(matches!(outcome, Outcome::Consumed));
        assert!(ctx.app_actions.iter().any(|a| matches!(a, AppAction::Quit)));
    }

    // -- Quit --

    #[test]
    fn quit_pushes_app_action() {
        let mut widget = BaseView::new();
        let mut harness = harness_with_items(1);
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::Quit, &mut ctx);
        assert!(matches!(outcome, Outcome::Consumed));
        assert!(ctx.app_actions.iter().any(|a| matches!(a, AppAction::Quit)));
    }

    // -- Push modal widgets --

    #[test]
    fn toggle_help_pushes_help_widget() {
        let mut widget = BaseView::new();
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::ToggleHelp, &mut ctx);
        assert!(matches!(outcome, Outcome::Push(_)));
    }

    #[test]
    fn open_branch_input_pushes_widget() {
        let mut widget = BaseView::new();
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::OpenBranchInput, &mut ctx);
        assert!(matches!(outcome, Outcome::Push(_)));
    }

    #[test]
    fn open_issue_search_pushes_widget_and_sets_mode() {
        let mut widget = BaseView::new();
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::OpenIssueSearch, &mut ctx);
        assert!(matches!(outcome, Outcome::Push(_)));
        assert!(matches!(harness.mode, UiMode::IssueSearch { .. }));
    }

    #[test]
    fn open_command_palette_pushes_widget() {
        let mut widget = BaseView::new();
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::OpenCommandPalette, &mut ctx);
        assert!(matches!(outcome, Outcome::Push(_)));
    }

    // -- Ignored actions --

    #[test]
    fn confirm_returns_ignored() {
        let mut widget = BaseView::new();
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::Confirm, &mut ctx);
        assert!(matches!(outcome, Outcome::Ignored));
    }

    #[test]
    fn open_action_menu_returns_ignored() {
        let mut widget = BaseView::new();
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::OpenActionMenu, &mut ctx);
        assert!(matches!(outcome, Outcome::Ignored));
    }

    #[test]
    fn tab_navigation_returns_ignored() {
        let mut widget = BaseView::new();
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        assert!(matches!(widget.handle_action(Action::PrevTab, &mut ctx), Outcome::Ignored));
        assert!(matches!(widget.handle_action(Action::NextTab, &mut ctx), Outcome::Ignored));
    }

    #[test]
    fn cycle_theme_pushes_app_action() {
        let mut widget = BaseView::new();
        let mut harness = TestWidgetHarness::new();
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::CycleTheme, &mut ctx);
        assert!(matches!(outcome, Outcome::Consumed));
        assert!(ctx.app_actions.iter().any(|a| matches!(a, AppAction::CycleTheme)));
    }

    // -- Non-Normal mode returns Ignored --

    #[test]
    fn non_normal_mode_returns_ignored() {
        let mut widget = BaseView::new();
        let mut harness = TestWidgetHarness::new();
        harness.mode = UiMode::Config;
        let mut ctx = harness.ctx();

        let outcome = widget.handle_action(Action::SelectNext, &mut ctx);
        assert!(matches!(outcome, Outcome::Ignored));
    }
}
