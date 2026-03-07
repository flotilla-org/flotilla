use ratatui::{
    layout::{Constraint, Direction, Flex, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{
        Block, Cell, Clear, HighlightSpacing, List, ListItem, ListState, Paragraph, Row, Table,
        Wrap,
    },
    Frame,
};

use unicode_width::UnicodeWidthStr;

use crate::app::{Intent, ProviderStatus, TabId, TuiModel, UiMode, UiState};
use crate::event_log::{self, LevelExt};
use flotilla_core::data::{GroupEntry, SectionHeader};
use flotilla_protocol::{ChangeRequestStatus, ProviderData, SessionStatus, WorkItem, WorkItemKind};

pub fn render(model: &TuiModel, ui: &mut UiState, frame: &mut Frame) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(frame.area());

    render_tab_bar(model, ui, frame, chunks[0]);
    render_content(model, ui, frame, chunks[1]);
    render_status_bar(model, ui, frame, chunks[2]);
    render_action_menu(model, ui, frame);
    render_input_popup(ui, frame);
    render_delete_confirm(model, ui, frame);
    render_help(model, ui, frame);
    render_file_picker(ui, frame);
}

fn render_tab_bar(model: &TuiModel, ui: &mut UiState, frame: &mut Frame, area: Rect) {
    let flotilla_label = TabId::FLOTILLA_LABEL;
    let flotilla_style = if ui.mode.is_config() {
        Style::default().bold().fg(Color::Black).bg(Color::White)
    } else {
        Style::default().bold().fg(Color::Black).bg(Color::Cyan)
    };
    let mut spans: Vec<Span> = vec![Span::styled(flotilla_label, flotilla_style)];

    ui.layout.tab_areas.clear();
    let flotilla_width = TabId::FLOTILLA_LABEL_WIDTH;
    ui.layout.tab_areas.insert(
        TabId::Flotilla,
        Rect::new(area.x, area.y, flotilla_width, 1),
    );
    let mut x_offset: u16 = flotilla_width;

    for (i, path) in model.repo_order.iter().enumerate() {
        let rm = &model.repos[path];
        let rui = &ui.repo_ui[path];
        let name = TuiModel::repo_name(path);
        let is_active = !ui.mode.is_config() && i == model.active_repo;
        let loading = if rm.loading { " ⟳" } else { "" };
        let changed = if rui.has_unseen_changes { "*" } else { "" };

        let sep = Span::styled(" | ", Style::default().fg(Color::DarkGray));
        spans.push(sep);
        x_offset += 3;

        let label = format!("{name}{changed}{loading}");
        let label_len = label.width() as u16;
        let style = if is_active && ui.drag.active {
            Style::default().bold().fg(Color::Cyan).underlined()
        } else if is_active {
            Style::default().bold().fg(Color::Cyan)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        spans.push(Span::styled(label, style));

        ui.layout.tab_areas.insert(
            TabId::Repo(i),
            Rect::new(area.x + x_offset, area.y, label_len, 1),
        );
        x_offset += label_len;
    }

    // [+] button
    let add_sep = Span::styled(" | ", Style::default().fg(Color::DarkGray));
    spans.push(add_sep);
    x_offset += 3;
    let add_label = Span::styled("[+]", Style::default().fg(Color::Green));
    spans.push(add_label);
    ui.layout
        .tab_areas
        .insert(TabId::Add, Rect::new(area.x + x_offset, area.y, 3, 1));

    let line = Line::from(spans);
    let title = Paragraph::new(line);
    frame.render_widget(title, area);
}

fn active_rui<'a>(model: &TuiModel, ui: &'a UiState) -> &'a crate::app::RepoUiState {
    ui.active_repo_ui(&model.repo_order, model.active_repo)
}

fn selected_work_item<'a>(model: &TuiModel, ui: &'a UiState) -> Option<&'a WorkItem> {
    let rui = active_rui(model, ui);
    let table_idx = rui.table_state.selected()?;
    match rui.table_view.table_entries.get(table_idx)? {
        GroupEntry::Item(item) => Some(item),
        GroupEntry::Header(_) => None,
    }
}

fn render_status_bar(model: &TuiModel, ui: &UiState, frame: &mut Frame, area: Rect) {
    if let Some(err) = &model.status_message {
        let msg = format!(" Error: {}", err);
        let status = Paragraph::new(msg).style(Style::default().fg(Color::Red));
        frame.render_widget(status, area);
        return;
    }

    let rui = active_rui(model, ui);

    let text: String = match &ui.mode {
        UiMode::Config => " j/k:scroll log  [/]:switch tab  ?:help  q:quit".into(),
        UiMode::BranchInput {
            generating: true, ..
        } => " Generating branch name...".into(),
        UiMode::BranchInput {
            generating: false, ..
        } => " type branch name  enter:create  esc:cancel".into(),
        UiMode::ActionMenu { .. } => " j/k:navigate  enter:select  esc:close".into(),
        UiMode::FilePicker { .. } => " j/k:navigate  tab:complete  enter:select  esc:cancel".into(),
        UiMode::DeleteConfirm { .. } => " y/enter:confirm  n/esc:cancel".into(),
        UiMode::Help => " ?:close help  esc:close help".into(),
        UiMode::Normal => {
            if rui.show_providers {
                " c:close providers  [/]:switch tab  ?:help  q:quit".into()
            } else if !rui.multi_selected.is_empty() {
                " enter:create branch  space:toggle  esc:clear  ?:help  q:quit".into()
            } else {
                let mut s = " enter:open".to_string();
                if let Some(item) = selected_work_item(model, ui) {
                    let labels = model.active_labels();
                    for &intent in Intent::all_in_menu_order() {
                        if let Some(hint) = intent.shortcut_hint(labels) {
                            if intent.is_available(item) {
                                s.push_str("  ");
                                s.push_str(&hint);
                            }
                        }
                    }
                }
                s.push_str("  .:menu  n:new  r:refresh  space:select  ?:help  q:quit");
                s
            }
        }
    };

    let status = Paragraph::new(text).style(Style::default().fg(Color::DarkGray));
    frame.render_widget(status, area);
}

fn render_content(model: &TuiModel, ui: &mut UiState, frame: &mut Frame, area: Rect) {
    if ui.mode.is_config() {
        render_config_screen(model, ui, frame, area);
        return;
    }

    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
        .split(area);

    render_unified_table(model, ui, frame, chunks[0]);
    render_preview(model, ui, frame, chunks[1]);
}

fn render_repo_providers(model: &TuiModel, _ui: &UiState, frame: &mut Frame, area: Rect) {
    let path = &model.repo_order[model.active_repo];
    let rm = &model.repos[path];

    let mut lines: Vec<Line> = Vec::new();

    let categories: Vec<(&str, &str)> = vec![
        ("VCS", "vcs"),
        ("Checkout mgr", "checkout_manager"),
        ("Code review", "code_review"),
        ("Issue tracker", "issue_tracker"),
        ("Coding agent", "coding_agent"),
        ("AI utility", "ai_utility"),
        ("Workspace mgr", "workspace_manager"),
    ];

    for (category, key) in categories {
        let provider_name = rm.provider_names.get(key);
        let status = provider_name.and_then(|pname| {
            model
                .provider_statuses
                .get(&(path.clone(), key.to_string(), pname.clone()))
                .copied()
        });

        let value = match (&provider_name, status) {
            (Some(name), Some(ProviderStatus::Ok)) => format!("{} ✓", name),
            (Some(name), Some(ProviderStatus::Error)) => format!("{} ✗", name),
            (Some(name), None) => (*name).clone(),
            (None, _) => "—".to_string(),
        };
        let value_style = match status {
            Some(ProviderStatus::Ok) => Style::default().fg(Color::Green),
            Some(ProviderStatus::Error) => Style::default().fg(Color::Red),
            _ if provider_name.is_some() => Style::default().fg(Color::White),
            _ => Style::default().fg(Color::DarkGray),
        };
        lines.push(Line::from(vec![
            Span::styled(
                format!("  {:<16}", category),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled(value, value_style),
        ]));
    }

    let paragraph = Paragraph::new(lines)
        .block(Block::bordered().title_top(Line::from(" ✕ ").right_aligned()))
        .wrap(Wrap { trim: true });
    frame.render_widget(paragraph, area);
}

fn render_unified_table(model: &TuiModel, ui: &mut UiState, frame: &mut Frame, area: Rect) {
    ui.layout.table_area = area;

    let rui = active_rui(model, ui);
    if rui.show_providers {
        let close_x = area.x + area.width.saturating_sub(5);
        ui.layout
            .tab_areas
            .insert(TabId::Gear, Rect::new(close_x, area.y, 3, 1));
        render_repo_providers(model, ui, frame, area);
        return;
    }

    let gear_x = area.x + area.width.saturating_sub(5);
    ui.layout
        .tab_areas
        .insert(TabId::Gear, Rect::new(gear_x, area.y, 3, 1));

    let labels = model.active_labels();
    let header = Row::new(vec![
        Cell::from(""),
        Cell::from("Description"),
        Cell::from("Branch"),
        Cell::from(labels.checkouts.abbr.as_str()),
        Cell::from("WS"),
        Cell::from(labels.code_review.abbr.as_str()),
        Cell::from(labels.sessions.abbr.as_str()),
        Cell::from("Issues"),
        Cell::from("Git"),
    ])
    .style(Style::default().fg(Color::DarkGray).bold())
    .height(1);

    let widths = [
        Constraint::Length(3),
        Constraint::Min(15),
        Constraint::Length(25),
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Length(10),
        Constraint::Length(4),
        Constraint::Length(10),
        Constraint::Length(5),
    ];

    let inner_width = area.width.saturating_sub(4);
    let col_areas = Layout::horizontal(widths).split(Rect::new(0, 0, inner_width, 1));
    let col_widths: Vec<u16> = col_areas.iter().map(|r| r.width).collect();

    // Build rows from active repo (immutable borrows)
    let rm = model.active();
    let rui = active_rui(model, ui);
    let rows: Vec<Row> = rui
        .table_view
        .table_entries
        .iter()
        .map(|entry| {
            let is_multi_selected = if let GroupEntry::Item(ref item) = entry {
                rui.multi_selected.contains(&item.identity)
            } else {
                false
            };

            match entry {
                GroupEntry::Header(header) => build_header_row(header),
                GroupEntry::Item(item) => {
                    let mut row = build_item_row(item, &rm.providers, &col_widths);
                    if is_multi_selected {
                        row = row.style(Style::default().bg(Color::Indexed(236)));
                    }
                    row
                }
            }
        })
        .collect();

    let table = Table::new(rows, widths)
        .header(header)
        .block(Block::bordered().title_top(Line::from(" ⚙ ").right_aligned()))
        .row_highlight_style(Style::default().bg(Color::DarkGray).bold())
        .highlight_symbol("▸ ")
        .highlight_spacing(HighlightSpacing::Always);

    // Now mutably borrow for stateful render
    let key = &model.repo_order[model.active_repo];
    let rui = ui.repo_ui.get_mut(key).unwrap();
    frame.render_stateful_widget(table, area, &mut rui.table_state);
}

fn build_header_row(header: &SectionHeader) -> Row<'static> {
    let style = Style::default().fg(Color::Yellow).bold();
    Row::new(vec![
        Cell::from(""),
        Cell::from(Span::styled(format!("── {} ──", header), style)),
        Cell::from(""),
        Cell::from(""),
        Cell::from(""),
        Cell::from(""),
        Cell::from(""),
        Cell::from(""),
        Cell::from(""),
    ])
    .height(1)
}

fn build_item_row<'a>(item: &WorkItem, providers: &ProviderData, col_widths: &[u16]) -> Row<'a> {
    let (icon, icon_color) = match item.kind {
        WorkItemKind::Checkout => {
            if !item.workspace_refs.is_empty() {
                ("●", Color::Green)
            } else {
                ("○", Color::Green)
            }
        }
        WorkItemKind::Session => {
            let session = item
                .session_key
                .as_deref()
                .and_then(|k| providers.sessions.get(k));
            match session.map(|s| &s.status) {
                Some(SessionStatus::Running) => ("▶", Color::Magenta),
                Some(SessionStatus::Idle) => ("◆", Color::Magenta),
                _ => ("○", Color::Magenta),
            }
        }
        WorkItemKind::Pr => ("⊙", Color::Blue),
        WorkItemKind::RemoteBranch => ("⊶", Color::DarkGray),
        WorkItemKind::Issue => ("◇", Color::Yellow),
    };

    let desc_width = col_widths.get(1).copied().unwrap_or(15) as usize;
    let branch_width = col_widths.get(2).copied().unwrap_or(25) as usize;

    let description = truncate(&item.description, desc_width);

    let wt_indicator = if item.is_main_worktree {
        "◆"
    } else if item.checkout_key().is_some() {
        "✓"
    } else {
        ""
    };

    let ws_indicator = match item.workspace_refs.len() {
        0 => String::new(),
        1 => "●".to_string(),
        n => format!("{n}"),
    };

    let branch = item.branch.as_deref().unwrap_or("—");
    let branch_display = truncate(branch, branch_width);

    let pr_display = if let Some(ref pr_key) = item.pr_key {
        if let Some(cr) = providers.change_requests.get(pr_key.as_str()) {
            let state_icon = match cr.status {
                ChangeRequestStatus::Merged => "✓",
                ChangeRequestStatus::Closed => "✗",
                _ => "",
            };
            format!("#{}{}", cr.id, state_icon)
        } else {
            String::new()
        }
    } else {
        String::new()
    };

    let session_display = if let Some(ref ses_key) = item.session_key {
        if let Some(ses) = providers.sessions.get(ses_key.as_str()) {
            match ses.status {
                SessionStatus::Running => "▶".to_string(),
                SessionStatus::Idle => "◆".to_string(),
                SessionStatus::Archived => "○".to_string(),
            }
        } else {
            String::new()
        }
    } else {
        String::new()
    };

    let issues_display = item
        .issue_keys
        .iter()
        .filter_map(|k| providers.issues.get(k.as_str()))
        .map(|i| format!("#{}", i.id))
        .collect::<Vec<_>>()
        .join(",");

    let git_display = if let Some(wt_key) = item.checkout_key() {
        if let Some(co) = providers.checkouts.get(wt_key) {
            let mut s = String::new();
            if co.working_tree.as_ref().is_some_and(|w| w.modified > 0) {
                s.push('M');
            }
            if co.working_tree.as_ref().is_some_and(|w| w.staged > 0) {
                s.push('S');
            }
            if co.working_tree.as_ref().is_some_and(|w| w.untracked > 0) {
                s.push('?');
            }
            if co.trunk_ahead_behind.as_ref().is_some_and(|m| m.ahead > 0) {
                s.push('↑');
            }
            s
        } else {
            String::new()
        }
    } else {
        String::new()
    };

    Row::new(vec![
        Cell::from(Span::styled(
            format!(" {icon}"),
            Style::default().fg(icon_color),
        )),
        Cell::from(description),
        Cell::from(Span::styled(
            branch_display,
            Style::default().fg(Color::Cyan),
        )),
        Cell::from(Span::styled(
            wt_indicator.to_string(),
            Style::default().fg(Color::Green),
        )),
        Cell::from(Span::styled(
            ws_indicator,
            Style::default().fg(Color::Green),
        )),
        Cell::from(Span::styled(pr_display, Style::default().fg(Color::Blue))),
        Cell::from(Span::styled(
            session_display,
            Style::default().fg(Color::Magenta),
        )),
        Cell::from(Span::styled(
            issues_display,
            Style::default().fg(Color::Yellow),
        )),
        Cell::from(Span::styled(git_display, Style::default().fg(Color::Red))),
    ])
}

fn render_preview(model: &TuiModel, ui: &UiState, frame: &mut Frame, area: Rect) {
    if ui.show_debug {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(area);
        render_preview_content(model, ui, frame, chunks[0]);
        render_debug_panel(model, ui, frame, chunks[1]);
    } else {
        render_preview_content(model, ui, frame, area);
    }
}

fn render_preview_content(model: &TuiModel, ui: &UiState, frame: &mut Frame, area: Rect) {
    let text = if let Some(item) = selected_work_item(model, ui) {
        let rm = model.active();
        let providers = &rm.providers;
        let mut lines = Vec::new();

        lines.push(format!("Description: {}", item.description));

        if let Some(ref branch) = item.branch {
            lines.push(format!("Branch: {}", branch));
        }

        if let Some(wt_key) = item.checkout_key() {
            if let Some(co) = providers.checkouts.get(wt_key) {
                lines.push(format!("Path: {}", co.path.display()));
                if let Some(commit) = &co.last_commit {
                    let sha = if commit.short_sha.is_empty() {
                        "?"
                    } else {
                        &commit.short_sha
                    };
                    lines.push(format!("Commit: {} {}", sha, commit.message));
                }
                if let Some(main) = &co.trunk_ahead_behind {
                    if main.ahead > 0 || main.behind > 0 {
                        lines.push(format!("vs main: +{} -{}", main.ahead, main.behind));
                    }
                }
                if let Some(remote) = &co.remote_ahead_behind {
                    if remote.ahead > 0 || remote.behind > 0 {
                        lines.push(format!("vs remote: +{} -{}", remote.ahead, remote.behind));
                    }
                }
            }
        }

        if let Some(ref pr_key) = item.pr_key {
            if let Some(cr) = providers.change_requests.get(pr_key.as_str()) {
                lines.push(format!(
                    "{} #{}: {}",
                    model.active_labels().code_review.abbr,
                    cr.id,
                    cr.title
                ));
                lines.push(format!("State: {:?}", cr.status));
            }
        }

        if let Some(ref ses_key) = item.session_key {
            if let Some(ses) = providers.sessions.get(ses_key.as_str()) {
                lines.push(format!("Session: {}", ses.title));
                lines.push(format!("Status: {:?}", ses.status));
                if let Some(ref model_name) = ses.model {
                    lines.push(format!("Model: {}", model_name));
                }
                if let Some(ref updated) = ses.updated_at {
                    let display = updated.split('T').next().unwrap_or(updated);
                    lines.push(format!("Updated: {}", display));
                }
            }
        }

        for ws_ref in &item.workspace_refs {
            if let Some(ws) = providers.workspaces.get(ws_ref.as_str()) {
                let name = if ws.name.is_empty() {
                    &ws.ws_ref
                } else {
                    &ws.name
                };
                lines.push(format!("Workspace: {}", name));
            }
        }

        for issue_key in &item.issue_keys {
            if let Some(issue) = providers.issues.get(issue_key.as_str()) {
                let labels = issue.labels.join(", ");
                lines.push(format!("Issue #{}: {} [{}]", issue.id, issue.title, labels));
            }
        }

        lines.join("\n")
    } else {
        String::new()
    };

    let preview = Paragraph::new(text)
        .block(Block::bordered().title(" Preview "))
        .wrap(Wrap { trim: true });
    frame.render_widget(preview, area);
}

fn render_debug_panel(model: &TuiModel, ui: &UiState, frame: &mut Frame, area: Rect) {
    let text = if let Some(item) = selected_work_item(model, ui) {
        if !item.debug_group.is_empty() {
            item.debug_group.join("\n")
        } else {
            "Not correlated (standalone)".into()
        }
    } else {
        String::new()
    };

    let panel = Paragraph::new(text)
        .block(Block::bordered().title(" Debug (D to toggle) "))
        .wrap(Wrap { trim: true });
    frame.render_widget(panel, area);
}

fn render_action_menu(model: &TuiModel, ui: &mut UiState, frame: &mut Frame) {
    let UiMode::ActionMenu { ref items, index } = ui.mode else {
        return;
    };

    let area = popup_area(frame.area(), 40, 40);
    ui.layout.menu_area = area;
    frame.render_widget(Clear, area);

    let labels = model.active_labels();
    let list_items: Vec<ListItem> = items
        .iter()
        .enumerate()
        .map(|(i, intent)| ListItem::new(format!(" {}: {}", i + 1, intent.label(labels))))
        .collect();

    let list = List::new(list_items)
        .block(Block::bordered().title(" Actions "))
        .highlight_style(Style::default().bg(Color::Blue).bold())
        .highlight_symbol("▸ ");

    let mut state = ListState::default();
    state.select(Some(index));
    frame.render_stateful_widget(list, area, &mut state);
}

fn render_input_popup(ui: &UiState, frame: &mut Frame) {
    let UiMode::BranchInput {
        ref input,
        generating,
        ..
    } = ui.mode
    else {
        return;
    };

    let area = popup_area(frame.area(), 50, 20);
    frame.render_widget(Clear, area);

    let inner = Block::bordered().title(" New Branch ");
    let inner_area = inner.inner(area);
    frame.render_widget(inner, area);

    if generating {
        let paragraph =
            Paragraph::new("  Generating branch name...").style(Style::default().fg(Color::Yellow));
        frame.render_widget(paragraph, inner_area);
        return;
    }

    let input_text = input.value();
    let display = format!("> {}", input_text);
    let paragraph = Paragraph::new(display).style(Style::default().fg(Color::Cyan));
    frame.render_widget(paragraph, inner_area);

    let cursor_x = inner_area.x + 2 + input.visual_cursor() as u16;
    let cursor_y = inner_area.y;
    frame.set_cursor_position((cursor_x, cursor_y));
}

fn render_delete_confirm(model: &TuiModel, ui: &UiState, frame: &mut Frame) {
    let UiMode::DeleteConfirm { ref info, loading } = ui.mode else {
        return;
    };

    let area = popup_area(frame.area(), 60, 50);
    frame.render_widget(Clear, area);

    let mut lines: Vec<Line> = Vec::new();

    if loading {
        lines.push(Line::from(Span::styled(
            "  Loading safety info...",
            Style::default().fg(Color::Yellow),
        )));
    } else if let Some(info) = info {
        lines.push(Line::from(vec![
            Span::raw("  Branch: "),
            Span::styled(&info.branch, Style::default().bold()),
        ]));
        lines.push(Line::from(""));

        if let Some(pr_status) = &info.pr_status {
            let (status_text, color) = match pr_status.as_str() {
                "MERGED" => ("MERGED", Color::Green),
                "CLOSED" => ("CLOSED", Color::Yellow),
                "OPEN" => ("OPEN", Color::Red),
                _ => (pr_status.as_str(), Color::White),
            };
            lines.push(Line::from(vec![
                Span::raw(format!("  {}: ", model.active_labels().code_review.abbr)),
                Span::styled(status_text, Style::default().fg(color).bold()),
            ]));
            if let Some(sha) = &info.merge_commit_sha {
                lines.push(Line::from(format!("  Merge commit: {}", sha)));
            }
        } else {
            lines.push(Line::from(Span::styled(
                format!("  No {} found", model.active_labels().code_review.abbr),
                Style::default().fg(Color::DarkGray),
            )));
        }

        lines.push(Line::from(""));

        if info.has_uncommitted {
            lines.push(Line::from(Span::styled(
                "  ⚠ Has uncommitted changes",
                Style::default().fg(Color::Red).bold(),
            )));
        }

        if let Some(warning) = &info.base_detection_warning {
            lines.push(Line::from(Span::styled(
                format!("  ⚠ {}", warning),
                Style::default().fg(Color::Yellow),
            )));
        } else if !info.unpushed_commits.is_empty() {
            lines.push(Line::from(Span::styled(
                format!("  ⚠ {} unpushed commit(s):", info.unpushed_commits.len()),
                Style::default().fg(Color::Red).bold(),
            )));
            for commit in info.unpushed_commits.iter().take(5) {
                lines.push(Line::from(format!("    {}", commit)));
            }
        }

        if !info.has_uncommitted
            && info.unpushed_commits.is_empty()
            && info.base_detection_warning.is_none()
            && info.pr_status.as_deref() == Some("MERGED")
        {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "  ✓ Safe to delete",
                Style::default().fg(Color::Green).bold(),
            )));
        }

        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  y/Enter: confirm    n/Esc: cancel",
            Style::default().fg(Color::DarkGray),
        )));
    }

    let title = format!(
        " Remove {} ",
        model.active_labels().checkouts.noun_capitalized()
    );
    let paragraph = Paragraph::new(lines)
        .block(Block::bordered().title(title))
        .wrap(Wrap { trim: true });
    frame.render_widget(paragraph, area);
}

fn render_help(model: &TuiModel, ui: &UiState, frame: &mut Frame) {
    if !matches!(ui.mode, UiMode::Help) {
        return;
    }

    let area = popup_area(frame.area(), 60, 70);
    frame.render_widget(Clear, area);

    let labels = model.active_labels();
    let help_text = vec![
        Line::from(Span::styled("Navigation", Style::default().bold())),
        Line::from("  j/k or ↑/↓      Navigate list"),
        Line::from("  Click            Select item"),
        Line::from("  Scroll wheel     Navigate list"),
        Line::from(""),
        Line::from(Span::styled("Actions", Style::default().bold())),
        Line::from("  Enter            Open workspace (switch/create as needed)"),
        Line::from("  Double-click     Same as Enter"),
        Line::from("  .                Action menu (all available actions)"),
        Line::from("  Right-click      Action menu"),
        Line::from(format!(
            "  n                New branch (enter name, creates {})",
            labels.checkouts.noun
        )),
        Line::from(format!(
            "  d                Remove {} (with safety check)",
            labels.checkouts.noun
        )),
        Line::from(format!(
            "  p                Show {} in browser",
            labels.code_review.abbr
        )),
        Line::from("  r                Refresh data"),
        Line::from(""),
        Line::from(Span::styled(
            "Multi-select (issues)",
            Style::default().bold(),
        )),
        Line::from("  Space            Toggle selection on current item"),
        Line::from("  Enter            Generate branch name for all selected"),
        Line::from("  Esc              Clear selection"),
        Line::from(""),
        Line::from(Span::styled("Repos", Style::default().bold())),
        Line::from("  [ / ]            Switch repo tab"),
        Line::from("  { / }            Move repo tab left/right"),
        Line::from("  Drag tab         Reorder tabs"),
        Line::from("  a                Add repository"),
        Line::from(""),
        Line::from(Span::styled("General", Style::default().bold())),
        Line::from("  D                Toggle correlation debug panel"),
        Line::from("  ?                Toggle this help"),
        Line::from("  q / Esc          Quit"),
    ];

    let paragraph = Paragraph::new(help_text)
        .block(Block::bordered().title(" Help "))
        .wrap(Wrap { trim: true });
    frame.render_widget(paragraph, area);
}

fn render_file_picker(ui: &mut UiState, frame: &mut Frame) {
    let UiMode::FilePicker {
        ref input,
        ref dir_entries,
        selected,
    } = ui.mode
    else {
        return;
    };

    let area = popup_area(frame.area(), 60, 60);
    ui.layout.file_picker_area = area;
    frame.render_widget(Clear, area);

    let block = Block::bordered().title(" Add Repository ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .split(inner);

    ui.layout.file_picker_list_area = chunks[1];

    let input_text = input.value();
    let display = format!("> {}", input_text);
    let paragraph = Paragraph::new(display).style(Style::default().fg(Color::Cyan));
    frame.render_widget(paragraph, chunks[0]);

    let cursor_x = chunks[0].x + 2 + input.visual_cursor() as u16;
    frame.set_cursor_position((cursor_x, chunks[0].y));

    let items: Vec<ListItem> = dir_entries
        .iter()
        .map(|entry| {
            let tag = if entry.is_added {
                " (added)"
            } else if entry.is_git_repo {
                " (git repo)"
            } else if entry.is_dir {
                "/"
            } else {
                ""
            };
            let style = if entry.is_git_repo && !entry.is_added {
                Style::default().fg(Color::Green)
            } else if entry.is_added {
                Style::default().fg(Color::DarkGray)
            } else {
                Style::default()
            };
            ListItem::new(format!("  {}{}", entry.name, tag)).style(style)
        })
        .collect();

    let list = List::new(items)
        .highlight_style(Style::default().bg(Color::DarkGray).bold())
        .highlight_symbol("▸ ");

    let mut state = ListState::default();
    if !dir_entries.is_empty() {
        state.select(Some(selected));
    }
    frame.render_stateful_widget(list, chunks[1], &mut state);
}

fn render_config_screen(model: &TuiModel, ui: &mut UiState, frame: &mut Frame, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
        .split(area);

    render_global_status(model, frame, chunks[0]);
    render_event_log(ui, frame, chunks[1]);
}

fn render_global_status(model: &TuiModel, frame: &mut Frame, area: Rect) {
    let mut lines: Vec<Line> = Vec::new();

    // Collect provider names across all repos
    let mut vcs_name: Option<String> = None;
    let mut checkout_name: Option<String> = None;
    let mut code_review_name: Option<String> = None;
    let mut issue_tracker_name: Option<String> = None;
    let mut coding_agent_name: Option<String> = None;
    let mut ai_utility_name: Option<String> = None;
    let mut workspace_name: Option<String> = None;

    let mut coding_agent_status: Option<ProviderStatus> = None;

    for path in &model.repo_order {
        let rm = &model.repos[path];

        if vcs_name.is_none() {
            vcs_name = rm.provider_names.get("vcs").cloned();
        }
        if checkout_name.is_none() {
            checkout_name = rm.provider_names.get("checkout_manager").cloned();
        }
        if code_review_name.is_none() {
            code_review_name = rm.provider_names.get("code_review").cloned();
        }
        if issue_tracker_name.is_none() {
            issue_tracker_name = rm.provider_names.get("issue_tracker").cloned();
        }
        if coding_agent_name.is_none() {
            coding_agent_name = rm.provider_names.get("coding_agent").cloned();
        }
        if ai_utility_name.is_none() {
            ai_utility_name = rm.provider_names.get("ai_utility").cloned();
        }
        if workspace_name.is_none() {
            workspace_name = rm.provider_names.get("workspace_manager").cloned();
        }

        if coding_agent_status.is_none() {
            if let Some(pname) = rm.provider_names.get("coding_agent") {
                if let Some(&status) = model.provider_statuses.get(&(
                    path.clone(),
                    "coding_agent".into(),
                    pname.clone(),
                )) {
                    coding_agent_status = Some(status);
                }
            }
        }
    }

    lines.push(Line::from(Span::styled(
        "── Provider Factories ──",
        Style::default().fg(Color::Yellow).bold(),
    )));

    let factories: Vec<(&str, &Option<String>)> = vec![
        ("VCS", &vcs_name),
        ("Checkout mgr", &checkout_name),
        ("Code review", &code_review_name),
        ("Issue tracker", &issue_tracker_name),
        ("Coding agent", &coding_agent_name),
        ("AI utility", &ai_utility_name),
        ("Workspace mgr", &workspace_name),
    ];

    for (category, name) in factories {
        let value = name.as_deref().unwrap_or("—");
        let style = if name.is_some() {
            Style::default().fg(Color::White)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        lines.push(Line::from(vec![
            Span::styled(
                format!("  {:<16}", category),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled(value, style),
        ]));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "── Coding Agent Status ──",
        Style::default().fg(Color::Yellow).bold(),
    )));

    if let Some(agent_name) = &coding_agent_name {
        let (status_text, color) = match coding_agent_status {
            Some(ProviderStatus::Ok) => ("✓ authenticated", Color::Green),
            Some(ProviderStatus::Error) => ("✗ auth error", Color::Red),
            None => ("? unknown", Color::DarkGray),
        };
        lines.push(Line::from(vec![
            Span::styled(
                format!("  {:<16}", agent_name),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled(status_text, Style::default().fg(color)),
        ]));
    } else {
        lines.push(Line::from(Span::styled(
            "  No coding agent configured",
            Style::default().fg(Color::DarkGray),
        )));
    }

    let paragraph = Paragraph::new(lines)
        .block(Block::bordered())
        .wrap(Wrap { trim: true });
    frame.render_widget(paragraph, area);
}

fn render_event_log(ui: &mut UiState, frame: &mut Frame, area: Rect) {
    use event_log::DisplayEntry;

    let filter = ui.event_log.filter;
    let entries = event_log::get_entries(&filter);
    let entry_count = entries.len();

    if entry_count != ui.event_log.count {
        ui.event_log.count = entry_count;
        if entry_count > 0 {
            ui.event_log.selected = Some(entry_count - 1);
        }
    }

    let items: Vec<ListItem> = entries
        .iter()
        .map(|display_entry| match display_entry {
            DisplayEntry::Log(entry) => {
                let (h, m, s) = entry.hms;
                let timestamp = format!("{h:02}:{m:02}:{s:02}");

                let level_color = match entry.level {
                    tracing::Level::ERROR => Color::Red,
                    tracing::Level::WARN => Color::Yellow,
                    tracing::Level::DEBUG => Color::Cyan,
                    tracing::Level::TRACE => Color::DarkGray,
                    _ => Color::DarkGray,
                };

                ListItem::new(Line::from(vec![
                    Span::styled(
                        format!("{} ", timestamp),
                        Style::default().fg(Color::DarkGray),
                    ),
                    Span::styled(
                        format!("{:<5} ", entry.level),
                        Style::default().fg(level_color),
                    ),
                    Span::raw(&entry.message),
                ]))
            }
            DisplayEntry::RetentionMarker(level) => ListItem::new(Line::from(Span::styled(
                format!("── {level} retention starts here ──"),
                Style::default().fg(Color::DarkGray),
            ))),
        })
        .collect();

    let filter_label = format!(" {} ", filter.filter_label());
    let filter_label_len = filter_label.len() as u16;
    let filter_x = area.x + area.width.saturating_sub(filter_label_len + 1);
    ui.layout.event_log_filter_area = Rect::new(filter_x, area.y, filter_label_len, 1);

    let list = List::new(items)
        .block(
            Block::bordered().title(" Event Log ").title_top(
                Line::from(Span::styled(
                    filter_label,
                    Style::default().fg(Color::DarkGray),
                ))
                .right_aligned(),
            ),
        )
        .highlight_style(Style::default().bg(Color::Indexed(236)));

    let mut state = ListState::default();
    state.select(ui.event_log.selected);
    frame.render_stateful_widget(list, area, &mut state);
}

fn truncate(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    let char_count: usize = s.chars().count();
    if char_count <= max {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max - 1).collect();
        format!("{truncated}…")
    }
}

fn popup_area(area: Rect, percent_x: u16, percent_y: u16) -> Rect {
    let [area] = Layout::vertical([Constraint::Percentage(percent_y)])
        .flex(Flex::Center)
        .areas(area);
    let [area] = Layout::horizontal([Constraint::Percentage(percent_x)])
        .flex(Flex::Center)
        .areas(area);
    area
}
