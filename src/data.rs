use serde::Deserialize;
use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{LazyLock, Mutex};

#[derive(Debug, Clone, Deserialize)]
pub struct Worktree {
    pub branch: String,
    pub path: PathBuf,
    #[serde(default)]
    pub is_main: bool,
    #[serde(default)]
    #[allow(dead_code)]
    pub is_current: bool,
    #[allow(dead_code)]
    pub main_state: Option<String>,
    pub main: Option<AheadBehind>,
    pub remote: Option<RemoteStatus>,
    pub working_tree: Option<WorkingTree>,
    pub commit: Option<CommitInfo>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AheadBehind {
    pub ahead: i64,
    pub behind: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RemoteStatus {
    #[allow(dead_code)]
    pub name: Option<String>,
    #[allow(dead_code)]
    pub branch: Option<String>,
    pub ahead: i64,
    pub behind: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WorkingTree {
    #[serde(default)]
    pub staged: bool,
    #[serde(default)]
    pub modified: bool,
    #[serde(default)]
    pub untracked: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CommitInfo {
    pub short_sha: Option<String>,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WorkItemKind {
    Worktree,
    Session,
    Pr,
    RemoteBranch,
    Issue,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SectionHeader {
    Worktrees,
    Sessions,
    PullRequests,
    RemoteBranches,
    Issues,
}

impl fmt::Display for SectionHeader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SectionHeader::Worktrees => write!(f, "Worktrees"),
            SectionHeader::Sessions => write!(f, "Sessions"),
            SectionHeader::PullRequests => write!(f, "Pull Requests"),
            SectionHeader::RemoteBranches => write!(f, "Remote Branches"),
            SectionHeader::Issues => write!(f, "Issues"),
        }
    }
}

#[derive(Debug, Clone)]
pub enum TableEntry {
    Header(SectionHeader),
    Item(WorkItem),
}

#[derive(Debug, Clone)]
pub struct WorkItem {
    pub kind: WorkItemKind,
    pub branch: Option<String>,
    pub description: String,
    pub worktree_idx: Option<usize>,
    pub is_main_worktree: bool,
    pub pr_idx: Option<usize>,
    pub session_idx: Option<usize>,
    pub issue_idxs: Vec<usize>,
    pub workspace_refs: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct CmuxWorkspace {
    pub ws_ref: String,
    pub name: String,
    pub directories: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GithubPr {
    pub number: i64,
    pub title: String,
    #[serde(rename = "headRefName")]
    pub head_ref_name: String,
    pub state: String,
    #[serde(rename = "updatedAt")]
    #[allow(dead_code)]
    pub updated_at: Option<String>,
    #[serde(default)]
    pub body: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GithubIssue {
    pub number: i64,
    pub title: String,
    pub labels: Vec<Label>,
    #[serde(rename = "updatedAt")]
    #[allow(dead_code)]
    pub updated_at: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Label {
    pub name: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WebSession {
    pub id: String,
    pub title: String,
    pub session_status: String,
    #[serde(default)]
    #[allow(dead_code)]
    pub created_at: String,
    #[serde(default)]
    pub updated_at: String,  // used for sorting and display
    #[serde(default)]
    pub session_context: SessionContext,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct SessionContext {
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub outcomes: Vec<SessionOutcome>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SessionOutcome {
    #[serde(default)]
    pub git_info: Option<SessionGitInfo>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SessionGitInfo {
    #[serde(default)]
    pub branches: Vec<String>,
}

impl WebSession {
    pub fn branch(&self) -> Option<&str> {
        self.session_context
            .outcomes
            .first()
            .and_then(|o| o.git_info.as_ref())
            .and_then(|gi| gi.branches.first())
            .map(|s| s.as_str())
    }
}

#[derive(Debug, Default, Clone)]
pub struct DataStore {
    pub worktrees: Vec<Worktree>,
    pub prs: Vec<GithubPr>,
    pub issues: Vec<GithubIssue>,
    pub cmux_workspaces: Vec<CmuxWorkspace>,
    pub sessions: Vec<WebSession>,
    pub remote_branches: Vec<String>,
    pub merged_branches: Vec<String>,
    pub table_entries: Vec<TableEntry>,
    pub selectable_indices: Vec<usize>,
    pub loading: bool,
}

impl DataStore {
    pub async fn refresh(&mut self, repo_root: &PathBuf) -> Vec<String> {
        self.loading = true;
        let mut errors = Vec::new();
        let (wt, prs, issues, ws, sessions, branches, merged) = tokio::join!(
            fetch_worktrees(repo_root),
            fetch_prs(repo_root),
            fetch_issues(repo_root),
            fetch_cmux_workspaces(),
            fetch_sessions(),
            fetch_remote_branches(repo_root),
            fetch_merged_pr_branches(repo_root),
        );
        self.worktrees = wt.unwrap_or_else(|e| { errors.push(format!("worktrees: {e}")); Vec::new() });
        self.prs = prs.unwrap_or_else(|e| { errors.push(format!("PRs: {e}")); Vec::new() });
        self.issues = issues.unwrap_or_else(|e| { errors.push(format!("issues: {e}")); Vec::new() });
        self.cmux_workspaces = ws.unwrap_or_else(|e| { errors.push(format!("workspaces: {e}")); Vec::new() });
        self.sessions = sessions.unwrap_or_else(|e| { errors.push(format!("sessions: {e}")); Vec::new() });
        self.remote_branches = branches.unwrap_or_else(|e| { errors.push(format!("branches: {e}")); Vec::new() });
        self.merged_branches = merged.unwrap_or_else(|e| { errors.push(format!("merged: {e}")); Vec::new() });
        self.correlate();
        self.loading = false;
        errors
    }

    fn find_workspaces_for_worktree(&self, wt: &Worktree) -> Vec<String> {
        let wt_path = wt.path.to_string_lossy();
        self.cmux_workspaces.iter().filter(|ws| {
            ws.directories.iter().any(|dir| dir == wt_path.as_ref())
        }).map(|ws| ws.ws_ref.clone()).collect()
    }

    fn correlate(&mut self) {
        let mut items_by_branch: HashMap<String, WorkItem> = HashMap::new();
        let mut branchless_sessions: Vec<WorkItem> = Vec::new();

        // 1. Insert worktrees (primary items)
        for (i, wt) in self.worktrees.iter().enumerate() {
            let branch = wt.branch.clone();
            let ws_refs = self.find_workspaces_for_worktree(wt);
            let item = WorkItem {
                kind: WorkItemKind::Worktree,
                branch: Some(branch.clone()),
                description: branch.clone(),
                worktree_idx: Some(i),
                is_main_worktree: wt.is_main,
                pr_idx: None,
                session_idx: None,
                issue_idxs: Vec::new(),
                workspace_refs: ws_refs,
            };
            items_by_branch.insert(branch, item);
        }

        // 2. Augment with PRs (or create new items)
        for (i, pr) in self.prs.iter().enumerate() {
            let branch = pr.head_ref_name.clone();
            if let Some(item) = items_by_branch.get_mut(&branch) {
                item.pr_idx = Some(i);
            } else {
                let item = WorkItem {
                    kind: WorkItemKind::Pr,
                    branch: Some(branch.clone()),
                    description: pr.title.clone(),
                    worktree_idx: None,
                    is_main_worktree: false,
                    pr_idx: Some(i),
                    session_idx: None,
                    issue_idxs: Vec::new(),
                    workspace_refs: Vec::new(),
                };
                items_by_branch.insert(branch, item);
            }
        }

        // 3. Augment with sessions
        for (i, session) in self.sessions.iter().enumerate() {
            if let Some(branch) = session.branch() {
                // Strip refs/heads/ prefix if present, keep everything else
                let branch = branch
                    .strip_prefix("refs/heads/")
                    .or_else(|| branch.strip_prefix("refs/remotes/origin/"))
                    .unwrap_or(branch)
                    .to_string();
                if let Some(item) = items_by_branch.get_mut(&branch) {
                    item.session_idx = Some(i);
                } else {
                    let item = WorkItem {
                        kind: WorkItemKind::Session,
                        branch: Some(branch.clone()),
                        description: session.title.clone(),
                        worktree_idx: None,
                        is_main_worktree: false,
                        pr_idx: None,
                        session_idx: Some(i),
                        issue_idxs: Vec::new(),
                        workspace_refs: Vec::new(),
                    };
                    items_by_branch.insert(branch, item);
                }
            } else {
                branchless_sessions.push(WorkItem {
                    kind: WorkItemKind::Session,
                    branch: None,
                    description: session.title.clone(),
                    worktree_idx: None,
                    is_main_worktree: false,
                    pr_idx: None,
                    session_idx: Some(i),
                    issue_idxs: Vec::new(),
                    workspace_refs: Vec::new(),
                });
            }
        }

        // 4. Link issues to PRs by parsing "Fixes/Closes/Resolves #N" from PR titles and bodies
        let mut linked_issues: std::collections::HashSet<i64> = std::collections::HashSet::new();
        for pr in &self.prs {
            let texts = [pr.title.as_str(), pr.body.as_deref().unwrap_or("")];
            for text in texts {
                for keyword in ["fixes", "closes", "resolves"] {
                    let lower = text.to_lowercase();
                    let mut search_from = 0;
                    while let Some(pos) = lower[search_from..].find(keyword) {
                        let after = search_from + pos + keyword.len();
                        let rest = &text[after..];
                        let rest = rest.trim_start();
                        if let Some(rest) = rest.strip_prefix('#') {
                            let num_str: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
                            if let Ok(num) = num_str.parse::<i64>() {
                                linked_issues.insert(num);
                                // Find which branch this PR belongs to and link the issue
                                if let Some(item) = items_by_branch.get_mut(&pr.head_ref_name) {
                                    if let Some(issue_idx) = self.issues.iter().position(|i| i.number == num) {
                                        if !item.issue_idxs.contains(&issue_idx) {
                                            item.issue_idxs.push(issue_idx);
                                        }
                                    }
                                }
                            }
                        }
                        search_from = after;
                    }
                }
            }
        }

        // 5. Build table entries in section order
        let mut entries: Vec<TableEntry> = Vec::new();
        let mut selectable: Vec<usize> = Vec::new();

        // Worktrees section — sorted by branch name
        let mut wt_items: Vec<WorkItem> = items_by_branch.values()
            .filter(|item| item.kind == WorkItemKind::Worktree)
            .cloned()
            .collect();
        wt_items.sort_by(|a, b| a.branch.cmp(&b.branch));
        if !wt_items.is_empty() {
            entries.push(TableEntry::Header(SectionHeader::Worktrees));
            for item in wt_items {
                selectable.push(entries.len());
                entries.push(TableEntry::Item(item));
            }
        }

        // Sessions section — sorted by updated_at descending (most recent first)
        let mut session_items: Vec<WorkItem> = items_by_branch.values()
            .filter(|item| item.kind == WorkItemKind::Session)
            .cloned()
            .chain(branchless_sessions)
            .collect();
        session_items.sort_by(|a, b| {
            let a_time = a.session_idx.and_then(|i| self.sessions.get(i)).map(|s| s.updated_at.as_str());
            let b_time = b.session_idx.and_then(|i| self.sessions.get(i)).map(|s| s.updated_at.as_str());
            b_time.cmp(&a_time) // descending
        });
        if !session_items.is_empty() {
            entries.push(TableEntry::Header(SectionHeader::Sessions));
            for item in session_items {
                selectable.push(entries.len());
                entries.push(TableEntry::Item(item));
            }
        }

        // PRs section — sorted by PR number descending (most recent first)
        let mut pr_items: Vec<WorkItem> = items_by_branch.values()
            .filter(|item| item.kind == WorkItemKind::Pr)
            .cloned()
            .collect();
        pr_items.sort_by(|a, b| {
            let a_num = a.pr_idx.and_then(|i| self.prs.get(i)).map(|p| p.number);
            let b_num = b.pr_idx.and_then(|i| self.prs.get(i)).map(|p| p.number);
            b_num.cmp(&a_num) // descending
        });
        if !pr_items.is_empty() {
            entries.push(TableEntry::Header(SectionHeader::PullRequests));
            for item in pr_items {
                selectable.push(entries.len());
                entries.push(TableEntry::Item(item));
            }
        }

        // Remote branches section — sorted by branch name
        let known_branches: std::collections::HashSet<&str> = items_by_branch.keys()
            .map(|s| s.as_str())
            .collect();
        let merged_set: std::collections::HashSet<&str> = self.merged_branches.iter()
            .map(|s| s.as_str())
            .collect();
        let mut remote_items: Vec<WorkItem> = self.remote_branches.iter()
            .filter(|b| {
                b.as_str() != "HEAD" && b.as_str() != "main" && b.as_str() != "master"
                    && !known_branches.contains(b.as_str())
                    && !merged_set.contains(b.as_str())
            })
            .map(|b| {
                WorkItem {
                    kind: WorkItemKind::RemoteBranch,
                    branch: Some(b.clone()),
                    description: b.clone(),
                    worktree_idx: None,
                    is_main_worktree: false,
                    pr_idx: None,
                    session_idx: None,
                    issue_idxs: Vec::new(),
                    workspace_refs: Vec::new(),
                }
            })
            .collect();
        remote_items.sort_by(|a, b| a.branch.cmp(&b.branch));
        if !remote_items.is_empty() {
            entries.push(TableEntry::Header(SectionHeader::RemoteBranches));
            for item in remote_items {
                selectable.push(entries.len());
                entries.push(TableEntry::Item(item));
            }
        }

        // Issues section — sorted by issue number descending (most recent first)
        let mut issue_items: Vec<WorkItem> = self.issues.iter()
            .enumerate()
            .filter(|(_, issue)| !linked_issues.contains(&issue.number))
            .map(|(i, issue)| WorkItem {
                kind: WorkItemKind::Issue,
                branch: None,
                description: issue.title.clone(),
                worktree_idx: None,
                is_main_worktree: false,
                pr_idx: None,
                session_idx: None,
                issue_idxs: vec![i],
                workspace_refs: Vec::new(),
            })
            .collect();
        issue_items.sort_by(|a, b| {
            let a_num = a.issue_idxs.first().and_then(|&i| self.issues.get(i)).map(|iss| iss.number);
            let b_num = b.issue_idxs.first().and_then(|&i| self.issues.get(i)).map(|iss| iss.number);
            b_num.cmp(&a_num) // descending
        });
        if !issue_items.is_empty() {
            entries.push(TableEntry::Header(SectionHeader::Issues));
            for item in issue_items {
                selectable.push(entries.len());
                entries.push(TableEntry::Item(item));
            }
        }

        self.table_entries = entries;
        self.selectable_indices = selectable;
    }
}

async fn run_command(cmd: &str, args: &[&str], cwd: Option<&PathBuf>) -> Result<String, String> {
    let mut command = tokio::process::Command::new(cmd);
    command.args(args);
    if let Some(dir) = cwd {
        command.current_dir(dir);
    }
    let output = command.output().await.map_err(|e| e.to_string())?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).to_string())
    }
}

async fn fetch_worktrees(repo_root: &PathBuf) -> Result<Vec<Worktree>, String> {
    let output = run_command("wt", &["list", "--format=json"], Some(repo_root)).await?;
    // wt may append ANSI escape codes after the JSON; strip them
    let json_end = output.rfind(']').map(|i| i + 1).unwrap_or(output.len());
    serde_json::from_str(&output[..json_end]).map_err(|e| e.to_string())
}

async fn fetch_prs(repo_root: &PathBuf) -> Result<Vec<GithubPr>, String> {
    let output = run_command(
        "gh",
        &["pr", "list", "--json", "number,title,headRefName,state,updatedAt,body", "--limit", "20"],
        Some(repo_root),
    ).await?;
    serde_json::from_str(&output).map_err(|e| e.to_string())
}

async fn fetch_remote_branches(repo_root: &PathBuf) -> Result<Vec<String>, String> {
    // Read directly from remote — no local state modification
    let output = run_command(
        "git",
        &["ls-remote", "--heads", "origin"],
        Some(repo_root),
    ).await?;
    // Output format: "<sha>\trefs/heads/<branch>"
    Ok(output
        .lines()
        .filter_map(|line| {
            line.split('\t')
                .nth(1)
                .and_then(|r| r.strip_prefix("refs/heads/"))
                .map(|s| s.to_string())
        })
        .collect())
}

async fn fetch_merged_pr_branches(repo_root: &PathBuf) -> Result<Vec<String>, String> {
    let output = run_command(
        "gh",
        &["pr", "list", "--state", "merged", "--limit", "50", "--json", "headRefName"],
        Some(repo_root),
    ).await?;
    let prs: Vec<serde_json::Value> = serde_json::from_str(&output).map_err(|e| e.to_string())?;
    Ok(prs
        .iter()
        .filter_map(|p| p.get("headRefName").and_then(|v| v.as_str()).map(|s| s.to_string()))
        .collect())
}

async fn fetch_issues(repo_root: &PathBuf) -> Result<Vec<GithubIssue>, String> {
    let output = run_command(
        "gh",
        &["issue", "list", "--json", "number,title,labels,updatedAt", "--limit", "20", "--state", "open"],
        Some(repo_root),
    ).await?;
    serde_json::from_str(&output).map_err(|e| e.to_string())
}

#[derive(Deserialize)]
struct OAuthCredentials {
    #[serde(rename = "claudeAiOauth")]
    claude_ai_oauth: OAuthToken,
}

#[derive(Deserialize, Clone)]
struct OAuthToken {
    #[serde(rename = "accessToken")]
    access_token: String,
    #[serde(rename = "expiresAt")]
    expires_at: i64,
}

struct AuthCache {
    token: Option<OAuthToken>,
    org_uuid: Option<String>,
}

static AUTH_CACHE: LazyLock<Mutex<AuthCache>> = LazyLock::new(|| {
    Mutex::new(AuthCache {
        token: None,
        org_uuid: None,
    })
});

#[derive(Deserialize)]
struct SessionsResponse {
    data: Vec<WebSession>,
}

async fn read_oauth_token_from_keychain() -> Result<OAuthToken, String> {
    let output = tokio::process::Command::new("security")
        .args(["find-generic-password", "-s", "Claude Code-credentials", "-w"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .await
        .map_err(|e| e.to_string())?;

    if !output.status.success() {
        return Err("No Claude Code credentials in keychain".to_string());
    }

    let json = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let creds: OAuthCredentials = serde_json::from_str(&json).map_err(|e| e.to_string())?;
    Ok(creds.claude_ai_oauth)
}

async fn get_oauth_token() -> Result<OAuthToken, String> {
    {
        let cache = AUTH_CACHE.lock().unwrap();
        if let Some(ref token) = cache.token {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64;
            if token.expires_at > now + 60 {
                return Ok(token.clone());
            }
        }
    }
    // Token missing or expiring soon — re-read from keychain
    let token = read_oauth_token_from_keychain().await?;
    let mut cache = AUTH_CACHE.lock().unwrap();
    cache.token = Some(token.clone());
    cache.org_uuid = None; // new token might mean different user
    Ok(token)
}

async fn get_org_uuid(token: &str) -> Result<String, String> {
    {
        let cache = AUTH_CACHE.lock().unwrap();
        if let Some(ref uuid) = cache.org_uuid {
            return Ok(uuid.clone());
        }
    }
    let uuid = read_org_uuid(token).await?;
    let mut cache = AUTH_CACHE.lock().unwrap();
    cache.org_uuid = Some(uuid.clone());
    Ok(uuid)
}

async fn read_org_uuid(token: &str) -> Result<String, String> {
    let output = tokio::process::Command::new("curl")
        .args([
            "-s",
            "-H", &format!("Authorization: Bearer {token}"),
            "-H", "anthropic-version: 2023-06-01",
            "https://api.anthropic.com/api/oauth/profile",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .await
        .map_err(|e| e.to_string())?;

    let body = String::from_utf8_lossy(&output.stdout).to_string();
    let v: serde_json::Value = serde_json::from_str(&body).map_err(|e| e.to_string())?;
    v.get("organization")
        .and_then(|o| o.get("uuid"))
        .and_then(|u| u.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| "No organization.uuid in profile".to_string())
}

async fn fetch_sessions() -> Result<Vec<WebSession>, String> {
    let token = get_oauth_token().await?;
    let org_uuid = get_org_uuid(&token.access_token).await?;
    let token = token.access_token;

    let output = tokio::process::Command::new("curl")
        .args([
            "-s",
            "-H", &format!("Authorization: Bearer {token}"),
            "-H", "anthropic-beta: ccr-byoc-2025-07-29",
            "-H", "anthropic-version: 2023-06-01",
            "-H", &format!("x-organization-uuid: {org_uuid}"),
            "https://api.anthropic.com/v1/sessions",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .await
        .map_err(|e| e.to_string())?;

    let body = String::from_utf8_lossy(&output.stdout).to_string();
    let resp: SessionsResponse = serde_json::from_str(&body).map_err(|e| e.to_string())?;

    // Filter to non-archived sessions, sorted by updated_at descending
    let mut sessions: Vec<WebSession> = resp
        .data
        .into_iter()
        .filter(|s| s.session_status != "archived")
        .collect();
    sessions.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    Ok(sessions)
}

pub async fn archive_session(session_id: &str) -> Result<(), String> {
    let token = get_oauth_token().await?;
    let org_uuid = get_org_uuid(&token.access_token).await?;
    let token = token.access_token;

    let url = format!("https://api.anthropic.com/v1/sessions/{session_id}");
    let output = tokio::process::Command::new("curl")
        .args([
            "-s",
            "-w", "\n%{http_code}",
            "-X", "PATCH",
            "-H", &format!("Authorization: Bearer {token}"),
            "-H", "anthropic-beta: ccr-byoc-2025-07-29",
            "-H", "anthropic-version: 2023-06-01",
            "-H", &format!("x-organization-uuid: {org_uuid}"),
            "-H", "content-type: application/json",
            "-d", r#"{"session_status":"archived"}"#,
            &url,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .await
        .map_err(|e| e.to_string())?;

    let body = String::from_utf8_lossy(&output.stdout).to_string();
    let status_code: u16 = body
        .lines()
        .last()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0);

    if status_code >= 200 && status_code < 300 {
        Ok(())
    } else {
        Err(format!("archive session failed (HTTP {}): {}", status_code, body.lines().next().unwrap_or("")))
    }
}

#[derive(Debug, Clone, Default)]
pub struct DeleteConfirmInfo {
    pub branch: String,
    pub pr_status: Option<String>,
    pub merge_commit_sha: Option<String>,
    pub unpushed_commits: Vec<String>,
    pub has_uncommitted: bool,
}

pub async fn fetch_delete_confirm_info(
    branch: &str,
    worktree_path: Option<&PathBuf>,
    pr_number: Option<i64>,
    repo_root: &PathBuf,
) -> DeleteConfirmInfo {
    let branch_owned = branch.to_string();
    let repo = repo_root.clone();
    let wt_path = worktree_path.cloned();
    let pr_num = pr_number;
    let repo2 = repo_root.clone();

    let (unpushed, uncommitted, pr_info) = tokio::join!(
        async {
            run_command(
                "git",
                &["log", &format!("origin/main..{}", branch_owned), "--oneline"],
                Some(&repo),
            ).await.unwrap_or_default()
        },
        async {
            if let Some(path) = &wt_path {
                run_command("git", &["status", "--porcelain"], Some(path))
                    .await.unwrap_or_default()
            } else {
                String::new()
            }
        },
        async {
            if let Some(num) = pr_num {
                run_command(
                    "gh",
                    &["pr", "view", &num.to_string(), "--json", "state,mergeCommit"],
                    Some(&repo2),
                ).await.ok()
            } else {
                None
            }
        },
    );

    let mut info = DeleteConfirmInfo {
        branch: branch.to_string(),
        ..Default::default()
    };

    info.unpushed_commits = unpushed
        .lines()
        .map(|l| l.to_string())
        .filter(|l| !l.is_empty())
        .collect();

    info.has_uncommitted = !uncommitted.trim().is_empty();

    if let Some(pr_json) = pr_info {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&pr_json) {
            info.pr_status = v.get("state").and_then(|s| s.as_str()).map(|s| s.to_string());
            info.merge_commit_sha = v.get("mergeCommit")
                .and_then(|mc| mc.get("oid"))
                .and_then(|s| s.as_str())
                .map(|s| s[..7.min(s.len())].to_string());
        }
    }

    info
}

async fn fetch_cmux_workspaces() -> Result<Vec<CmuxWorkspace>, String> {
    let output = run_command(
        "/Applications/cmux.app/Contents/Resources/bin/cmux",
        &["--json", "list-workspaces"],
        None,
    ).await?;
    let parsed: serde_json::Value = serde_json::from_str(&output).map_err(|e| e.to_string())?;
    let workspaces = parsed["workspaces"].as_array().ok_or("no workspaces array")?;
    Ok(workspaces
        .iter()
        .filter_map(|ws| {
            let ws_ref = ws["ref"].as_str()?.to_string();
            let name = ws["title"].as_str().unwrap_or("").to_string();
            let directories = ws["directories"]
                .as_array()
                .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                .unwrap_or_default();
            Some(CmuxWorkspace { ws_ref, name, directories })
        })
        .collect())
}
