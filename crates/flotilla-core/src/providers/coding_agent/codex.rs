use std::{
    collections::{HashMap, HashSet},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Instant,
};

use async_trait::async_trait;
use serde::Deserialize;
use tracing::{debug, warn};

use crate::{
    path_context::ExecutionEnvironmentPath,
    providers::{http_execute, types::*, HttpClient},
};

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

fn read_auth(auth_path: &ExecutionEnvironmentPath) -> Option<CodexAuth> {
    let contents = std::fs::read_to_string(auth_path.as_path()).ok()?;
    parse_auth_file(&contents)
}

// --- API response types ---

#[derive(Debug, Deserialize)]
pub struct EnvironmentInfo {
    pub id: String,
    #[serde(default)]
    pub label: Option<String>,
}

/// Full environment info from the `/wham/environments` list endpoint.
/// Each environment has a `repo_map` mapping repo refs to repo details
/// including the `repository_full_name` (e.g. "owner/repo").
#[derive(Debug, Deserialize)]
struct FullEnvironmentInfo {
    id: String,
    #[serde(default)]
    repo_map: HashMap<String, EnvironmentRepo>,
}

#[derive(Debug, Deserialize)]
struct EnvironmentRepo {
    repository_full_name: Option<String>,
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

    let title = if task.title.is_empty() { task.id.clone() } else { task.title.clone() };

    let updated_at = task.updated_at.and_then(epoch_to_rfc3339);

    (task.id.clone(), CloudAgentSession {
        title,
        status,
        model: None,
        updated_at,
        provider_name: provider_name.to_string(),
        provider_display_name: "Codex".into(),
        item_noun: "Task".into(),
    })
}

// --- CodexCodingAgent struct and HTTP helpers ---

const BASE_URL: &str = "https://chatgpt.com/backend-api";
const AUTH_CACHE_TTL_SECS: u64 = 300; // 5 minutes
const ENV_CACHE_TTL_SECS: u64 = 600; // 10 minutes

struct AuthCache {
    auth: Option<CodexAuth>,
    loaded_at: Option<Instant>,
}

struct EnvCacheEntry {
    environment_ids: Vec<String>,
    loaded_at: Instant,
}

struct EnvCache {
    entries: HashMap<String, EnvCacheEntry>,
}

pub struct CodexCodingAgent {
    provider_name: String,
    auth_path: ExecutionEnvironmentPath,
    http: Arc<dyn HttpClient>,
    auth_cache: Mutex<AuthCache>,
    env_cache: Mutex<EnvCache>,
    auth_warned: AtomicBool,
}

impl CodexCodingAgent {
    pub fn new(provider_name: String, auth_path: ExecutionEnvironmentPath, http: Arc<dyn HttpClient>) -> Self {
        Self {
            provider_name,
            auth_path,
            http,
            auth_cache: Mutex::new(AuthCache { auth: None, loaded_at: None }),
            env_cache: Mutex::new(EnvCache { entries: HashMap::new() }),
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
        let auth = read_auth(&self.auth_path);
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

    /// Fallback: fetch all environments and find those whose `repo_map` contains
    /// a repo matching `repo_slug`. This avoids matching by arbitrary environment
    /// label, which can cross-contaminate between repos.
    async fn fallback_via_all_environments(&self, repo_slug: &str, auth: &CodexAuth) -> Result<Vec<(String, CloudAgentSession)>, String> {
        let mut auth = auth.clone();
        let env_ids = match self.fetch_env_ids_from_all(&auth).await {
            Ok(ids) => ids,
            Err(e) if is_auth_error(&e) => {
                let fresh_auth = match self.refresh_auth() {
                    Some(a) => a,
                    None => {
                        if !self.auth_warned.swap(true, Ordering::Relaxed) {
                            warn!(provider = "codex", "Codex fallback environment list failed: auth refresh failed");
                        }
                        return Ok(vec![]);
                    }
                };
                match self.fetch_env_ids_from_all(&fresh_auth).await {
                    Ok(ids) => {
                        auth = fresh_auth;
                        ids
                    }
                    Err(retry_error) => {
                        if !self.auth_warned.swap(true, Ordering::Relaxed) {
                            warn!(
                                provider = "codex",
                                error = %retry_error,
                                "Codex fallback environment list unavailable after auth retry"
                            );
                        }
                        return Ok(vec![]);
                    }
                }
            }
            Err(e) => {
                debug!(provider = "codex", error = %e, "fallback environment list failed");
                return Ok(vec![]);
            }
        };

        // Deduplicate: an environment may appear multiple times if its repo_map
        // has several refs pointing at the same owner/repo.
        let mut seen = HashSet::new();
        let matching_ids: Vec<&str> = env_ids
            .iter()
            .filter(|(_, slug)| slug.eq_ignore_ascii_case(repo_slug))
            .map(|(id, _)| id.as_str())
            .filter(|id| seen.insert(*id))
            .collect();

        if matching_ids.is_empty() {
            return Ok(vec![]);
        }

        // Cache the discovered env IDs for this repo
        {
            let mut cache = self.env_cache.lock().expect("env_cache lock poisoned");
            cache.entries.insert(repo_slug.to_string(), EnvCacheEntry {
                environment_ids: matching_ids.iter().map(|s| s.to_string()).collect(),
                loaded_at: Instant::now(),
            });
        }

        let mut all_sessions = Vec::new();
        for env_id in matching_ids {
            match self.fetch_tasks(env_id, &auth).await {
                Ok(tasks) => {
                    for task in &tasks {
                        all_sessions.push(map_task_to_session(task, &self.provider_name));
                    }
                }
                Err(e) => {
                    debug!(provider = "codex", %env_id, error = %e, "fallback task fetch failed");
                }
            }
        }

        Ok(all_sessions)
    }

    /// Fetch all environments and return `(env_id, repository_full_name)` pairs.
    async fn fetch_env_ids_from_all(&self, auth: &CodexAuth) -> Result<Vec<(String, String)>, String> {
        let url = format!("{BASE_URL}/wham/environments");
        let request = self.build_request("GET", &url, auth)?;
        let resp = http_execute!(self.http, request)?;
        let status = resp.status().as_u16();
        if status == 401 || status == 403 {
            return Err(format!("authentication error (HTTP {status})"));
        }
        if !resp.status().is_success() {
            let body = String::from_utf8_lossy(resp.body()).to_string();
            return Err(format!("environment list failed (HTTP {status}): {body}"));
        }
        let envs: Vec<FullEnvironmentInfo> =
            serde_json::from_slice(resp.body()).map_err(|e| format!("environment list parse error: {e}"))?;

        Ok(envs
            .into_iter()
            .flat_map(|env| env.repo_map.into_values().filter_map(|repo| repo.repository_full_name).map(move |slug| (env.id.clone(), slug)))
            .collect())
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

        // Resolve environment IDs: check per-repo cache, then fetch
        let env_ids = {
            let cache = self.env_cache.lock().expect("env_cache lock poisoned");
            cache.entries.get(repo_slug.as_str()).and_then(|entry| {
                if entry.loaded_at.elapsed().as_secs() < ENV_CACHE_TTL_SECS {
                    Some(entry.environment_ids.clone())
                } else {
                    None
                }
            })
        };

        let env_ids = match env_ids {
            Some(ids) => ids,
            None => {
                match self.fetch_environment_ids(repo_slug, &auth).await {
                    Ok(ids) => {
                        let mut cache = self.env_cache.lock().expect("env_cache lock poisoned");
                        cache
                            .entries
                            .insert(repo_slug.to_string(), EnvCacheEntry { environment_ids: ids.clone(), loaded_at: Instant::now() });
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
                                cache.entries.insert(repo_slug.to_string(), EnvCacheEntry {
                                    environment_ids: ids.clone(),
                                    loaded_at: Instant::now(),
                                });
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
                        debug!(provider = "codex", error = %e, "by-repo lookup failed, falling back to all-environments repo_map");
                        return self.fallback_via_all_environments(repo_slug, &auth).await;
                    }
                }
            }
        };

        if env_ids.is_empty() {
            return self.fallback_via_all_environments(repo_slug, &auth).await;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{coding_agent::CloudAgentService, replay};

    #[tokio::test]
    async fn fallback_retries_environment_list_with_refreshed_auth() {
        let tmp = tempfile::tempdir().expect("create tempdir");
        let auth_path = tmp.path().join("auth.json");
        std::fs::write(&auth_path, r#"{"auth_mode":"chatgpt","tokens":{"access_token":"fresh-token","account_id":"acc-1"}}"#)
            .expect("write auth file");

        let fixture = crate::providers::testing::fixture_path("coding_agent", "codex_fallback_auth_retry.yaml");
        let session = replay::test_session(&fixture, replay::Masks::new());
        let http = replay::test_http_client(&session);
        let agent = CodexCodingAgent::new("codex".into(), ExecutionEnvironmentPath::new(auth_path), http);
        {
            let mut cache = agent.auth_cache.lock().expect("auth cache lock");
            cache.auth = Some(CodexAuth { bearer_token: "expired-token".into(), account_id: None });
            cache.loaded_at = Some(Instant::now());
        }

        let sessions = agent.list_sessions(&RepoCriteria { repo_slug: Some("owner/repo".into()) }).await.expect("list sessions");

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].0, "task-1");
        session.assert_complete();
    }
}
