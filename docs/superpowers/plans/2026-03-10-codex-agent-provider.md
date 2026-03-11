# Codex Cloud Agent Provider Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add OpenAI Codex as a `CloudAgentService` provider, listing cloud tasks via the ChatGPT backend API with branch/PR-based correlation.

**Architecture:** New `CodexCodingAgent` struct in `coding_agent/codex.rs` following the Cursor provider pattern (constructor takes `provider_name + http`). Auth reads `~/.codex/auth.json`. Repo filtering via environment-by-repo API call. Correlation uses branch name from task status display and PR head branch when available.

**Tech Stack:** Rust, async-trait, serde, reqwest (via `HttpClient` trait), replay test fixtures (YAML).

**Spec:** `docs/superpowers/specs/2026-03-10-codex-agent-provider-design.md`

---

## Chunk 1: Auth and API types

### Task 1: Auth file reader with tests

**Files:**
- Create: `crates/flotilla-core/src/providers/coding_agent/codex.rs`
- Modify: `crates/flotilla-core/src/providers/coding_agent/mod.rs`

- [ ] **Step 1: Create codex module and write auth parsing tests**

Add `pub mod codex;` to `crates/flotilla-core/src/providers/coding_agent/mod.rs` (after the `pub mod cursor;` line).

Create `crates/flotilla-core/src/providers/coding_agent/codex.rs` with auth types and tests:

```rust
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Instant;

// ---------- auth types ----------

#[derive(Debug, Deserialize)]
struct CodexAuthFile {
    auth_mode: String,
    #[serde(default)]
    tokens: Option<CodexTokens>,
    #[serde(rename = "OPENAI_API_KEY", default)]
    openai_api_key: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CodexTokens {
    access_token: String,
    #[serde(default)]
    account_id: Option<String>,
}

#[derive(Debug, Clone)]
struct CodexAuth {
    bearer_token: String,
    account_id: Option<String>,
}

struct AuthCache {
    auth: Option<CodexAuth>,
    loaded_at: Option<Instant>,
}

const AUTH_CACHE_TTL_SECS: u64 = 300; // 5 minutes

fn codex_home() -> PathBuf {
    if let Ok(home) = std::env::var("CODEX_HOME") {
        return PathBuf::from(home);
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".codex")
}

fn parse_auth_file(contents: &str) -> Option<CodexAuth> {
    let file: CodexAuthFile = serde_json::from_str(contents).ok()?;
    match file.auth_mode.as_str() {
        "chatgpt" => {
            let tokens = file.tokens?;
            Some(CodexAuth {
                bearer_token: tokens.access_token,
                account_id: tokens.account_id,
            })
        }
        "api-key" => {
            let key = file.openai_api_key.filter(|k| !k.is_empty())?;
            Some(CodexAuth {
                bearer_token: key,
                account_id: None,
            })
        }
        _ => None,
    }
}

fn read_auth() -> Option<CodexAuth> {
    let path = codex_home().join("auth.json");
    let contents = std::fs::read_to_string(path).ok()?;
    parse_auth_file(&contents)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_auth_chatgpt_mode() {
        let json = r#"{
            "auth_mode": "chatgpt",
            "tokens": {
                "access_token": "eyJhbGciOiJSUz...",
                "account_id": "acc-123",
                "refresh_token": "rt-456"
            }
        }"#;
        let auth = parse_auth_file(json).expect("should parse");
        assert_eq!(auth.bearer_token, "eyJhbGciOiJSUz...");
        assert_eq!(auth.account_id.as_deref(), Some("acc-123"));
    }

    #[test]
    fn parse_auth_api_key_mode() {
        let json = r#"{
            "auth_mode": "api-key",
            "OPENAI_API_KEY": "sk-test-key"
        }"#;
        let auth = parse_auth_file(json).expect("should parse");
        assert_eq!(auth.bearer_token, "sk-test-key");
        assert!(auth.account_id.is_none());
    }

    #[test]
    fn parse_auth_unknown_mode_returns_none() {
        let json = r#"{"auth_mode": "something-else"}"#;
        assert!(parse_auth_file(json).is_none());
    }

    #[test]
    fn parse_auth_malformed_json_returns_none() {
        assert!(parse_auth_file("not json").is_none());
    }

    #[test]
    fn parse_auth_chatgpt_missing_tokens_returns_none() {
        let json = r#"{"auth_mode": "chatgpt"}"#;
        assert!(parse_auth_file(json).is_none());
    }

    #[test]
    fn parse_auth_api_key_empty_key_returns_none() {
        let json = r#"{"auth_mode": "api-key", "OPENAI_API_KEY": ""}"#;
        assert!(parse_auth_file(json).is_none());
    }
}
```

- [ ] **Step 2: Run tests to verify they pass**

Run: `cargo test -p flotilla-core --lib providers::coding_agent::codex::tests -- --nocapture`
Expected: All 6 tests pass.

- [ ] **Step 3: Commit**

```bash
git add crates/flotilla-core/src/providers/coding_agent/codex.rs \
        crates/flotilla-core/src/providers/coding_agent/mod.rs
git commit -m "feat: add Codex auth file parser with tests"
```

### Task 2: API response deserialization types with tests

**Files:**
- Modify: `crates/flotilla-core/src/providers/coding_agent/codex.rs`

- [ ] **Step 1: Write deserialization tests for API response types**

Add API response structs and tests to `codex.rs`. These model the raw JSON from the `/wham/tasks/list` and `/wham/environments/by-repo/...` endpoints. All fields that might be absent are `Option` or `#[serde(default)]`.

```rust
use serde::Deserialize;

// ---------- API response types ----------

#[derive(Debug, Deserialize)]
struct EnvironmentInfo {
    id: String,
    #[serde(default)]
    label: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TaskListResponse {
    #[serde(default)]
    items: Vec<TaskItem>,
    #[serde(default)]
    cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TaskItem {
    id: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    updated_at: Option<f64>,
    #[serde(default)]
    task_status_display: Option<TaskStatusDisplay>,
    #[serde(default)]
    pull_requests: Option<Vec<TaskPullRequest>>,
}

#[derive(Debug, Deserialize)]
struct TaskStatusDisplay {
    #[serde(default)]
    environment_label: Option<String>,
    #[serde(default)]
    branch_name: Option<String>,
    #[serde(default)]
    latest_turn_status_display: Option<LatestTurnStatus>,
}

#[derive(Debug, Deserialize)]
struct LatestTurnStatus {
    #[serde(default)]
    turn_status: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TaskPullRequest {
    #[serde(default)]
    number: Option<u64>,
    #[serde(default)]
    head: Option<String>,
    #[serde(default)]
    url: Option<String>,
}
```

Add these tests:

```rust
#[test]
fn deserialize_task_list_response() {
    let json = r#"{
        "items": [{
            "id": "task_e_abc123",
            "title": "Fix bug",
            "updated_at": 1773176190.037,
            "task_status_display": {
                "environment_label": "flotilla",
                "branch_name": "main",
                "latest_turn_status_display": {
                    "turn_status": "completed"
                }
            },
            "pull_requests": [{
                "number": 208,
                "head": "codex/fix-bug",
                "url": "https://github.com/owner/repo/pull/208"
            }]
        }],
        "cursor": null
    }"#;
    let resp: TaskListResponse = serde_json::from_str(json).expect("parse");
    assert_eq!(resp.items.len(), 1);
    let task = &resp.items[0];
    assert_eq!(task.id, "task_e_abc123");
    assert_eq!(task.title, "Fix bug");
    let tsd = task.task_status_display.as_ref().unwrap();
    assert_eq!(tsd.branch_name.as_deref(), Some("main"));
    assert_eq!(tsd.environment_label.as_deref(), Some("flotilla"));
    let lts = tsd.latest_turn_status_display.as_ref().unwrap();
    assert_eq!(lts.turn_status.as_deref(), Some("completed"));
    let pr = &task.pull_requests.as_ref().unwrap()[0];
    assert_eq!(pr.number, Some(208));
    assert_eq!(pr.head.as_deref(), Some("codex/fix-bug"));
}

#[test]
fn deserialize_task_list_response_minimal() {
    // All optional fields absent — should still deserialize
    let json = r#"{"items": [{"id": "task_1"}]}"#;
    let resp: TaskListResponse = serde_json::from_str(json).expect("parse");
    assert_eq!(resp.items.len(), 1);
    assert_eq!(resp.items[0].id, "task_1");
    assert!(resp.items[0].task_status_display.is_none());
    assert!(resp.items[0].pull_requests.is_none());
}

#[test]
fn deserialize_environment_list() {
    let json = r#"[{"id": "env-abc", "label": "flotilla"}]"#;
    let envs: Vec<EnvironmentInfo> = serde_json::from_str(json).expect("parse");
    assert_eq!(envs.len(), 1);
    assert_eq!(envs[0].id, "env-abc");
    assert_eq!(envs[0].label.as_deref(), Some("flotilla"));
}
```

- [ ] **Step 2: Run tests**

Run: `cargo test -p flotilla-core --lib providers::coding_agent::codex::tests -- --nocapture`
Expected: All tests pass (previous 6 + new 3 = 9).

- [ ] **Step 3: Commit**

```bash
git add crates/flotilla-core/src/providers/coding_agent/codex.rs
git commit -m "feat: add Codex API response deserialization types"
```

### Task 3: Task-to-session mapping logic with tests

**Files:**
- Modify: `crates/flotilla-core/src/providers/coding_agent/codex.rs`

- [ ] **Step 1: Write mapping tests first**

Add the mapping function and its tests. This function converts a `TaskItem` into a `(String, CloudAgentSession)` tuple, handling status mapping, correlation key generation, and the branch/PR rules.

```rust
use crate::providers::types::*;

fn is_trunk_branch(name: &str) -> bool {
    matches!(name, "main" | "master")
}

fn epoch_to_rfc3339(epoch: f64) -> String {
    let secs = epoch as i64;
    let nanos = ((epoch - secs as f64) * 1_000_000_000.0) as u32;
    let dt = chrono::DateTime::<chrono::Utc>::from(
        std::time::UNIX_EPOCH + std::time::Duration::new(secs.max(0) as u64, nanos),
    );
    dt.to_rfc3339()
}

fn map_task_to_session(task: &TaskItem, provider_name: &str) -> (String, CloudAgentSession) {
    let tsd = task.task_status_display.as_ref();
    let lts = tsd.and_then(|d| d.latest_turn_status_display.as_ref());

    let status = match lts.and_then(|s| s.turn_status.as_deref()) {
        Some("pending" | "in_progress") => SessionStatus::Running,
        _ => SessionStatus::Idle,
    };

    let mut correlation_keys = vec![CorrelationKey::SessionRef(
        provider_name.to_string(),
        task.id.clone(),
    )];

    // PR-based correlation takes priority
    let prs = task.pull_requests.as_deref().unwrap_or_default();
    let has_pr = !prs.is_empty();
    if has_pr {
        for pr in prs {
            if let Some(ref head) = pr.head {
                correlation_keys.push(CorrelationKey::Branch(head.clone()));
            }
            if let Some(number) = pr.number {
                correlation_keys.push(CorrelationKey::ChangeRequestRef(
                    "github".to_string(),
                    number.to_string(),
                ));
            }
        }
    } else if let Some(branch) = tsd.and_then(|d| d.branch_name.as_deref()) {
        if !is_trunk_branch(branch) {
            correlation_keys.push(CorrelationKey::Branch(branch.to_string()));
        }
    }

    let title = if task.title.is_empty() {
        task.id.clone()
    } else {
        task.title.clone()
    };

    let updated_at = task.updated_at.map(epoch_to_rfc3339);

    (
        task.id.clone(),
        CloudAgentSession {
            title,
            status,
            model: None,
            updated_at,
            correlation_keys,
        },
    )
}
```

Add these tests:

```rust
#[test]
fn map_task_pending_status() {
    let task = TaskItem {
        id: "task_1".into(),
        title: "Do stuff".into(),
        updated_at: Some(1773176190.0),
        task_status_display: Some(TaskStatusDisplay {
            environment_label: Some("flotilla".into()),
            branch_name: Some("feat-x".into()),
            latest_turn_status_display: Some(LatestTurnStatus {
                turn_status: Some("pending".into()),
            }),
        }),
        pull_requests: None,
    };
    let (id, session) = map_task_to_session(&task, "codex");
    assert_eq!(id, "task_1");
    assert_eq!(session.title, "Do stuff");
    assert_eq!(session.status, SessionStatus::Running);
    assert!(session.updated_at.is_some());
    assert!(session.correlation_keys.contains(&CorrelationKey::Branch("feat-x".into())));
    assert!(session.correlation_keys.contains(&CorrelationKey::SessionRef("codex".into(), "task_1".into())));
}

#[test]
fn map_task_completed_status() {
    let task = TaskItem {
        id: "task_2".into(),
        title: "Review".into(),
        updated_at: None,
        task_status_display: Some(TaskStatusDisplay {
            environment_label: None,
            branch_name: None,
            latest_turn_status_display: Some(LatestTurnStatus {
                turn_status: Some("completed".into()),
            }),
        }),
        pull_requests: None,
    };
    let (_, session) = map_task_to_session(&task, "codex");
    assert_eq!(session.status, SessionStatus::Idle);
    // Only SessionRef, no branch
    assert_eq!(session.correlation_keys.len(), 1);
}

#[test]
fn map_task_skips_main_branch_correlation() {
    let task = TaskItem {
        id: "task_3".into(),
        title: "Fix".into(),
        updated_at: None,
        task_status_display: Some(TaskStatusDisplay {
            environment_label: None,
            branch_name: Some("main".into()),
            latest_turn_status_display: None,
        }),
        pull_requests: None,
    };
    let (_, session) = map_task_to_session(&task, "codex");
    // Should NOT have Branch("main")
    assert!(!session.correlation_keys.iter().any(|k| matches!(k, CorrelationKey::Branch(_))));
}

#[test]
fn map_task_with_pr_uses_head_branch_and_cr_ref() {
    let task = TaskItem {
        id: "task_4".into(),
        title: "PR task".into(),
        updated_at: None,
        task_status_display: Some(TaskStatusDisplay {
            environment_label: None,
            branch_name: Some("main".into()),  // would be skipped without PR
            latest_turn_status_display: None,
        }),
        pull_requests: Some(vec![TaskPullRequest {
            number: Some(208),
            head: Some("codex/fix-bug".into()),
            url: Some("https://github.com/owner/repo/pull/208".into()),
        }]),
    };
    let (_, session) = map_task_to_session(&task, "codex");
    assert!(session.correlation_keys.contains(&CorrelationKey::Branch("codex/fix-bug".into())));
    assert!(session.correlation_keys.contains(&CorrelationKey::ChangeRequestRef("github".into(), "208".into())));
    // Should NOT have Branch("main") — PR path takes priority
    assert!(!session.correlation_keys.contains(&CorrelationKey::Branch("main".into())));
}

#[test]
fn map_task_in_progress_status() {
    let task = TaskItem {
        id: "task_ip".into(),
        title: "Active".into(),
        updated_at: None,
        task_status_display: Some(TaskStatusDisplay {
            environment_label: None,
            branch_name: None,
            latest_turn_status_display: Some(LatestTurnStatus {
                turn_status: Some("in_progress".into()),
            }),
        }),
        pull_requests: None,
    };
    let (_, session) = map_task_to_session(&task, "codex");
    assert_eq!(session.status, SessionStatus::Running);
}

#[test]
fn map_task_empty_title_uses_id() {
    let task = TaskItem {
        id: "task_5".into(),
        title: String::new(),
        updated_at: None,
        task_status_display: None,
        pull_requests: None,
    };
    let (_, session) = map_task_to_session(&task, "codex");
    assert_eq!(session.title, "task_5");
}
```

- [ ] **Step 2: Run tests**

Run: `cargo test -p flotilla-core --lib providers::coding_agent::codex::tests -- --nocapture`
Expected: All tests pass (previous 9 + new 5 = 14).

- [ ] **Step 3: Commit**

```bash
git add crates/flotilla-core/src/providers/coding_agent/codex.rs
git commit -m "feat: add Codex task-to-session mapping with correlation"
```

## Chunk 2: HTTP client and CloudAgentService implementation

### Task 4: CodexCodingAgent struct and HTTP helpers

**Files:**
- Modify: `crates/flotilla-core/src/providers/coding_agent/codex.rs`

- [ ] **Step 1: Add the provider struct, auth cache, and HTTP request building**

```rust
use async_trait::async_trait;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tracing::{debug, info, warn};
use crate::providers::HttpClient;

const BASE_URL: &str = "https://chatgpt.com/backend-api";

pub struct CodexCodingAgent {
    provider_name: String,
    http: Arc<dyn HttpClient>,
    auth_cache: Mutex<AuthCache>,
    env_cache: Mutex<EnvCache>,
    auth_warned: AtomicBool,
}

struct EnvCache {
    environment_ids: Vec<String>,
    loaded_at: Option<Instant>,
}

const ENV_CACHE_TTL_SECS: u64 = 600; // 10 minutes

impl CodexCodingAgent {
    pub fn new(provider_name: String, http: Arc<dyn HttpClient>) -> Self {
        Self {
            provider_name,
            http,
            auth_cache: Mutex::new(AuthCache {
                auth: None,
                loaded_at: None,
            }),
            env_cache: Mutex::new(EnvCache {
                environment_ids: Vec::new(),
                loaded_at: None,
            }),
            auth_warned: AtomicBool::new(false),
        }
    }

    fn get_cached_auth(&self) -> Option<CodexAuth> {
        let cache = self.auth_cache.lock().unwrap();
        if let Some(ref loaded_at) = cache.loaded_at {
            if loaded_at.elapsed().as_secs() < AUTH_CACHE_TTL_SECS {
                return cache.auth.clone();
            }
        }
        None
    }

    fn refresh_auth(&self) -> Option<CodexAuth> {
        let auth = read_auth();
        let mut cache = self.auth_cache.lock().unwrap();
        cache.auth = auth.clone();
        cache.loaded_at = Some(Instant::now());
        auth
    }

    fn build_request(
        &self,
        method: &str,
        url: &str,
        auth: &CodexAuth,
    ) -> Result<reqwest::Request, String> {
        let method = reqwest::Method::from_bytes(method.as_bytes())
            .map_err(|e| format!("invalid HTTP method: {e}"))?;
        let mut builder = super::REQUEST_FACTORY
            .request(method, url)
            .header("authorization", format!("Bearer {}", auth.bearer_token))
            .header("user-agent", "flotilla");
        if let Some(ref account_id) = auth.account_id {
            builder = builder.header("chatgpt-account-id", account_id);
        }
        builder.build().map_err(|e| e.to_string())
    }

    async fn fetch_environment_ids(
        &self,
        repo_slug: &str,
        auth: &CodexAuth,
    ) -> Result<Vec<String>, String> {
        let (owner, repo) = repo_slug
            .split_once('/')
            .ok_or_else(|| format!("invalid repo slug: {repo_slug}"))?;
        let url = format!("{BASE_URL}/wham/environments/by-repo/github/{owner}/{repo}");
        let request = self.build_request("GET", &url, auth)?;
        let resp = self.http.execute(request).await?;
        let status = resp.status().as_u16();
        if status == 401 || status == 403 {
            return Err(format!("authentication error (HTTP {status})"));
        }
        if !resp.status().is_success() {
            let body = String::from_utf8_lossy(resp.body()).to_string();
            return Err(format!("environment lookup failed (HTTP {status}): {body}"));
        }
        let envs: Vec<EnvironmentInfo> = serde_json::from_slice(resp.body())
            .map_err(|e| format!("environment parse error: {e}"))?;
        Ok(envs.into_iter().map(|e| e.id).collect())
    }

    /// Fetch a single page from the tasks list endpoint, returning items and cursor.
    async fn fetch_tasks_page(
        &self,
        base_query: &str,
        cursor: Option<&str>,
        auth: &CodexAuth,
    ) -> Result<TaskListResponse, String> {
        let mut url = format!("{BASE_URL}/wham/tasks/list?task_filter=current&limit=20&{base_query}");
        if let Some(c) = cursor {
            url.push_str("&cursor=");
            url.push_str(&urlencoding::encode(c));
        }
        let request = self.build_request("GET", &url, auth)?;
        let resp = self.http.execute(request).await?;
        let status = resp.status().as_u16();
        if status == 401 || status == 403 {
            return Err(format!("authentication error (HTTP {status})"));
        }
        if !resp.status().is_success() {
            let body = String::from_utf8_lossy(resp.body()).to_string();
            return Err(format!("task list failed (HTTP {status}): {body}"));
        }
        serde_json::from_slice(resp.body())
            .map_err(|e| format!("task list parse error: {e}"))
    }

    /// Paginate through all tasks matching a base query string.
    async fn fetch_all_tasks(
        &self,
        base_query: &str,
        auth: &CodexAuth,
    ) -> Result<Vec<TaskItem>, String> {
        let mut all_items = Vec::new();
        let mut cursor: Option<String> = None;
        // Cap pages to avoid runaway loops (same as Cursor provider).
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

    /// Fallback: list all tasks (no env filter) and filter by environment_label
    /// matching the repo name portion of the slug (case-insensitive).
    async fn fetch_tasks_by_label(
        &self,
        repo_slug: &str,
        auth: &CodexAuth,
    ) -> Result<Vec<(String, CloudAgentSession)>, String> {
        let repo_name = repo_slug
            .split('/')
            .last()
            .unwrap_or(repo_slug)
            .to_lowercase();
        let items = match self.fetch_all_tasks("", auth).await {
            Ok(items) => items,
            Err(e) => {
                debug!(err = %e, "Codex label-fallback task fetch failed");
                return Ok(vec![]);
            }
        };
        Ok(items
            .iter()
            .filter(|t| {
                t.task_status_display
                    .as_ref()
                    .and_then(|d| d.environment_label.as_deref())
                    .is_some_and(|label| label.to_lowercase() == repo_name)
            })
            .map(|t| map_task_to_session(t, &self.provider_name))
            .collect())
    }

    async fn fetch_tasks(
        &self,
        environment_id: &str,
        auth: &CodexAuth,
    ) -> Result<Vec<TaskItem>, String> {
        self.fetch_all_tasks(&format!("environment_id={environment_id}"), auth).await
    }
}
```

- [ ] **Step 2: Verify it compiles**

Run: `cargo check -p flotilla-core`
Expected: Compiles without errors.

- [ ] **Step 3: Commit**

```bash
git add crates/flotilla-core/src/providers/coding_agent/codex.rs
git commit -m "feat: add CodexCodingAgent struct with HTTP helpers"
```

### Task 5: CloudAgentService trait implementation

**Files:**
- Modify: `crates/flotilla-core/src/providers/coding_agent/codex.rs`

- [ ] **Step 1: Implement the trait**

```rust
#[async_trait]
impl super::CloudAgentService for CodexCodingAgent {
    fn display_name(&self) -> &str {
        "Codex"
    }

    fn item_noun(&self) -> &str {
        "task"
    }

    fn abbreviation(&self) -> &str {
        "Cdx"
    }

    async fn list_sessions(
        &self,
        criteria: &RepoCriteria,
    ) -> Result<Vec<(String, CloudAgentSession)>, String> {
        let Some(ref slug) = criteria.repo_slug else {
            return Ok(vec![]);
        };

        // Get auth — try cache, then re-read file
        let auth = match self.get_cached_auth().or_else(|| self.refresh_auth()) {
            Some(auth) => auth,
            None => {
                if !self.auth_warned.swap(true, Ordering::Relaxed) {
                    warn!("Codex tasks unavailable: no auth in ~/.codex/auth.json");
                }
                return Ok(vec![]);
            }
        };

        // Get environment IDs — try cache first
        let env_ids = {
            let cache = self.env_cache.lock().unwrap();
            if let Some(ref loaded_at) = cache.loaded_at {
                if loaded_at.elapsed().as_secs() < ENV_CACHE_TTL_SECS
                    && !cache.environment_ids.is_empty()
                {
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
                let ids = match self.fetch_environment_ids(slug, &auth).await {
                    Ok(ids) => ids,
                    Err(e) if e.contains("authentication") => {
                        // Retry with fresh auth
                        match self.refresh_auth() {
                            Some(fresh) => {
                                match self.fetch_environment_ids(slug, &fresh).await {
                                    Ok(ids) => ids,
                                    Err(e) => {
                                        if !self.auth_warned.swap(true, Ordering::Relaxed) {
                                            warn!(err = %e, "Codex auth retry failed");
                                        }
                                        return Ok(vec![]);
                                    }
                                }
                            }
                            None => {
                                if !self.auth_warned.swap(true, Ordering::Relaxed) {
                                    warn!("Codex auth refresh failed: no valid auth");
                                }
                                return Ok(vec![]);
                            }
                        }
                    }
                    Err(e) => {
                        // Non-auth failure (5xx, shape drift, etc.) — use label fallback
                        debug!(err = %e, "Codex environment lookup failed, trying label fallback");
                        return self.fetch_tasks_by_label(slug, &auth).await;
                    }
                };
                let mut cache = self.env_cache.lock().unwrap();
                cache.environment_ids = ids.clone();
                cache.loaded_at = Some(Instant::now());
                ids
            }
        };

        if env_ids.is_empty() {
            debug!(%slug, "no Codex environments found for repo, trying label fallback");
            return self.fetch_tasks_by_label(slug, &auth).await;
        }

        // Fetch tasks for each environment
        let mut all_sessions = Vec::new();
        for env_id in &env_ids {
            match self.fetch_tasks(env_id, &auth).await {
                Ok(tasks) => {
                    debug!(env_id, count = tasks.len(), "Codex tasks fetched");
                    for task in &tasks {
                        all_sessions.push(map_task_to_session(task, &self.provider_name));
                    }
                }
                Err(e) if e.contains("authentication") => {
                    // Retry once with refreshed auth
                    if let Some(fresh) = self.refresh_auth() {
                        if let Ok(tasks) = self.fetch_tasks(env_id, &fresh).await {
                            for task in &tasks {
                                all_sessions
                                    .push(map_task_to_session(task, &self.provider_name));
                            }
                        }
                    }
                }
                Err(e) => {
                    warn!(env_id, err = %e, "Codex task fetch failed");
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
```

- [ ] **Step 2: Verify it compiles**

Run: `cargo check -p flotilla-core`
Expected: Compiles without errors.

- [ ] **Step 3: Commit**

```bash
git add crates/flotilla-core/src/providers/coding_agent/codex.rs
git commit -m "feat: implement CloudAgentService for CodexCodingAgent"
```

## Chunk 3: Integration tests with replay fixtures

### Task 6: Replay fixture for environment lookup + task list

**Files:**
- Create: `crates/flotilla-core/src/providers/coding_agent/fixtures/codex_tasks.yaml`

- [ ] **Step 1: Create replay fixture**

```yaml
interactions:
- channel: http
  method: GET
  url: "https://chatgpt.com/backend-api/wham/environments/by-repo/github/rjwittams/flotilla"
  request_headers:
    authorization: "Bearer test-token"
    chatgpt-account-id: "acc-123"
  status: 200
  response_body: >-
    [{"id": "env-abc", "label": "flotilla"}]

- channel: http
  method: GET
  url: "https://chatgpt.com/backend-api/wham/tasks/list?task_filter=current&environment_id=env-abc&limit=20"
  request_headers:
    authorization: "Bearer test-token"
    chatgpt-account-id: "acc-123"
  status: 200
  response_body: >-
    {"items": [
      {"id": "task_1", "title": "Fix concurrency bug", "updated_at": 1773176190.0,
       "task_status_display": {"environment_label": "flotilla", "branch_name": "feat-x",
         "latest_turn_status_display": {"turn_status": "completed"}},
       "pull_requests": []},
      {"id": "task_2", "title": "Review code", "updated_at": 1773176100.0,
       "task_status_display": {"environment_label": "flotilla", "branch_name": "main",
         "latest_turn_status_display": {"turn_status": "in_progress"}},
       "pull_requests": [{"number": 208, "head": "codex/review-code", "url": "https://github.com/rjwittams/flotilla/pull/208"}]}
    ], "cursor": null}
```

- [ ] **Step 2: Write integration test**

Add to `codex.rs` tests:

```rust
use crate::providers::coding_agent::CloudAgentService;
use crate::providers::replay;
use std::sync::Arc;

fn fixture(name: &str) -> String {
    format!(
        "{}/src/providers/coding_agent/fixtures/{}",
        env!("CARGO_MANIFEST_DIR"),
        name
    )
}

#[tokio::test]
async fn list_sessions_fetches_envs_and_tasks() {
    let session = replay::test_session(
        &fixture("codex_tasks.yaml"),
        replay::Masks::new(),
    );
    let http = replay::test_http_client(&session);
    let agent = CodexCodingAgent::new("codex".into(), http);

    // Prime auth cache with test token
    {
        let mut cache = agent.auth_cache.lock().unwrap();
        cache.auth = Some(CodexAuth {
            bearer_token: "test-token".into(),
            account_id: Some("acc-123".into()),
        });
        cache.loaded_at = Some(Instant::now());
    }

    let sessions = agent
        .list_sessions(&RepoCriteria {
            repo_slug: Some("rjwittams/flotilla".into()),
        })
        .await
        .expect("list sessions");
    session.assert_complete();

    assert_eq!(sessions.len(), 2);

    let (id1, s1) = sessions.iter().find(|(id, _)| id == "task_1").unwrap();
    assert_eq!(s1.title, "Fix concurrency bug");
    assert_eq!(s1.status, SessionStatus::Idle);
    assert!(s1.correlation_keys.contains(&CorrelationKey::Branch("feat-x".into())));

    let (id2, s2) = sessions.iter().find(|(id, _)| id == "task_2").unwrap();
    assert_eq!(s2.title, "Review code");
    assert_eq!(s2.status, SessionStatus::Running);
    assert!(s2.correlation_keys.contains(&CorrelationKey::Branch("codex/review-code".into())));
    assert!(s2.correlation_keys.contains(&CorrelationKey::ChangeRequestRef("github".into(), "208".into())));
    // Should NOT have Branch("main") — PR path takes priority
    assert!(!s2.correlation_keys.contains(&CorrelationKey::Branch("main".into())));
}
```

- [ ] **Step 3: Run the test**

Run: `cargo test -p flotilla-core --lib providers::coding_agent::codex::tests::list_sessions_fetches_envs_and_tasks -- --nocapture`
Expected: PASS

- [ ] **Step 4: Commit**

```bash
git add crates/flotilla-core/src/providers/coding_agent/codex.rs \
        crates/flotilla-core/src/providers/coding_agent/fixtures/codex_tasks.yaml
git commit -m "test: add Codex provider replay integration test"
```

### Task 7: Auth retry replay test

**Files:**
- Create: `crates/flotilla-core/src/providers/coding_agent/fixtures/codex_auth_retry.yaml`
- Modify: `crates/flotilla-core/src/providers/coding_agent/codex.rs`

**Note:** Tasks 7 and 8 set `CODEX_HOME` env var, which is process-global. Add a `TEST_LOCK` at the top of the test module (like the Claude provider uses) to serialize tests that touch env vars:

```rust
use std::sync::LazyLock;
use tokio::sync::Mutex as AsyncMutex;

static CODEX_TEST_LOCK: LazyLock<AsyncMutex<()>> = LazyLock::new(|| AsyncMutex::new(()));
```

Each env-var test must acquire `let _lock = CODEX_TEST_LOCK.lock().await;` at the start.

- [ ] **Step 1: Create auth retry fixture**

First call returns 401, second call (after auth refresh) succeeds.

```yaml
interactions:
- channel: http
  method: GET
  url: "https://chatgpt.com/backend-api/wham/environments/by-repo/github/owner/repo"
  request_headers:
    authorization: "Bearer expired-token"
  status: 401
  response_body: '{"error": "unauthorized"}'

- channel: http
  method: GET
  url: "https://chatgpt.com/backend-api/wham/environments/by-repo/github/owner/repo"
  request_headers:
    authorization: "Bearer fresh-token"
    chatgpt-account-id: "acc-1"
  status: 200
  response_body: '[{"id": "env-1"}]'

- channel: http
  method: GET
  url: "https://chatgpt.com/backend-api/wham/tasks/list?task_filter=current&environment_id=env-1&limit=20"
  request_headers:
    authorization: "Bearer fresh-token"
    chatgpt-account-id: "acc-1"
  status: 200
  response_body: '{"items": [{"id": "task_1", "title": "Test"}], "cursor": null}'
```

- [ ] **Step 2: Write the test**

This test requires that `read_auth()` returns a fresh token on the second call. To make this testable, refactor `read_auth` to accept a path, and use a temp file in the test. Alternatively, pre-populate the auth cache with the expired token, then write a fresh auth.json to a temp CODEX_HOME before the retry reads it.

```rust
#[tokio::test]
async fn list_sessions_retries_on_auth_error() {
    let _lock = CODEX_TEST_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let auth_path = dir.path().join("auth.json");
    // Write fresh token that will be read on retry
    std::fs::write(
        &auth_path,
        r#"{"auth_mode":"chatgpt","tokens":{"access_token":"fresh-token","account_id":"acc-1"}}"#,
    ).unwrap();
    // Point CODEX_HOME to temp dir
    std::env::set_var("CODEX_HOME", dir.path());

    let session = replay::test_session(
        &fixture("codex_auth_retry.yaml"),
        replay::Masks::new(),
    );
    let http = replay::test_http_client(&session);
    let agent = CodexCodingAgent::new("codex".into(), http);

    // Prime with expired token
    {
        let mut cache = agent.auth_cache.lock().unwrap();
        cache.auth = Some(CodexAuth {
            bearer_token: "expired-token".into(),
            account_id: None,
        });
        cache.loaded_at = Some(Instant::now());
    }

    let sessions = agent
        .list_sessions(&RepoCriteria {
            repo_slug: Some("owner/repo".into()),
        })
        .await
        .expect("retry should succeed");
    session.assert_complete();

    assert_eq!(sessions.len(), 1);

    std::env::remove_var("CODEX_HOME");
}
```

- [ ] **Step 3: Run the test**

Run: `cargo test -p flotilla-core --lib providers::coding_agent::codex::tests::list_sessions_retries_on_auth_error -- --nocapture`
Expected: PASS

- [ ] **Step 4: Commit**

```bash
git add crates/flotilla-core/src/providers/coding_agent/codex.rs \
        crates/flotilla-core/src/providers/coding_agent/fixtures/codex_auth_retry.yaml
git commit -m "test: add Codex auth retry flow test"
```

### Task 8: No-auth graceful degradation test

**Files:**
- Modify: `crates/flotilla-core/src/providers/coding_agent/codex.rs`

- [ ] **Step 1: Write the test**

```rust
#[tokio::test]
async fn list_sessions_returns_empty_when_no_auth() {
    let _lock = CODEX_TEST_LOCK.lock().await;
    // Point CODEX_HOME to a directory with no auth.json
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("CODEX_HOME", dir.path());

    let empty_fixture_path = dir.path().join("empty.yaml");
    std::fs::write(&empty_fixture_path, "interactions: []\n").unwrap();
    let session = replay::test_session(empty_fixture_path.to_str().unwrap(), replay::Masks::new());
    let http = replay::test_http_client(&session);
    let agent = CodexCodingAgent::new("codex".into(), http);

    let sessions = agent
        .list_sessions(&RepoCriteria {
            repo_slug: Some("owner/repo".into()),
        })
        .await
        .expect("should degrade gracefully");

    assert!(sessions.is_empty());

    std::env::remove_var("CODEX_HOME");
}
```

- [ ] **Step 2: Run the test**

Run: `cargo test -p flotilla-core --lib providers::coding_agent::codex::tests::list_sessions_returns_empty_when_no_auth -- --nocapture`
Expected: PASS

- [ ] **Step 3: Commit**

```bash
git add crates/flotilla-core/src/providers/coding_agent/codex.rs
git commit -m "test: add Codex no-auth graceful degradation test"
```

## Chunk 4: Provider discovery registration

### Task 9: Register CodexCodingAgent in discovery

**Files:**
- Modify: `crates/flotilla-core/src/providers/discovery.rs`

- [ ] **Step 1: Add import and registration**

Add import at the top of `discovery.rs`:

```rust
use crate::providers::coding_agent::codex::CodexCodingAgent;
```

Add registration after the Cursor block (after the closing `}` of the `if std::env::var("CURSOR_API_KEY")...` block, before the Claude section).

Gate on `~/.codex/auth.json` existence rather than the `codex` binary. The provider uses the ChatGPT backend API directly and reads file-based auth — nothing at runtime requires the CLI. This avoids false negatives for users who have Codex auth but installed the CLI elsewhere or use the web-only flow.

```rust
    // 4b. Cloud agent: Codex (gated on auth file, not binary — provider uses API directly)
    if codex::codex_auth_file_exists() {
        registry.cloud_agents.insert(
            "codex".to_string(),
            Arc::new(CodexCodingAgent::new(
                "codex".to_string(),
                Arc::new(crate::providers::ReqwestHttpClient::new()),
            )),
        );
        info!(%repo_name, "Cloud agent → Codex");
    }
```

This requires making `codex_auth_file_exists()` a public function in `codex.rs`:

```rust
/// Check whether the Codex auth file exists (used by discovery).
pub fn codex_auth_file_exists() -> bool {
    codex_home().join("auth.json").exists()
}
```

- [ ] **Step 2: Add discovery test**

Add a test in the `tests` module of `discovery.rs`. Since the gate is file-based (not binary-based), the test creates/removes a temp auth file and points `CODEX_HOME` to it:

```rust
#[tokio::test]
async fn detect_providers_codex_registration_depends_on_auth_file() {
    let dir = tempfile::tempdir().unwrap();
    let codex_dir = dir.path().join(".codex-test");
    std::fs::create_dir_all(&codex_dir).unwrap();

    // With auth.json present → registered
    std::fs::write(
        codex_dir.join("auth.json"),
        r#"{"auth_mode":"chatgpt","tokens":{"access_token":"t","account_id":"a"}}"#,
    ).unwrap();
    std::env::set_var("CODEX_HOME", &codex_dir);
    {
        let (dir2, repo) = make_repo_with_git_dir();
        let config = temp_config(&dir2);
        let runner: Arc<dyn CommandRunner> = Arc::new(
            discovery_runner()
                .on_run("git", &["remote"], Err("no remotes".to_string()))
                .tool_exists("wt", false)
                .tool_exists("gh", false)
                .tool_exists("claude", false)
                .build(),
        );
        let (registry, _) = detect_providers(&repo, &config, runner).await;
        assert!(registry.cloud_agents.contains_key("codex"));
    }

    // Without auth.json → not registered
    std::fs::remove_file(codex_dir.join("auth.json")).unwrap();
    {
        let (dir2, repo) = make_repo_with_git_dir();
        let config = temp_config(&dir2);
        let runner: Arc<dyn CommandRunner> = Arc::new(
            discovery_runner()
                .on_run("git", &["remote"], Err("no remotes".to_string()))
                .tool_exists("wt", false)
                .tool_exists("gh", false)
                .tool_exists("claude", false)
                .build(),
        );
        let (registry, _) = detect_providers(&repo, &config, runner).await;
        assert!(!registry.cloud_agents.contains_key("codex"));
    }

    std::env::remove_var("CODEX_HOME");
}
```

Note: existing discovery tests don't need updating — Codex registration is gated on a file, not a tool_exists check.

- [ ] **Step 4: Run all discovery tests**

Run: `cargo test -p flotilla-core --lib providers::discovery::tests -- --nocapture`
Expected: All tests pass including the new one.

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-core/src/providers/discovery.rs
git commit -m "feat: register Codex provider in discovery"
```

## Chunk 5: Final verification

### Task 10: Full test suite and lint

- [ ] **Step 1: Run full test suite**

Run: `cargo test --locked`
Expected: All tests pass.

- [ ] **Step 2: Run clippy**

Run: `cargo clippy --all-targets --locked -- -D warnings`
Expected: No warnings.

- [ ] **Step 3: Run fmt**

Run: `cargo fmt --check`
Expected: No formatting issues. If there are, run `cargo fmt` and commit.

- [ ] **Step 4: Final commit if any formatting fixes needed**

```bash
git add -u
git commit -m "chore: fix formatting"
```
