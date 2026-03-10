use std::path::Path;

use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::Color;

use flotilla_protocol::{ChangeRequestStatus, Checkout, SessionStatus, WorkItemKind};

/// Truncate a string to `max` characters, appending '…' if truncated.
pub fn truncate(s: &str, max: usize) -> String {
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

/// Calculate a centered popup rectangle within `area`.
pub fn popup_area(area: Rect, percent_x: u16, percent_y: u16) -> Rect {
    let [area] = Layout::vertical([Constraint::Percentage(percent_y)])
        .flex(Flex::Center)
        .areas(area);
    let [area] = Layout::horizontal([Constraint::Percentage(percent_x)])
        .flex(Flex::Center)
        .areas(area);
    area
}

/// Return (icon, color) for a work item based on its kind, workspace status,
/// and optional session status.
pub fn work_item_icon(
    kind: &WorkItemKind,
    has_workspace: bool,
    session_status: Option<&SessionStatus>,
) -> (&'static str, Color) {
    match kind {
        WorkItemKind::Checkout => {
            if has_workspace {
                ("●", Color::Green)
            } else {
                ("○", Color::Green)
            }
        }
        WorkItemKind::Session => match session_status {
            Some(SessionStatus::Running) => ("▶", Color::Magenta),
            Some(SessionStatus::Idle) => ("◆", Color::Magenta),
            _ => ("○", Color::Magenta),
        },
        WorkItemKind::ChangeRequest => ("⊙", Color::Blue),
        WorkItemKind::RemoteBranch => ("⊶", Color::DarkGray),
        WorkItemKind::Issue => ("◇", Color::Yellow),
    }
}

/// Return the display icon for a session status.
pub fn session_status_display(status: &SessionStatus) -> &'static str {
    match status {
        SessionStatus::Running => "▶",
        SessionStatus::Idle => "◆",
        SessionStatus::Archived => "○",
    }
}

/// Return the display icon for a change request status.
pub fn change_request_status_icon(status: &ChangeRequestStatus) -> &'static str {
    match status {
        ChangeRequestStatus::Merged => "✓",
        ChangeRequestStatus::Closed => "✗",
        ChangeRequestStatus::Open | ChangeRequestStatus::Draft => "",
    }
}

/// Build the git status indicator string (e.g. "MS?↑") from a checkout.
pub fn git_status_display(checkout: &Checkout) -> String {
    let mut s = String::new();
    if checkout
        .working_tree
        .as_ref()
        .is_some_and(|w| w.modified > 0)
    {
        s.push('M');
    }
    if checkout.working_tree.as_ref().is_some_and(|w| w.staged > 0) {
        s.push('S');
    }
    if checkout
        .working_tree
        .as_ref()
        .is_some_and(|w| w.untracked > 0)
    {
        s.push('?');
    }
    if checkout
        .trunk_ahead_behind
        .as_ref()
        .is_some_and(|m| m.ahead > 0)
    {
        s.push('↑');
    }
    s
}

/// Return the checkout indicator: "◆" for main, "✓" for checked out, "" otherwise.
pub fn checkout_indicator(is_main: bool, has_checkout: bool) -> &'static str {
    if is_main {
        "◆"
    } else if has_checkout {
        "✓"
    } else {
        ""
    }
}

/// Shorten a checkout path for display in the table.
///
/// Main checkout shows home-relative (e.g. `~/dev/flotilla`).
/// Other checkouts are indented to show hierarchy: sibling worktrees show the
/// suffix (e.g. `.low-hang-12`), nested worktrees show the relative path
/// (e.g. `.worktrees/feat-auth`).  Padding matches the parent-directory portion
/// of the main display, but shrinks when `col_width` is tight so the actual
/// name is preserved.
pub fn shorten_path(path: &Path, repo_root: &Path, col_width: usize) -> String {
    let main_display = shorten_against_home(repo_root);

    // Main checkout — show the full shortened path.
    if path == repo_root {
        return main_display;
    }

    // Ideal padding = width of the parent-directory portion of main_display.
    let repo_name_len = repo_root
        .file_name()
        .map(|n| n.to_string_lossy().len())
        .unwrap_or(0);
    let ideal_padding = main_display.len().saturating_sub(repo_name_len);

    // Cap padding so it never exceeds half the column — leaves room for the name
    // even after the caller truncates.  Crucially this is independent of the name
    // length, so every worktree at the same depth gets identical indentation.
    let padding = ideal_padding.min(col_width / 2);

    // Under repo root (e.g. .worktrees/feat-auth, sub/dir)
    if let Ok(rel) = path.strip_prefix(repo_root) {
        let s = rel.to_string_lossy();
        if !s.is_empty() {
            let name = s.into_owned();
            return format!("{:padding$}{name}", "");
        }
    }

    // Sibling directory (same parent as repo root)
    if let (Some(path_parent), Some(root_parent)) = (path.parent(), repo_root.parent()) {
        if path_parent == root_parent {
            let root_name = repo_root
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let path_name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            // Strip repo name prefix to show just the suffix (e.g. ".low-hang-12")
            let name = path_name
                .strip_prefix(&root_name)
                .unwrap_or(&path_name)
                .to_string();
            return format!("{:padding$}{name}", "");
        }
    }

    // Elsewhere — shorten against home directory.
    shorten_against_home(path)
}

fn shorten_against_home(path: &Path) -> String {
    if let Some(home) = dirs::home_dir() {
        if let Ok(rel) = path.strip_prefix(&home) {
            let s = rel.to_string_lossy();
            if s.is_empty() {
                return "~".to_string();
            }
            return format!("~/{s}");
        }
    }
    path.display().to_string()
}

/// Return the workspace indicator: "" for 0, "●" for 1, count as string for >1.
pub fn workspace_indicator(count: usize) -> String {
    match count {
        0 => String::new(),
        1 => "●".to_string(),
        n => format!("{n}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flotilla_protocol::{AheadBehind, WorkingTreeStatus};

    #[test]
    fn truncate_empty_max() {
        assert_eq!(truncate("hello", 0), "");
    }

    #[test]
    fn truncate_short_string() {
        assert_eq!(truncate("hi", 5), "hi");
    }

    #[test]
    fn truncate_exact_length() {
        assert_eq!(truncate("hello", 5), "hello");
    }

    #[test]
    fn truncate_long_string() {
        assert_eq!(truncate("hello world", 5), "hell…");
    }

    #[test]
    fn truncate_max_one() {
        // max=1 means 0 chars + ellipsis
        assert_eq!(truncate("hello", 1), "…");
    }

    #[test]
    fn popup_area_centered() {
        let area = Rect::new(0, 0, 100, 50);
        let popup = popup_area(area, 50, 50);
        // Should be centered and smaller
        assert!(popup.width <= 50);
        assert!(popup.height <= 25);
        assert!(popup.x > 0);
        assert!(popup.y > 0);
    }

    #[test]
    fn work_item_icon_checkout_with_workspace() {
        let (icon, color) = work_item_icon(&WorkItemKind::Checkout, true, None);
        assert_eq!(icon, "●");
        assert_eq!(color, Color::Green);
    }

    #[test]
    fn work_item_icon_checkout_without_workspace() {
        let (icon, color) = work_item_icon(&WorkItemKind::Checkout, false, None);
        assert_eq!(icon, "○");
        assert_eq!(color, Color::Green);
    }

    #[test]
    fn work_item_icon_session_running() {
        let (icon, color) =
            work_item_icon(&WorkItemKind::Session, false, Some(&SessionStatus::Running));
        assert_eq!(icon, "▶");
        assert_eq!(color, Color::Magenta);
    }

    #[test]
    fn work_item_icon_session_idle() {
        let (icon, color) =
            work_item_icon(&WorkItemKind::Session, false, Some(&SessionStatus::Idle));
        assert_eq!(icon, "◆");
        assert_eq!(color, Color::Magenta);
    }

    #[test]
    fn work_item_icon_session_none() {
        let (icon, color) = work_item_icon(&WorkItemKind::Session, false, None);
        assert_eq!(icon, "○");
        assert_eq!(color, Color::Magenta);
    }

    #[test]
    fn work_item_icon_change_request() {
        let (icon, color) = work_item_icon(&WorkItemKind::ChangeRequest, false, None);
        assert_eq!(icon, "⊙");
        assert_eq!(color, Color::Blue);
    }

    #[test]
    fn work_item_icon_remote_branch() {
        let (icon, color) = work_item_icon(&WorkItemKind::RemoteBranch, false, None);
        assert_eq!(icon, "⊶");
        assert_eq!(color, Color::DarkGray);
    }

    #[test]
    fn work_item_icon_issue() {
        let (icon, color) = work_item_icon(&WorkItemKind::Issue, false, None);
        assert_eq!(icon, "◇");
        assert_eq!(color, Color::Yellow);
    }

    #[test]
    fn session_status_display_all() {
        assert_eq!(session_status_display(&SessionStatus::Running), "▶");
        assert_eq!(session_status_display(&SessionStatus::Idle), "◆");
        assert_eq!(session_status_display(&SessionStatus::Archived), "○");
    }

    #[test]
    fn change_request_status_icon_all() {
        assert_eq!(
            change_request_status_icon(&ChangeRequestStatus::Merged),
            "✓"
        );
        assert_eq!(
            change_request_status_icon(&ChangeRequestStatus::Closed),
            "✗"
        );
        assert_eq!(change_request_status_icon(&ChangeRequestStatus::Open), "");
        assert_eq!(change_request_status_icon(&ChangeRequestStatus::Draft), "");
    }

    #[test]
    fn git_status_display_empty() {
        let co = Checkout {
            branch: "main".into(),
            is_trunk: true,
            trunk_ahead_behind: None,
            remote_ahead_behind: None,
            working_tree: None,
            last_commit: None,
            correlation_keys: vec![],
            association_keys: vec![],
        };
        assert_eq!(git_status_display(&co), "");
    }

    #[test]
    fn git_status_display_all_flags() {
        let co = Checkout {
            branch: "feat".into(),
            is_trunk: false,
            trunk_ahead_behind: Some(AheadBehind {
                ahead: 3,
                behind: 0,
            }),
            remote_ahead_behind: None,
            working_tree: Some(WorkingTreeStatus {
                modified: 2,
                staged: 1,
                untracked: 4,
            }),
            last_commit: None,
            correlation_keys: vec![],
            association_keys: vec![],
        };
        assert_eq!(git_status_display(&co), "MS?↑");
    }

    #[test]
    fn git_status_display_partial() {
        let co = Checkout {
            branch: "fix".into(),
            is_trunk: false,
            trunk_ahead_behind: None,
            remote_ahead_behind: None,
            working_tree: Some(WorkingTreeStatus {
                modified: 1,
                staged: 0,
                untracked: 0,
            }),
            last_commit: None,
            correlation_keys: vec![],
            association_keys: vec![],
        };
        assert_eq!(git_status_display(&co), "M");
    }

    #[test]
    fn checkout_indicator_main() {
        assert_eq!(checkout_indicator(true, true), "◆");
        assert_eq!(checkout_indicator(true, false), "◆");
    }

    #[test]
    fn checkout_indicator_checked_out() {
        assert_eq!(checkout_indicator(false, true), "✓");
    }

    #[test]
    fn checkout_indicator_none() {
        assert_eq!(checkout_indicator(false, false), "");
    }

    #[test]
    fn workspace_indicator_values() {
        assert_eq!(workspace_indicator(0), "");
        assert_eq!(workspace_indicator(1), "●");
        assert_eq!(workspace_indicator(2), "2");
        assert_eq!(workspace_indicator(10), "10");
    }

    #[test]
    fn shorten_path_main_checkout() {
        let root = Path::new("/dev/project");
        assert_eq!(shorten_path(root, root, 40), "/dev/project");
    }

    #[test]
    fn shorten_path_main_checkout_under_home() {
        let home = dirs::home_dir().expect("home dir");
        let root = home.join("dev/flotilla");
        assert_eq!(shorten_path(&root, &root, 40), "~/dev/flotilla");
    }

    #[test]
    fn shorten_path_worktree_wide_column() {
        // Wide column — full padding (5 = len("/dev/")).
        let root = Path::new("/dev/project");
        let wt = Path::new("/dev/project/.worktrees/feat-auth");
        assert_eq!(shorten_path(wt, root, 40), "     .worktrees/feat-auth");
    }

    #[test]
    fn shorten_path_worktree_narrow_column() {
        // Narrow column — padding is consistent (capped at col/2), caller truncates.
        let root = Path::new("/dev/project");
        let wt = Path::new("/dev/project/.worktrees/feat-auth");
        // ideal_padding = 5, col/2 = 11 → padding = 5 (same as wide)
        assert_eq!(shorten_path(wt, root, 22), "     .worktrees/feat-auth");
    }

    #[test]
    fn shorten_path_worktree_very_narrow() {
        // Very narrow — padding capped at col/2 = 5, still consistent indent.
        let root = Path::new("/dev/project");
        let wt = Path::new("/dev/project/.worktrees/feat-auth");
        assert_eq!(shorten_path(wt, root, 10), "     .worktrees/feat-auth");
    }

    #[test]
    fn shorten_path_relative() {
        let root = Path::new("/dev/project");
        let sub = Path::new("/dev/project/sub/dir");
        assert_eq!(shorten_path(sub, root, 40), "     sub/dir");
    }

    #[test]
    fn shorten_path_nested_worktree() {
        let root = Path::new("/dev/project");
        let wt = Path::new("/dev/project/.worktrees/group/feat-auth");
        assert_eq!(
            shorten_path(wt, root, 40),
            "     .worktrees/group/feat-auth"
        );
    }

    #[test]
    fn shorten_path_sibling_worktree() {
        // padding = len("/dev/") = 5
        let root = Path::new("/dev/flotilla");
        let wt = Path::new("/dev/flotilla.feat-xyz");
        assert_eq!(shorten_path(wt, root, 40), "     .feat-xyz");
    }

    #[test]
    fn shorten_path_sibling_different_name() {
        let root = Path::new("/dev/flotilla");
        let wt = Path::new("/dev/other-project");
        assert_eq!(shorten_path(wt, root, 40), "     other-project");
    }

    #[test]
    fn shorten_path_sibling_under_home() {
        // padding = len("~/dev/") = 6
        let home = dirs::home_dir().expect("home dir");
        let root = home.join("dev/flotilla");
        let wt = home.join("dev/flotilla.low-hang-12");
        assert_eq!(shorten_path(&wt, &root, 40), "      .low-hang-12");
    }

    #[test]
    fn shorten_path_outside_root() {
        let root = Path::new("/tmp/project");
        let other = Path::new("/elsewhere/wt");
        assert_eq!(shorten_path(other, root, 40), "/elsewhere/wt");
    }
}
