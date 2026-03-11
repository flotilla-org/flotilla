# Codex Cloud Agent Provider

Issue: #52 | Related: #209

## Summary

Add OpenAI Codex as a `CloudAgentService` provider. Uses the ChatGPT backend API directly (not the CLI) to list cloud tasks, extract branch and PR correlation data, and filter tasks by repo via environment lookup.

**API stability caveat**: The `/backend-api/wham/` endpoints are ChatGPT's internal API, not a published developer API. They may change without notice. The implementation should degrade gracefully on unexpected response shapes (missing fields default to None/empty, unknown status strings map to a default).

## API Surface

Base URL: `https://chatgpt.com/backend-api`

Auth: `Authorization: Bearer {access_token}` + `ChatGPT-Account-Id: {account_id}` headers, sourced from `~/.codex/auth.json` `tokens` object.

### Endpoints used

| Endpoint | Purpose |
|----------|---------|
| `GET /wham/environments/by-repo/github/{owner}/{repo}` | Get environment IDs for this repo |
| `GET /wham/tasks/list?task_filter=current&environment_id={id}&limit=20` | List tasks for an environment |

### Task list response shape

```json
{
  "items": [{
    "id": "task_e_...",
    "title": "Review for concurrency issues",
    "updated_at": 1773176190.037205,
    "task_status_display": {
      "environment_label": "flotilla",
      "branch_name": "main",
      "latest_turn_status_display": {
        "turn_status": "completed",
        "diff_stats": { "files_modified": 1, "lines_added": 57, "lines_removed": 7 }
      }
    },
    "pull_requests": [{
      "number": 208,
      "url": "https://github.com/owner/repo/pull/208",
      "head": "codex/review-for-concurrency-issues",
      "state": "open"
    }]
  }],
  "cursor": null
}
```

All nested fields are optional — the implementation must tolerate any of them being absent.

## Auth

Read `~/.codex/auth.json` (path: `$CODEX_HOME/auth.json`, where `CODEX_HOME` defaults to `~/.codex`):

```json
{
  "auth_mode": "chatgpt",
  "tokens": {
    "access_token": "<JWT>",
    "account_id": "<string>",
    "refresh_token": "<string>"
  }
}
```

Behavior by auth mode:
- `"chatgpt"`: Use `tokens.access_token` as bearer token, `tokens.account_id` as the `ChatGPT-Account-Id` header.
- `"api-key"`: Use `OPENAI_API_KEY` field as bearer token, no account ID header.
- Any other mode or missing fields: skip provider gracefully.

Cache:
- Cache parsed auth with ~5 min TTL.
- On 401: re-read file, retry once. If still 401, warn and return empty (like Claude's graceful degradation).
- If file missing or unparseable: skip provider, no error.

## Discovery

In `detect_providers()`, check for `~/.codex/auth.json` existence via `codex::codex_auth_file_exists()`. If present, register `CodexCodingAgent`. Gate on the auth file rather than the `codex` binary because the provider uses the ChatGPT backend API directly — nothing at runtime requires the CLI. This avoids false negatives for users who have Codex auth but don't have the CLI on PATH.

Constructor: `CodexCodingAgent::new(provider_name: String, http: Arc<dyn HttpClient>)`. No `CommandRunner` dependency — auth is file-based, not keychain-based.

## Repo Filtering

1. Extract `owner/repo` from git remote (already done by discovery as `RepoCriteria.repo_slug`).
2. Call `/wham/environments/by-repo/github/{owner}/{repo}` to get environment IDs.
3. Filter task list by `environment_id`.
4. Cache environment IDs with a long TTL (environments rarely change).

If the by-repo call fails or returns empty, fall back to listing all tasks and filtering by `environment_label` matching the repo name (the part after `/` in `owner/repo`), case-insensitive exact match.

## Field Mapping

| API field | `CloudAgentSession` field |
|-----------|--------------------------|
| `title` | `title` |
| `task_status_display.latest_turn_status_display.turn_status` | `status` (see mapping below) |
| (not available) | `model` = `None` |
| `updated_at` (epoch float → RFC 3339 string) | `updated_at` |
| (computed, see Correlation) | `correlation_keys` |

### Status Mapping

| Codex `turn_status` | `SessionStatus` |
|---|---|
| `pending`, `in_progress` | `Running` |
| `completed` | `Idle` |
| `failed`, `cancelled` | `Idle` |
| anything else / missing | `Idle` |

## Correlation

Each task produces these correlation keys:

1. **Always**: `SessionRef("codex", task_id)`
2. **If PR exists** (task has entries in `pull_requests`):
   - `Branch(pull_request.head)` — the pushed branch (e.g. `codex/review-for-concurrency-issues`)
   - `ChangeRequestRef("github", pr_number_as_string)` — links to our GitHub code review provider
3. **If no PR and source branch is not main/master**: `Branch(task_status_display.branch_name)`

Rules 2 and 3 are mutually exclusive: if a PR exists, use the PR head branch; otherwise fall back to the source branch. Skip branch correlation on `main`/`master` to avoid merging the task into every work item on the default branch.

## CloudAgentService Implementation

```
display_name() -> "Codex"
section_label() -> "Cloud Agents"
item_noun() -> "task"
abbreviation() -> "Cdx"

list_sessions(criteria) -> env lookup + task list + map to CloudAgentSession
archive_session(id) -> Err("not supported") initially
attach_command(id) -> "open https://chatgpt.com/codex/tasks/{id}"
```

`attach_command` returns a shell command using `open` (macOS) to launch the browser. The executor passes this string to the workspace manager as a pane command — it must be a valid shell command, not a bare URL.

## Module Structure

```
crates/flotilla-core/src/providers/coding_agent/
  mod.rs          -- add `pub mod codex;`
  codex.rs        -- CodexCodingAgent implementation
  fixtures/       -- replay YAML fixtures for tests
```

## Testing

- Auth file parsing: valid chatgpt mode, api-key mode, missing file, malformed JSON.
- Environment lookup: by-repo success, by-repo empty (fallback to label matching).
- Task list -> session mapping: status mapping, correlation key extraction, branch filtering, field mapping (title, updated_at conversion).
- PR correlation: task with PR gets Branch + ChangeRequestRef keys; task without PR uses source branch (unless main).
- Auth retry: 401 -> re-read -> retry flow.
- All HTTP tests use replay fixtures via the existing `HttpClient` mock infrastructure.

## Out of Scope

- Token refresh via OAuth (rely on file re-read; codex CLI refreshes the file).
- Task creation/submission from flotilla (#209).
- Manual correlation overrides (#209).
- Archive/delete tasks (API endpoint not yet identified).
- Linux `xdg-open` support for `attach_command` (macOS-only `open` for now, matches existing codebase).
