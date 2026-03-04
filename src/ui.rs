use ratatui::{
    layout::{Constraint, Direction, Flex, Layout, Rect},
    style::{Color, Style, Stylize},
    text::{Line, Span},
    widgets::{
        Block, Cell, Clear, HighlightSpacing, List, ListItem, ListState, Paragraph, Row, Table,
        Wrap,
    },
    Frame,
};

use crate::app::{Action, App};
use crate::data::{SectionHeader, TableEntry, WorkItem, WorkItemKind};
use crate::providers::types::{ChangeRequestStatus, SessionStatus};

pub fn render(app: &mut App, frame: &mut Frame) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(frame.area());

    render_tab_bar(app, frame, chunks[0]);
    render_content(app, frame, chunks[1]);
    render_status_bar(app, frame, chunks[2]);
    render_action_menu(app, frame);
    render_input_popup(app, frame);
    render_delete_confirm(app, frame);
    render_help(app, frame);
    render_file_picker(app, frame);
}

fn render_tab_bar(app: &mut App, frame: &mut Frame, area: Rect) {
    let mut spans: Vec<Span> = vec![
        Span::styled(" cmux ", Style::default().bold().fg(Color::Cyan)),
    ];

    app.tab_areas.clear();
    let mut x_offset: u16 = 6; // length of " cmux "

    for (i, path) in app.repo_order.iter().enumerate() {
        let rs = &app.repos[path];
        let name = App::repo_name(path);
        let is_active = i == app.active_repo;
        let loading = if rs.data.loading { " ⟳" } else { "" };
        let changed = if rs.has_unseen_changes { "*" } else { "" };

        let sep = Span::styled(" | ", Style::default().fg(Color::DarkGray));
        spans.push(sep);
        x_offset += 3;

        let label = format!("{name}{changed}{loading}");
        let label_len = label.len() as u16;
        let style = if is_active {
            Style::default().bold().fg(Color::Cyan)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        spans.push(Span::styled(label, style));

        // Record tab area for mouse hit-testing
        app.tab_areas.push(Rect::new(area.x + x_offset, area.y, label_len, 1));
        x_offset += label_len;
    }

    // [+] button
    let add_sep = Span::styled(" | ", Style::default().fg(Color::DarkGray));
    spans.push(add_sep);
    x_offset += 3;
    let add_label = Span::styled("[+]", Style::default().fg(Color::Green));
    spans.push(add_label);
    app.add_tab_area = Rect::new(area.x + x_offset, area.y, 3, 1);

    let line = Line::from(spans);
    let title = Paragraph::new(line);
    frame.render_widget(title, area);
}

fn render_status_bar(app: &App, frame: &mut Frame, area: Rect) {
    if let Some(err) = &app.status_message {
        let msg = format!(" Error: {}", err);
        let status = Paragraph::new(msg).style(Style::default().fg(Color::Red));
        frame.render_widget(status, area);
        return;
    }

    let text: String = if app.generating_branch {
        " Generating branch name...".into()
    } else if app.show_action_menu {
        " j/k:navigate  enter:select  esc:close".into()
    } else if app.input_mode == crate::app::InputMode::BranchName {
        " type branch name  enter:create  esc:cancel".into()
    } else if app.input_mode == crate::app::InputMode::AddRepo {
        " j/k:navigate  tab:complete  enter:select  esc:cancel".into()
    } else if app.show_delete_confirm {
        " y/enter:confirm  n/esc:cancel".into()
    } else if !app.active().multi_selected.is_empty() {
        " enter:create branch  shift+enter:toggle  esc:clear  ?:help  q:quit".into()
    } else {
        let mut s = " enter:open".to_string();
        if let Some(item) = app.selected_work_item() {
            for &action in Action::all_in_menu_order() {
                if let Some(hint) = action.shortcut_hint() {
                    if action.is_available(item) {
                        s.push_str("  ");
                        s.push_str(hint);
                    }
                }
            }
        }
        s.push_str("  space:menu  n:new  r:refresh  shift+enter:select  ?:help  q:quit");
        s
    };

    let status = Paragraph::new(text).style(Style::default().fg(Color::DarkGray));
    frame.render_widget(status, area);
}

fn render_content(app: &mut App, frame: &mut Frame, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
        .split(area);

    render_unified_table(app, frame, chunks[0]);
    render_preview(app, frame, chunks[1]);
}

fn render_unified_table(app: &mut App, frame: &mut Frame, area: Rect) {
    app.table_area = area;

    let header = Row::new(vec![
        Cell::from(""),
        Cell::from("Description"),
        Cell::from("WT"),
        Cell::from("WS"),
        Cell::from("Branch"),
        Cell::from("PR"),
        Cell::from("Ses"),
        Cell::from("Issues"),
        Cell::from("Git"),
    ])
    .style(Style::default().fg(Color::DarkGray).bold())
    .height(1);

    let widths = [
        Constraint::Length(3),
        Constraint::Min(15),
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Length(25),
        Constraint::Length(10),
        Constraint::Length(4),
        Constraint::Length(10),
        Constraint::Length(5),
    ];

    // Resolve actual column widths for truncation
    // Account for border (2) and highlight spacing (2)
    let inner_width = area.width.saturating_sub(4);
    let col_areas = Layout::horizontal(widths).split(Rect::new(0, 0, inner_width, 1));
    let col_widths: Vec<u16> = col_areas.iter().map(|r| r.width).collect();

    // Build rows from active repo state (immutable borrow)
    let active = app.active();
    let rows: Vec<Row> = active
        .data
        .table_entries
        .iter()
        .enumerate()
        .map(|(table_idx, entry)| {
            let is_multi_selected = active
                .data
                .selectable_indices
                .iter()
                .position(|&idx| idx == table_idx)
                .map(|si| active.multi_selected.contains(&si))
                .unwrap_or(false);

            match entry {
                TableEntry::Header(header) => build_header_row(header),
                TableEntry::Item(item) => {
                    let mut row = build_item_row(item, &active.data, &col_widths);
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
        .block(Block::bordered())
        .row_highlight_style(Style::default().bg(Color::DarkGray).bold())
        .highlight_symbol("▸ ")
        .highlight_spacing(HighlightSpacing::Always);

    // Now mutably borrow for stateful render
    let key = &app.repo_order[app.active_repo];
    let rs = app.repos.get_mut(key).unwrap();
    frame.render_stateful_widget(table, area, &mut rs.table_state);
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

fn build_item_row<'a>(item: &WorkItem, data: &crate::data::DataStore, col_widths: &[u16]) -> Row<'a> {
    let (icon, icon_color) = match item.kind {
        WorkItemKind::Worktree => {
            if !item.workspace_refs.is_empty() {
                ("●", Color::Green)
            } else {
                ("○", Color::Green)
            }
        }
        WorkItemKind::Session => {
            let session = item.session_idx.and_then(|idx| data.sessions.get(idx));
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
    let branch_width = col_widths.get(4).copied().unwrap_or(25) as usize;

    let description = truncate(&item.description, desc_width);

    let wt_indicator = if item.is_main_worktree {
        "◆"
    } else if item.worktree_idx.is_some() {
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

    let pr_display = if let Some(pr_idx) = item.pr_idx {
        if let Some(cr) = data.change_requests.get(pr_idx) {
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

    let session_display = if let Some(ses_idx) = item.session_idx {
        if let Some(ses) = data.sessions.get(ses_idx) {
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
        .issue_idxs
        .iter()
        .filter_map(|&idx| data.issues.get(idx))
        .map(|i| format!("#{}", i.id))
        .collect::<Vec<_>>()
        .join(",");

    let git_display = if let Some(wt_idx) = item.worktree_idx {
        if let Some(co) = data.checkouts.get(wt_idx) {
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
        Cell::from(Span::styled(format!(" {icon}"), Style::default().fg(icon_color))),
        Cell::from(description),
        Cell::from(Span::styled(
            wt_indicator.to_string(),
            Style::default().fg(Color::Green),
        )),
        Cell::from(Span::styled(
            ws_indicator,
            Style::default().fg(Color::Green),
        )),
        Cell::from(Span::styled(
            branch_display,
            Style::default().fg(Color::Cyan),
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

fn render_preview(app: &App, frame: &mut Frame, area: Rect) {
    let text = if let Some(item) = app.selected_work_item() {
        let mut lines = Vec::new();

        // Description
        lines.push(format!("Description: {}", item.description));

        // Branch
        if let Some(branch) = &item.branch {
            lines.push(format!("Branch: {}", branch));
        }

        // Checkout info
        if let Some(wt_idx) = item.worktree_idx {
            if let Some(co) = app.active().data.checkouts.get(wt_idx) {
                lines.push(format!("Path: {}", co.path.display()));
                if let Some(commit) = &co.last_commit {
                    let sha = if commit.short_sha.is_empty() { "?" } else { &commit.short_sha };
                    lines.push(format!("Commit: {} {}", sha, commit.message));
                }
                if let Some(main) = &co.trunk_ahead_behind {
                    if main.ahead > 0 || main.behind > 0 {
                        lines.push(format!("vs main: +{} -{}", main.ahead, main.behind));
                    }
                }
                if let Some(remote) = &co.remote_ahead_behind {
                    if remote.ahead > 0 || remote.behind > 0 {
                        lines.push(format!(
                            "vs remote: +{} -{}",
                            remote.ahead, remote.behind
                        ));
                    }
                }
            }
        }

        // PR info
        if let Some(pr_idx) = item.pr_idx {
            if let Some(cr) = app.active().data.change_requests.get(pr_idx) {
                lines.push(format!("PR #{}: {}", cr.id, cr.title));
                lines.push(format!("State: {:?}", cr.status));
            }
        }

        // Session info
        if let Some(ses_idx) = item.session_idx {
            if let Some(ses) = app.active().data.sessions.get(ses_idx) {
                lines.push(format!("Session: {}", ses.title));
                lines.push(format!("Status: {:?}", ses.status));
                if let Some(ref model) = ses.model {
                    lines.push(format!("Model: {}", model));
                }
                if let Some(ref updated) = ses.updated_at {
                    let display = updated.split('T').next().unwrap_or(updated);
                    lines.push(format!("Updated: {}", display));
                }
            }
        }

        // Workspaces
        for ws_ref in &item.workspace_refs {
            if let Some(ws) = app.active().data.workspaces.iter().find(|w| &w.ws_ref == ws_ref) {
                let name = if ws.name.is_empty() { &ws.ws_ref } else { &ws.name };
                lines.push(format!("Workspace: {}", name));
            }
        }

        // Issues
        for &issue_idx in &item.issue_idxs {
            if let Some(issue) = app.active().data.issues.get(issue_idx) {
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

fn render_action_menu(app: &mut App, frame: &mut Frame) {
    if !app.show_action_menu {
        return;
    }

    let area = popup_area(frame.area(), 40, 40);
    app.menu_area = area;
    frame.render_widget(Clear, area);

    let items: Vec<ListItem> = app
        .action_menu_items
        .iter()
        .enumerate()
        .map(|(i, action)| ListItem::new(format!(" {}: {}", i + 1, action.label())))
        .collect();

    let list = List::new(items)
        .block(Block::bordered().title(" Actions "))
        .highlight_style(Style::default().bg(Color::Blue).bold())
        .highlight_symbol("▸ ");

    let mut state = ListState::default();
    state.select(Some(app.action_menu_index));
    frame.render_stateful_widget(list, area, &mut state);
}

fn render_input_popup(app: &App, frame: &mut Frame) {
    if app.input_mode != crate::app::InputMode::BranchName && !app.generating_branch {
        return;
    }

    let area = popup_area(frame.area(), 50, 20);
    frame.render_widget(Clear, area);

    let inner = Block::bordered().title(" New Branch ");
    let inner_area = inner.inner(area);
    frame.render_widget(inner, area);

    if app.generating_branch {
        let paragraph = Paragraph::new("  Generating branch name...")
            .style(Style::default().fg(Color::Yellow));
        frame.render_widget(paragraph, inner_area);
        return;
    }

    let input_text = app.input.value();
    let display = format!("> {}", input_text);
    let paragraph = Paragraph::new(display).style(Style::default().fg(Color::Cyan));
    frame.render_widget(paragraph, inner_area);

    let cursor_x = inner_area.x + 2 + app.input.visual_cursor() as u16;
    let cursor_y = inner_area.y;
    frame.set_cursor_position((cursor_x, cursor_y));
}

fn render_delete_confirm(app: &App, frame: &mut Frame) {
    if !app.show_delete_confirm {
        return;
    }

    let area = popup_area(frame.area(), 60, 50);
    frame.render_widget(Clear, area);

    let mut lines: Vec<Line> = Vec::new();

    if app.delete_confirm_loading {
        lines.push(Line::from(Span::styled(
            "  Loading safety info...",
            Style::default().fg(Color::Yellow),
        )));
    } else if let Some(info) = &app.delete_confirm_info {
        lines.push(Line::from(vec![
            Span::raw("  Branch: "),
            Span::styled(&info.branch, Style::default().bold()),
        ]));
        lines.push(Line::from(""));

        // PR status
        if let Some(pr_status) = &info.pr_status {
            let (status_text, color) = match pr_status.as_str() {
                "MERGED" => ("MERGED", Color::Green),
                "CLOSED" => ("CLOSED", Color::Yellow),
                "OPEN" => ("OPEN", Color::Red),
                _ => (pr_status.as_str(), Color::White),
            };
            lines.push(Line::from(vec![
                Span::raw("  PR: "),
                Span::styled(status_text, Style::default().fg(color).bold()),
            ]));
            if let Some(sha) = &info.merge_commit_sha {
                lines.push(Line::from(format!("  Merge commit: {}", sha)));
            }
        } else {
            lines.push(Line::from(Span::styled(
                "  No PR found",
                Style::default().fg(Color::DarkGray),
            )));
        }

        lines.push(Line::from(""));

        // Uncommitted changes
        if info.has_uncommitted {
            lines.push(Line::from(Span::styled(
                "  ⚠ Has uncommitted changes",
                Style::default().fg(Color::Red).bold(),
            )));
        }

        // Unpushed commits
        if !info.unpushed_commits.is_empty() {
            lines.push(Line::from(Span::styled(
                format!("  ⚠ {} unpushed commit(s):", info.unpushed_commits.len()),
                Style::default().fg(Color::Red).bold(),
            )));
            for commit in info.unpushed_commits.iter().take(5) {
                lines.push(Line::from(format!("    {}", commit)));
            }
        }

        // Safe indicator
        if !info.has_uncommitted
            && info.unpushed_commits.is_empty()
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

    let paragraph = Paragraph::new(lines)
        .block(Block::bordered().title(" Remove Worktree "))
        .wrap(Wrap { trim: true });
    frame.render_widget(paragraph, area);
}

fn render_help(app: &App, frame: &mut Frame) {
    if !app.show_help {
        return;
    }

    let area = popup_area(frame.area(), 60, 70);
    frame.render_widget(Clear, area);

    let help_text = vec![
        Line::from(Span::styled("Navigation", Style::default().bold())),
        Line::from("  j/k or ↑/↓      Navigate list"),
        Line::from("  Click            Select item"),
        Line::from("  Scroll wheel     Navigate list"),
        Line::from(""),
        Line::from(Span::styled("Actions", Style::default().bold())),
        Line::from("  Enter            Open workspace (switch/create as needed)"),
        Line::from("  Double-click     Same as Enter"),
        Line::from("  Space            Action menu (all available actions)"),
        Line::from("  Right-click      Action menu"),
        Line::from("  n                New branch (enter name, creates worktree)"),
        Line::from("  d                Remove worktree (with safety check)"),
        Line::from("  p                Show PR in browser"),
        Line::from("  r                Refresh data"),
        Line::from(""),
        Line::from(Span::styled("Multi-select (issues)", Style::default().bold())),
        Line::from("  Shift+Enter      Toggle selection on current item"),
        Line::from("  Shift+Click      Toggle selection on clicked item"),
        Line::from("  Enter            Generate branch name for all selected"),
        Line::from("  Esc              Clear selection"),
        Line::from(""),
        Line::from(Span::styled("Repos", Style::default().bold())),
        Line::from("  [ / ]            Switch repo tab"),
        Line::from("  a                Add repository"),
        Line::from(""),
        Line::from(Span::styled("General", Style::default().bold())),
        Line::from("  ?                Toggle this help"),
        Line::from("  q / Esc          Quit"),
    ];

    let paragraph = Paragraph::new(help_text)
        .block(Block::bordered().title(" Help "))
        .wrap(Wrap { trim: true });
    frame.render_widget(paragraph, area);
}

fn render_file_picker(app: &mut App, frame: &mut Frame) {
    if app.input_mode != crate::app::InputMode::AddRepo {
        return;
    }

    let area = popup_area(frame.area(), 60, 60);
    app.file_picker_area = area;
    frame.render_widget(Clear, area);

    let block = Block::bordered().title(" Add Repository ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .split(inner);

    app.file_picker_list_area = chunks[1];

    // Input line
    let input_text = app.input.value();
    let display = format!("> {}", input_text);
    let paragraph = Paragraph::new(display).style(Style::default().fg(Color::Cyan));
    frame.render_widget(paragraph, chunks[0]);

    // Cursor
    let cursor_x = chunks[0].x + 2 + app.input.visual_cursor() as u16;
    frame.set_cursor_position((cursor_x, chunks[0].y));

    // Directory listing
    let items: Vec<ListItem> = app
        .dir_entries
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
    if !app.dir_entries.is_empty() {
        state.select(Some(app.dir_selected));
    }
    frame.render_stateful_widget(list, chunks[1], &mut state);
}

fn truncate(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    // Use char count for display width (good enough for most text)
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
