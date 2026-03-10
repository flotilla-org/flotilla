use serde::Deserialize;
use std::path::PathBuf;

use crate::providers::types::*;

// ---------------------------------------------------------------------------
// Task 1: Auth file reader
// ---------------------------------------------------------------------------

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
        dirs::home_dir()
            .expect("could not determine home directory")
            .join(".codex")
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
            Some(CodexAuth {
                bearer_token: tokens.access_token,
                account_id: tokens.account_id,
            })
        }
        "api-key" => {
            let key = file.openai_api_key?;
            if key.is_empty() {
                return None;
            }
            Some(CodexAuth {
                bearer_token: key,
                account_id: None,
            })
        }
        _ => None,
    }
}

#[allow(dead_code)]
fn read_auth() -> Option<CodexAuth> {
    let path = codex_home().join("auth.json");
    let contents = std::fs::read_to_string(path).ok()?;
    parse_auth_file(&contents)
}

pub fn codex_auth_file_exists() -> bool {
    codex_home().join("auth.json").exists()
}

// ---------------------------------------------------------------------------
// Task 2: API response deserialization types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct EnvironmentInfo {
    pub id: String,
    #[serde(default)]
    pub label: Option<String>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct TaskListResponse {
    #[serde(default)]
    pub items: Vec<TaskItem>,
    #[serde(default)]
    pub cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct TaskItem {
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub updated_at: Option<f64>,
    #[serde(default)]
    pub task_status_display: Option<TaskStatusDisplay>,
    #[serde(default)]
    pub pull_requests: Option<Vec<TaskPullRequest>>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct TaskStatusDisplay {
    #[serde(default)]
    pub environment_label: Option<String>,
    #[serde(default)]
    pub branch_name: Option<String>,
    #[serde(default)]
    pub latest_turn_status_display: Option<LatestTurnStatus>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct LatestTurnStatus {
    #[serde(default)]
    pub turn_status: Option<String>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct TaskPullRequest {
    #[serde(default)]
    pub number: Option<u64>,
    #[serde(default)]
    pub head: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
}

// ---------------------------------------------------------------------------
// Task 3: Task-to-session mapping logic
// ---------------------------------------------------------------------------

#[allow(dead_code)]
fn is_trunk_branch(name: &str) -> bool {
    matches!(name, "main" | "master")
}

#[allow(dead_code)]
fn epoch_to_rfc3339(epoch: f64) -> Option<String> {
    use chrono::{TimeZone, Utc};
    let secs = epoch as i64;
    let nanos = ((epoch - secs as f64) * 1_000_000_000.0).clamp(0.0, 999_999_999.0) as u32;
    Utc.timestamp_opt(secs, nanos)
        .single()
        .map(|dt| dt.to_rfc3339())
}

#[allow(dead_code)]
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

    let mut correlation_keys = vec![CorrelationKey::SessionRef(
        provider_name.to_string(),
        task.id.clone(),
    )];

    if let Some(prs) = &task.pull_requests {
        if !prs.is_empty() {
            for pr in prs {
                if let Some(ref head) = pr.head {
                    if !head.is_empty() {
                        correlation_keys.push(CorrelationKey::Branch(head.clone()));
                    }
                }
                if let Some(number) = pr.number {
                    correlation_keys.push(CorrelationKey::ChangeRequestRef(
                        "github".to_string(),
                        number.to_string(),
                    ));
                }
            }
        } else {
            // Empty PR list — use source branch if it's not trunk
            if let Some(ref display) = task.task_status_display {
                if let Some(ref branch) = display.branch_name {
                    if !branch.is_empty() && !is_trunk_branch(branch) {
                        correlation_keys.push(CorrelationKey::Branch(branch.clone()));
                    }
                }
            }
        }
    } else {
        // No PR — use source branch if it's not trunk
        if let Some(ref display) = task.task_status_display {
            if let Some(ref branch) = display.branch_name {
                if !branch.is_empty() && !is_trunk_branch(branch) {
                    correlation_keys.push(CorrelationKey::Branch(branch.clone()));
                }
            }
        }
    }

    let title = if task.title.is_empty() {
        task.id.clone()
    } else {
        task.title.clone()
    };

    let updated_at = task.updated_at.and_then(epoch_to_rfc3339);

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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // Task 1 tests

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

    // Task 2 tests

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
                            "number": 42,
                            "head": "fix/bug",
                            "url": "https://github.com/owner/repo/pull/42"
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
        let turn = display
            .latest_turn_status_display
            .as_ref()
            .expect("has turn");
        assert_eq!(turn.turn_status.as_deref(), Some("in_progress"));
        let prs = task.pull_requests.as_ref().expect("has PRs");
        assert_eq!(prs[0].number, Some(42));
        assert_eq!(prs[0].head.as_deref(), Some("fix/bug"));
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

    // Task 3 tests

    #[test]
    fn map_task_pending_status() {
        let task = TaskItem {
            id: "t-1".to_string(),
            title: "My Task".to_string(),
            updated_at: Some(1710000000.0),
            task_status_display: Some(TaskStatusDisplay {
                environment_label: None,
                branch_name: Some("feat/cool".to_string()),
                latest_turn_status_display: Some(LatestTurnStatus {
                    turn_status: Some("pending".to_string()),
                }),
            }),
            pull_requests: None,
        };
        let (id, session) = map_task_to_session(&task, "codex");
        assert_eq!(id, "t-1");
        assert_eq!(session.status, SessionStatus::Running);
        assert_eq!(session.title, "My Task");
        assert!(session.updated_at.is_some());
        assert!(session
            .correlation_keys
            .contains(&CorrelationKey::SessionRef(
                "codex".to_string(),
                "t-1".to_string()
            )));
        assert!(session
            .correlation_keys
            .contains(&CorrelationKey::Branch("feat/cool".to_string())));
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
                latest_turn_status_display: Some(LatestTurnStatus {
                    turn_status: Some("in_progress".to_string()),
                }),
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
                latest_turn_status_display: Some(LatestTurnStatus {
                    turn_status: Some("completed".to_string()),
                }),
            }),
            pull_requests: None,
        };
        let (_, session) = map_task_to_session(&task, "codex");
        assert_eq!(session.status, SessionStatus::Idle);
        // Branch is trunk ("main"), so only SessionRef is present
        assert_eq!(session.correlation_keys.len(), 1);
        assert!(session
            .correlation_keys
            .contains(&CorrelationKey::SessionRef(
                "codex".to_string(),
                "t-3".to_string()
            )));
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
                latest_turn_status_display: Some(LatestTurnStatus {
                    turn_status: Some("pending".to_string()),
                }),
            }),
            pull_requests: None,
        };
        let (_, session) = map_task_to_session(&task, "codex");
        assert_eq!(session.correlation_keys.len(), 1);
        assert!(session
            .correlation_keys
            .contains(&CorrelationKey::SessionRef(
                "codex".to_string(),
                "t-4".to_string()
            )));
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
                latest_turn_status_display: Some(LatestTurnStatus {
                    turn_status: Some("completed".to_string()),
                }),
            }),
            pull_requests: Some(vec![TaskPullRequest {
                number: Some(99),
                head: Some("feat/pr-branch".to_string()),
                url: Some("https://github.com/owner/repo/pull/99".to_string()),
            }]),
        };
        let (_, session) = map_task_to_session(&task, "codex");
        assert!(session
            .correlation_keys
            .contains(&CorrelationKey::Branch("feat/pr-branch".to_string())));
        assert!(session
            .correlation_keys
            .contains(&CorrelationKey::ChangeRequestRef(
                "github".to_string(),
                "99".to_string()
            )));
        // Should NOT have Branch("main") — PR path doesn't add source branch
        assert!(!session
            .correlation_keys
            .contains(&CorrelationKey::Branch("main".to_string())));
    }

    #[test]
    fn map_task_empty_title_uses_id() {
        let task = TaskItem {
            id: "t-6".to_string(),
            title: String::new(),
            updated_at: None,
            task_status_display: None,
            pull_requests: None,
        };
        let (_, session) = map_task_to_session(&task, "codex");
        assert_eq!(session.title, "t-6");
    }
}
