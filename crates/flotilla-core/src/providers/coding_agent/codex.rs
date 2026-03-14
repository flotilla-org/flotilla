use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Instant,
};

use async_trait::async_trait;
use serde::Deserialize;
use tracing::{debug, warn};

use crate::providers::{http_execute, types::*, HttpClient};

// --- Auth ---

#[derive(Debug, Deserialize)]
struct CodexAuthFile {
    auth_mode: String,
    tokens: Option<CodexTokens>,
    #[serde(rename = "OPENAI_API_KEY")]
    openai_api_key: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CodexTokens {
    access_token: String,
    #[allow(dead_code)]
    account_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CodexAuth {
    pub bearer_token: String,
    pub account_id: Option<String>,
}

fn codex_home() -> PathBuf {
    if let Ok(val) = std::env::var("CODEX_HOME") {
        PathBuf::from(val)
    } else {
        dirs::home_dir().expect("could not determine home directory").join(".codex")
    }
}

fn parse_auth_file(contents: &str) -> Option<CodexAuth> {
    let file: CodexAuthFile = serde_json::from_str(contents).ok()?;
    match file.auth_mode.as_str() {
        "chatgpt" => {
            let tokens = file.tokens?;
            if tokens.access_token.is_empty() {
                return None;
            }
            Some(CodexAuth { bearer_token: tokens.access_token, account_id: tokens.account_id })
        }
        "api-key" => {
            let key = file.openai_api_key?;
            if key.is_empty() {
                return None;
            }
            Some(CodexAuth { bearer_token: key, account_id: None })
        }
        _ => None,
    }
}

fn read_auth() -> Option<CodexAuth> {
    let path = codex_home().join("auth.json");
    let contents = std::fs::read_to_string(path).ok()?;
    parse_auth_file(&contents)
}

pub fn codex_auth_file_exists() -> bool {
    codex_home().join("auth.json").exists()
}

// --- API response types ---

#[derive(Debug, Deserialize)]
pub struct EnvironmentInfo {
    pub id: String,
    #[serde(default)]
    pub label: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct TaskListResponse {
    #[serde(default)]
    pub items: Vec<TaskItem>,
    #[serde(default)]
    pub cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct TaskItem {
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub updated_at: Option<f64>,
    #[serde(default)]
    pub task_status_display: Option<TaskStatusDisplay>,
    #[serde(default)]
    pub pull_requests: Option<Vec<TaskPullRequestEntry>>,
}

#[derive(Debug, Deserialize)]
pub struct TaskStatusDisplay {
    #[serde(default)]
    pub environment_label: Option<String>,
    #[serde(default)]
    pub branch_name: Option<String>,
    #[serde(default)]
    pub latest_turn_status_display: Option<LatestTurnStatus>,
}

#[derive(Debug, Deserialize)]
pub struct LatestTurnStatus {
    #[serde(default)]
    pub turn_status: Option<String>,
}

/// Wrapper for the outer pull_requests array items, which contain a nested
/// `pull_request` object with the actual PR fields.
#[derive(Debug, Deserialize)]
pub struct TaskPullRequestEntry {
    #[serde(default)]
    pub pull_request: Option<PullRequestDetail>,
}

#[derive(Debug, Deserialize)]
pub struct PullRequestDetail {
    #[serde(default)]
    pub number: Option<u64>,
    #[serde(default)]
    pub head: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
}

// --- Task-to-session mapping ---

fn is_trunk_branch(name: &str) -> bool {
    matches!(name, "main" | "master")
}

fn epoch_to_rfc3339(epoch: f64) -> Option<String> {
    use chrono::{TimeZone, Utc};
    let secs = epoch as i64;
    let nanos = ((epoch - secs as f64) * 1_000_000_000.0).clamp(0.0, 999_999_999.0) as u32;
    Utc.timestamp_opt(secs, nanos).single().map(|dt| dt.to_rfc3339())
}

fn map_task_to_session(task: &TaskItem, provider_name: &str) -> (String, CloudAgentSession) {
    // Determine status from latest_turn_status_display
    let status = task
        .task_status_display
        .as_ref()
        .and_then(|d| d.latest_turn_status_display.as_ref())
        .and_then(|l| l.turn_status.as_deref())
        .map(|s| match s {
            "pending" | "in_progress" => SessionStatus::Running,
            _ => SessionStatus::Idle,
        })
        .unwrap_or(SessionStatus::Idle);

    let mut correlation_keys = vec![CorrelationKey::SessionRef(provider_name.to_string(), task.id.clone())];

    // Extract actual PR details from the nested structure
    let pr_details: Vec<&PullRequestDetail> =
        task.pull_requests.as_deref().unwrap_or_default().iter().filter_map(|entry| entry.pull_request.as_ref()).collect();

    if !pr_details.is_empty() {
        for pr in &pr_details {
            if let Some(ref head) = pr.head {
                if !head.is_empty() {
                    correlation_keys.push(CorrelationKey::Branch(head.clone()));
                }
            }
            if let Some(number) = pr.number {
                correlation_keys.push(CorrelationKey::ChangeRequestRef("github".to_string(), number.to_string()));
            }
        }
    } else {
        // No usable PR — use source branch if it's not trunk
        if let Some(ref display) = task.task_status_display {
            if let Some(ref branch) = display.branch_name {
                if !branch.is_empty() && !is_trunk_branch(branch) {
                    correlation_keys.push(CorrelationKey::Branch(branch.clone()));
                }
            }
        }
    }

    let title = if task.title.is_empty() { task.id.clone() } else { task.title.clone() };

    let updated_at = task.updated_at.and_then(epoch_to_rfc3339);

    (
        task.id.clone(),
        CloudAgentSession {
            title,
            status,
            model: None,
            updated_at,
            correlation_keys,
            provider_name: provider_name.to_string(),
            provider_display_name: "Codex".into(),
            item_noun: "Task".into(),
        },
    )
}

// --- CodexCodingAgent struct and HTTP helpers ---

const BASE_URL: &str = "https://chatgpt.com/backend-api";
const AUTH_CACHE_TTL_SECS: u64 = 300; // 5 minutes
const ENV_CACHE_TTL_SECS: u64 = 600; // 10 minutes

struct AuthCache {
    auth: Option<CodexAuth>,
    loaded_at: Option<Instant>,
}

struct EnvCache {
    environment_ids: Vec<String>,
    loaded_at: Option<Instant>,
}

pub struct CodexCodingAgent {
    provider_name: String,
    http: Arc<dyn HttpClient>,
    auth_cache: Mutex<AuthCache>,
    env_cache: Mutex<EnvCache>,
    auth_warned: AtomicBool,
}

impl CodexCodingAgent {
    pub fn new(provider_name: String, http: Arc<dyn HttpClient>) -> Self {
        Self {
            provider_name,
            http,
            auth_cache: Mutex::new(AuthCache { auth: None, loaded_at: None }),
            env_cache: Mutex::new(EnvCache { environment_ids: Vec::new(), loaded_at: None }),
            auth_warned: AtomicBool::new(false),
        }
    }

    fn get_cached_auth(&self) -> Option<CodexAuth> {
        let cache = self.auth_cache.lock().expect("auth_cache lock poisoned");
        if let (Some(auth), Some(loaded_at)) = (&cache.auth, cache.loaded_at) {
            if loaded_at.elapsed().as_secs() < AUTH_CACHE_TTL_SECS {
                return Some(auth.clone());
            }
        }
        None
    }

    fn refresh_auth(&self) -> Option<CodexAuth> {
        let auth = read_auth();
        let mut cache = self.auth_cache.lock().expect("auth_cache lock poisoned");
        cache.auth = auth.clone();
        cache.loaded_at = Some(Instant::now());
        auth
    }

    fn build_request(&self, method: &str, url: &str, auth: &CodexAuth) -> Result<reqwest::Request, String> {
        let method = reqwest::Method::from_bytes(method.as_bytes()).map_err(|e| format!("invalid HTTP method: {e}"))?;
        let mut builder = super::REQUEST_FACTORY
            .request(method, url)
            .header("authorization", format!("Bearer {}", auth.bearer_token))
            .header("user-agent", "flotilla");
        if let Some(ref account_id) = auth.account_id {
            builder = builder.header("chatgpt-account-id", account_id);
        }
        builder.build().map_err(|e| format!("request build error: {e}"))
    }

    async fn fetch_environment_ids(&self, repo_slug: &str, auth: &CodexAuth) -> Result<Vec<String>, String> {
        let (owner, repo) = repo_slug.split_once('/').ok_or_else(|| format!("invalid repo slug: {repo_slug}"))?;
        let url = format!("{BASE_URL}/wham/environments/by-repo/github/{owner}/{repo}");
        let request = self.build_request("GET", &url, auth)?;
        let resp = http_execute!(self.http, request)?;
        let status = resp.status().as_u16();
        if status == 401 || status == 403 {
            return Err(format!("authentication error (HTTP {status})"));
        }
        if !resp.status().is_success() {
            let body = String::from_utf8_lossy(resp.body()).to_string();
            return Err(format!("environment lookup failed (HTTP {status}): {body}"));
        }
        let envs: Vec<EnvironmentInfo> = serde_json::from_slice(resp.body()).map_err(|e| format!("environment list parse error: {e}"))?;
        Ok(envs.into_iter().map(|e| e.id).collect())
    }

    async fn fetch_tasks_page(&self, base_query: &str, cursor: Option<&str>, auth: &CodexAuth) -> Result<TaskListResponse, String> {
        let mut url = format!("{BASE_URL}/wham/tasks/list?task_filter=current&limit=20");
        if !base_query.is_empty() {
            url.push('&');
            url.push_str(base_query);
        }
        if let Some(c) = cursor {
            url.push_str("&cursor=");
            url.push_str(&urlencoding::encode(c));
        }
        let request = self.build_request("GET", &url, auth)?;
        let resp = http_execute!(self.http, request)?;
        let status = resp.status().as_u16();
        if status == 401 || status == 403 {
            return Err(format!("authentication error (HTTP {status})"));
        }
        if !resp.status().is_success() {
            let body = String::from_utf8_lossy(resp.body()).to_string();
            return Err(format!("task list failed (HTTP {status}): {body}"));
        }
        serde_json::from_slice(resp.body()).map_err(|e| format!("task list parse error: {e}"))
    }

    async fn fetch_all_tasks(&self, base_query: &str, auth: &CodexAuth) -> Result<Vec<TaskItem>, String> {
        let mut all_items = Vec::new();
        let mut cursor: Option<String> = None;

        for _ in 0..10 {
            let page = self.fetch_tasks_page(base_query, cursor.as_deref(), auth).await?;
            all_items.extend(page.items);
            cursor = page.cursor;
            if cursor.is_none() {
                break;
            }
        }

        Ok(all_items)
    }

    async fn fetch_tasks_by_label(&self, repo_slug: &str, auth: &CodexAuth) -> Result<Vec<(String, CloudAgentSession)>, String> {
        let repo_name = repo_slug.rsplit_once('/').map(|(_, name)| name).unwrap_or(repo_slug);

        let tasks = match self.fetch_all_tasks("", auth).await {
            Ok(tasks) => tasks,
            Err(e) => {
                debug!(provider = "codex", error = %e, "failed to fetch tasks by label");
                return Ok(vec![]);
            }
        };

        let sessions: Vec<(String, CloudAgentSession)> = tasks
            .iter()
            .filter(|t| {
                t.task_status_display
                    .as_ref()
                    .and_then(|d| d.environment_label.as_deref())
                    .is_some_and(|label| label.eq_ignore_ascii_case(repo_name))
            })
            .map(|t| map_task_to_session(t, &self.provider_name))
            .collect();

        Ok(sessions)
    }

    async fn fetch_tasks(&self, environment_id: &str, auth: &CodexAuth) -> Result<Vec<TaskItem>, String> {
        let query = format!("environment_id={}", urlencoding::encode(environment_id));
        self.fetch_all_tasks(&query, auth).await
    }
}

// --- CloudAgentService trait implementation ---

fn is_auth_error(e: &str) -> bool {
    e.contains("authentication error")
}

#[async_trait]
impl super::CloudAgentService for CodexCodingAgent {
    async fn list_sessions(&self, criteria: &RepoCriteria) -> Result<Vec<(String, CloudAgentSession)>, String> {
        let Some(ref repo_slug) = criteria.repo_slug else {
            return Ok(vec![]);
        };

        // Obtain auth: try cache first, then refresh from disk.
        // Mutable so we can update it after an env-lookup auth retry.
        let mut auth = match self.get_cached_auth() {
            Some(a) => a,
            None => match self.refresh_auth() {
                Some(a) => a,
                None => {
                    if !self.auth_warned.swap(true, Ordering::Relaxed) {
                        warn!(provider = "codex", "Codex sessions unavailable: no auth found in ~/.codex/auth.json");
                    }
                    return Ok(vec![]);
                }
            },
        };

        // Resolve environment IDs: check cache, then fetch
        let env_ids = {
            let cache = self.env_cache.lock().expect("env_cache lock poisoned");
            if let Some(loaded_at) = cache.loaded_at {
                if loaded_at.elapsed().as_secs() < ENV_CACHE_TTL_SECS {
                    Some(cache.environment_ids.clone())
                } else {
                    None
                }
            } else {
                None
            }
        };

        let env_ids = match env_ids {
            Some(ids) => ids,
            None => {
                match self.fetch_environment_ids(repo_slug, &auth).await {
                    Ok(ids) => {
                        let mut cache = self.env_cache.lock().expect("env_cache lock poisoned");
                        cache.environment_ids = ids.clone();
                        cache.loaded_at = Some(Instant::now());
                        ids
                    }
                    Err(e) if is_auth_error(&e) => {
                        // Retry with refreshed auth
                        let fresh_auth = match self.refresh_auth() {
                            Some(a) => a,
                            None => {
                                if !self.auth_warned.swap(true, Ordering::Relaxed) {
                                    warn!(provider = "codex", "Codex sessions unavailable: auth refresh failed");
                                }
                                return Ok(vec![]);
                            }
                        };
                        match self.fetch_environment_ids(repo_slug, &fresh_auth).await {
                            Ok(ids) => {
                                let mut cache = self.env_cache.lock().expect("env_cache lock poisoned");
                                cache.environment_ids = ids.clone();
                                cache.loaded_at = Some(Instant::now());
                                auth = fresh_auth;
                                ids
                            }
                            Err(e2) => {
                                if !self.auth_warned.swap(true, Ordering::Relaxed) {
                                    warn!(provider = "codex", error = %e2, "Codex sessions unavailable after auth retry");
                                }
                                return Ok(vec![]);
                            }
                        }
                    }
                    Err(e) => {
                        debug!(provider = "codex", error = %e, "environment lookup failed, falling back to label match");
                        return self.fetch_tasks_by_label(repo_slug, &auth).await;
                    }
                }
            }
        };

        if env_ids.is_empty() {
            return self.fetch_tasks_by_label(repo_slug, &auth).await;
        }

        let mut all_sessions = Vec::new();
        for env_id in &env_ids {
            match self.fetch_tasks(env_id, &auth).await {
                Ok(tasks) => {
                    for task in &tasks {
                        all_sessions.push(map_task_to_session(task, &self.provider_name));
                    }
                }
                Err(e) if is_auth_error(&e) => {
                    // Retry once with fresh auth
                    let fresh_auth = match self.refresh_auth() {
                        Some(a) => a,
                        None => {
                            if !self.auth_warned.swap(true, Ordering::Relaxed) {
                                warn!(provider = "codex", "Codex task fetch failed: auth refresh failed");
                            }
                            return Ok(all_sessions);
                        }
                    };
                    match self.fetch_tasks(env_id, &fresh_auth).await {
                        Ok(tasks) => {
                            for task in &tasks {
                                all_sessions.push(map_task_to_session(task, &self.provider_name));
                            }
                        }
                        Err(e2) => {
                            debug!(provider = "codex", env_id, error = %e2, "task fetch failed after auth retry");
                        }
                    }
                }
                Err(e) => {
                    debug!(provider = "codex", env_id, error = %e, "task fetch failed");
                }
            }
        }

        Ok(all_sessions)
    }

    async fn archive_session(&self, _session_id: &str) -> Result<(), String> {
        Err("archiving Codex tasks is not supported".to_string())
    }

    async fn attach_command(&self, session_id: &str) -> Result<String, String> {
        Ok(format!("open https://chatgpt.com/codex/tasks/{session_id}"))
    }
}

// --- Tests ---

/// Lock shared across test modules that manipulate the `CODEX_HOME` env var.
#[cfg(test)]
pub(crate) static CODEX_TEST_LOCK: std::sync::LazyLock<tokio::sync::Mutex<()>> = std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{coding_agent::CloudAgentService, replay};

    fn fixture(name: &str) -> String {
        crate::providers::testing::fixture_path("coding_agent", name)
    }

    // Auth parsing tests

    #[test]
    fn parse_auth_chatgpt_mode() {
        let json = r#"{
            "auth_mode": "chatgpt",
            "tokens": {
                "access_token": "tok-abc123",
                "account_id": "acct-456"
            }
        }"#;
        let auth = parse_auth_file(json).expect("should parse chatgpt auth");
        assert_eq!(auth.bearer_token, "tok-abc123");
        assert_eq!(auth.account_id.as_deref(), Some("acct-456"));
    }

    #[test]
    fn parse_auth_api_key_mode() {
        let json = r#"{
            "auth_mode": "api-key",
            "OPENAI_API_KEY": "sk-test-key"
        }"#;
        let auth = parse_auth_file(json).expect("should parse api-key auth");
        assert_eq!(auth.bearer_token, "sk-test-key");
        assert!(auth.account_id.is_none());
    }

    #[test]
    fn parse_auth_unknown_mode_returns_none() {
        let json = r#"{"auth_mode": "oauth2"}"#;
        assert!(parse_auth_file(json).is_none());
    }

    #[test]
    fn parse_auth_malformed_json_returns_none() {
        assert!(parse_auth_file("not json at all").is_none());
    }

    #[test]
    fn parse_auth_chatgpt_missing_tokens_returns_none() {
        let json = r#"{"auth_mode": "chatgpt"}"#;
        assert!(parse_auth_file(json).is_none());
    }

    #[test]
    fn parse_auth_chatgpt_empty_token_returns_none() {
        let json = r#"{"auth_mode": "chatgpt", "tokens": {"access_token": ""}}"#;
        assert!(parse_auth_file(json).is_none());
    }

    #[test]
    fn parse_auth_api_key_empty_key_returns_none() {
        let json = r#"{"auth_mode": "api-key", "OPENAI_API_KEY": ""}"#;
        assert!(parse_auth_file(json).is_none());
    }

    // Deserialization tests

    #[test]
    fn deserialize_task_list_response() {
        let json = r#"{
            "items": [
                {
                    "id": "task-1",
                    "title": "Fix the bug",
                    "updated_at": 1710000000.5,
                    "task_status_display": {
                        "environment_label": "env-1",
                        "branch_name": "fix/bug",
                        "latest_turn_status_display": {
                            "turn_status": "in_progress"
                        }
                    },
                    "pull_requests": [
                        {
                            "id": "github-123-42",
                            "pull_request": {
                                "number": 42,
                                "head": "fix/bug",
                                "url": "https://github.com/owner/repo/pull/42"
                            }
                        }
                    ]
                }
            ],
            "cursor": "next-page-token"
        }"#;
        let resp: TaskListResponse = serde_json::from_str(json).expect("should deserialize");
        assert_eq!(resp.items.len(), 1);
        assert_eq!(resp.cursor.as_deref(), Some("next-page-token"));
        let task = &resp.items[0];
        assert_eq!(task.id, "task-1");
        assert_eq!(task.title, "Fix the bug");
        assert_eq!(task.updated_at, Some(1710000000.5));
        let display = task.task_status_display.as_ref().expect("has display");
        assert_eq!(display.branch_name.as_deref(), Some("fix/bug"));
        let turn = display.latest_turn_status_display.as_ref().expect("has turn");
        assert_eq!(turn.turn_status.as_deref(), Some("in_progress"));
        let prs = task.pull_requests.as_ref().expect("has PRs");
        let pr = prs[0].pull_request.as_ref().expect("has PR detail");
        assert_eq!(pr.number, Some(42));
        assert_eq!(pr.head.as_deref(), Some("fix/bug"));
    }

    #[test]
    fn deserialize_task_list_response_minimal() {
        let json = r#"{"items": [{"id": "task-2"}]}"#;
        let resp: TaskListResponse = serde_json::from_str(json).expect("should deserialize");
        assert_eq!(resp.items.len(), 1);
        assert_eq!(resp.items[0].id, "task-2");
        assert!(resp.items[0].title.is_empty());
        assert!(resp.items[0].updated_at.is_none());
        assert!(resp.items[0].task_status_display.is_none());
        assert!(resp.items[0].pull_requests.is_none());
        assert!(resp.cursor.is_none());
    }

    #[test]
    fn deserialize_environment_list() {
        let json = r#"[
            {"id": "env-1", "label": "My Env"},
            {"id": "env-2"}
        ]"#;
        let envs: Vec<EnvironmentInfo> = serde_json::from_str(json).expect("should deserialize");
        assert_eq!(envs.len(), 2);
        assert_eq!(envs[0].id, "env-1");
        assert_eq!(envs[0].label.as_deref(), Some("My Env"));
        assert_eq!(envs[1].id, "env-2");
        assert!(envs[1].label.is_none());
    }

    // Task-to-session mapping tests

    #[test]
    fn map_task_pending_status() {
        let task = TaskItem {
            id: "t-1".to_string(),
            title: "My Task".to_string(),
            updated_at: Some(1710000000.0),
            task_status_display: Some(TaskStatusDisplay {
                environment_label: None,
                branch_name: Some("feat/cool".to_string()),
                latest_turn_status_display: Some(LatestTurnStatus { turn_status: Some("pending".to_string()) }),
            }),
            pull_requests: None,
        };
        let (id, session) = map_task_to_session(&task, "codex");
        assert_eq!(id, "t-1");
        assert_eq!(session.status, SessionStatus::Running);
        assert_eq!(session.title, "My Task");
        assert!(session.updated_at.is_some());
        assert!(session.correlation_keys.contains(&CorrelationKey::SessionRef("codex".to_string(), "t-1".to_string())));
        assert!(session.correlation_keys.contains(&CorrelationKey::Branch("feat/cool".to_string())));
    }

    #[test]
    fn map_task_in_progress_status() {
        let task = TaskItem {
            id: "t-2".to_string(),
            title: "Working".to_string(),
            updated_at: None,
            task_status_display: Some(TaskStatusDisplay {
                environment_label: None,
                branch_name: None,
                latest_turn_status_display: Some(LatestTurnStatus { turn_status: Some("in_progress".to_string()) }),
            }),
            pull_requests: None,
        };
        let (_, session) = map_task_to_session(&task, "codex");
        assert_eq!(session.status, SessionStatus::Running);
    }

    #[test]
    fn map_task_completed_status() {
        let task = TaskItem {
            id: "t-3".to_string(),
            title: "Done".to_string(),
            updated_at: None,
            task_status_display: Some(TaskStatusDisplay {
                environment_label: None,
                branch_name: Some("main".to_string()),
                latest_turn_status_display: Some(LatestTurnStatus { turn_status: Some("completed".to_string()) }),
            }),
            pull_requests: None,
        };
        let (_, session) = map_task_to_session(&task, "codex");
        assert_eq!(session.status, SessionStatus::Idle);
        // Branch is trunk ("main"), so only SessionRef is present
        assert_eq!(session.correlation_keys.len(), 1);
        assert!(session.correlation_keys.contains(&CorrelationKey::SessionRef("codex".to_string(), "t-3".to_string())));
    }

    #[test]
    fn map_task_skips_main_branch_correlation() {
        let task = TaskItem {
            id: "t-4".to_string(),
            title: "On main".to_string(),
            updated_at: None,
            task_status_display: Some(TaskStatusDisplay {
                environment_label: None,
                branch_name: Some("main".to_string()),
                latest_turn_status_display: Some(LatestTurnStatus { turn_status: Some("pending".to_string()) }),
            }),
            pull_requests: None,
        };
        let (_, session) = map_task_to_session(&task, "codex");
        assert_eq!(session.correlation_keys.len(), 1);
        assert!(session.correlation_keys.contains(&CorrelationKey::SessionRef("codex".to_string(), "t-4".to_string())));
    }

    #[test]
    fn map_task_with_pr_uses_head_branch_and_cr_ref() {
        let task = TaskItem {
            id: "t-5".to_string(),
            title: "PR task".to_string(),
            updated_at: None,
            task_status_display: Some(TaskStatusDisplay {
                environment_label: None,
                branch_name: Some("main".to_string()),
                latest_turn_status_display: Some(LatestTurnStatus { turn_status: Some("completed".to_string()) }),
            }),
            pull_requests: Some(vec![TaskPullRequestEntry {
                pull_request: Some(PullRequestDetail {
                    number: Some(99),
                    head: Some("feat/pr-branch".to_string()),
                    url: Some("https://github.com/owner/repo/pull/99".to_string()),
                }),
            }]),
        };
        let (_, session) = map_task_to_session(&task, "codex");
        assert!(session.correlation_keys.contains(&CorrelationKey::Branch("feat/pr-branch".to_string())));
        assert!(session.correlation_keys.contains(&CorrelationKey::ChangeRequestRef("github".to_string(), "99".to_string())));
        // Should NOT have Branch("main") — PR path doesn't add source branch
        assert!(!session.correlation_keys.contains(&CorrelationKey::Branch("main".to_string())));
    }

    #[test]
    fn map_task_empty_title_uses_id() {
        let task =
            TaskItem { id: "t-6".to_string(), title: String::new(), updated_at: None, task_status_display: None, pull_requests: None };
        let (_, session) = map_task_to_session(&task, "codex");
        assert_eq!(session.title, "t-6");
    }

    // Replay fixture integration tests

    #[tokio::test]
    async fn list_sessions_fetches_envs_and_tasks() {
        let session = replay::test_session(&fixture("codex_tasks.yaml"), replay::Masks::new());
        let http = replay::test_http_client(&session);
        let agent = CodexCodingAgent::new("codex".into(), http);

        // Prime auth cache with valid credentials
        {
            let mut cache = agent.auth_cache.lock().expect("lock");
            cache.auth = Some(CodexAuth { bearer_token: "test-token".to_string(), account_id: Some("acc-123".to_string()) });
            cache.loaded_at = Some(Instant::now());
        }

        let criteria = RepoCriteria { repo_slug: Some("rjwittams/flotilla".into()) };
        let sessions = agent.list_sessions(&criteria).await.expect("should succeed");

        assert_eq!(sessions.len(), 2, "expected 2 sessions");

        // Task 1: completed -> Idle, branch "feat-x" (non-trunk, empty PR list)
        let (id1, s1) = &sessions[0];
        assert_eq!(id1, "task_1");
        assert_eq!(s1.status, SessionStatus::Idle);
        assert!(s1.correlation_keys.contains(&CorrelationKey::Branch("feat-x".to_string())), "task_1 should correlate with branch feat-x");

        // Task 2: in_progress -> Running, PR head "codex/review-code", CR ref 208
        let (id2, s2) = &sessions[1];
        assert_eq!(id2, "task_2");
        assert_eq!(s2.status, SessionStatus::Running);
        assert!(
            s2.correlation_keys.contains(&CorrelationKey::Branch("codex/review-code".to_string())),
            "task_2 should correlate with PR head branch"
        );
        assert!(
            s2.correlation_keys.contains(&CorrelationKey::ChangeRequestRef("github".to_string(), "208".to_string())),
            "task_2 should have ChangeRequestRef for PR 208"
        );
        // Should NOT have Branch("main") — PR path doesn't add source branch
        assert!(
            !s2.correlation_keys.contains(&CorrelationKey::Branch("main".to_string())),
            "task_2 should not correlate with trunk branch main"
        );

        session.assert_complete();
    }

    // Auth retry replay test

    #[tokio::test]
    async fn list_sessions_retries_on_auth_error() {
        let _lock = CODEX_TEST_LOCK.lock().await;

        // Write auth.json with fresh token to a temp dir
        let tmp = tempfile::tempdir().expect("tempdir");
        let auth_json = r#"{"auth_mode":"chatgpt","tokens":{"access_token":"fresh-token","account_id":"acc-1"}}"#;
        std::fs::write(tmp.path().join("auth.json"), auth_json).expect("write auth.json");
        std::env::set_var("CODEX_HOME", tmp.path());

        let session = replay::test_session(&fixture("codex_auth_retry.yaml"), replay::Masks::new());
        let http = replay::test_http_client(&session);
        let agent = CodexCodingAgent::new("codex".into(), http);

        // Prime auth cache with expired token (no account_id)
        {
            let mut cache = agent.auth_cache.lock().expect("lock");
            cache.auth = Some(CodexAuth { bearer_token: "expired-token".to_string(), account_id: None });
            cache.loaded_at = Some(Instant::now());
        }

        let criteria = RepoCriteria { repo_slug: Some("owner/repo".into()) };
        let sessions = agent.list_sessions(&criteria).await.expect("should succeed");

        assert_eq!(sessions.len(), 1, "expected 1 session after auth retry");
        assert_eq!(sessions[0].0, "task_1");

        std::env::remove_var("CODEX_HOME");
        session.assert_complete();
    }

    // No-auth graceful degradation test

    #[tokio::test]
    async fn list_sessions_returns_empty_when_no_auth() {
        let _lock = CODEX_TEST_LOCK.lock().await;

        // Temp dir with NO auth.json
        let tmp = tempfile::tempdir().expect("tempdir");
        std::env::set_var("CODEX_HOME", tmp.path());

        // Empty fixture — no HTTP calls should be made
        let empty_dir = tempfile::tempdir().expect("tempdir");
        let empty_fixture = empty_dir.path().join("empty.yaml");
        std::fs::write(&empty_fixture, "interactions: []\n").expect("write empty fixture");
        let session = replay::test_session(empty_fixture.to_str().unwrap(), replay::Masks::new());
        let http = replay::test_http_client(&session);
        let agent = CodexCodingAgent::new("codex".into(), http);

        let criteria = RepoCriteria { repo_slug: Some("owner/repo".into()) };
        let sessions = agent.list_sessions(&criteria).await.expect("should succeed");

        assert!(sessions.is_empty(), "expected empty sessions when no auth");

        std::env::remove_var("CODEX_HOME");
    }
}
