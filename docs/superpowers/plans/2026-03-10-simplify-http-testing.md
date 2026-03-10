# Simplify HTTP Testing Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the custom `ClaudeHttp` abstraction with an `HttpClient` trait that speaks reqwest types, wire it through the replay framework, and migrate claude.rs tests to fixture files.

**Architecture:** Add an `HttpClient` trait (reqwest::Request in, http::Response<Bytes> out) to `providers/mod.rs` alongside `CommandRunner`. Add `ReqwestHttpClient` production impl and `ReplayHttpClient` replay impl. Refactor `ClaudeCodingAgent` to accept `Arc<dyn HttpClient>`, remove all custom HTTP types, and create fixture YAML files for its tests.

**Tech Stack:** Rust, reqwest, http (crate), bytes, async-trait, serde_yml

---

## Chunk 1: HttpClient Trait and Implementations

### Task 1: Add `http` and `bytes` dependencies

**Files:**
- Modify: `crates/flotilla-core/Cargo.toml`

- [ ] **Step 1: Add dependencies**

Add `http` and `bytes` to `[dependencies]` in Cargo.toml. Both are already transitive deps of reqwest so this adds no new downloads:

```toml
http = "1"
bytes = "1"
```

- [ ] **Step 2: Verify it compiles**

Run: `cargo check -p flotilla-core`
Expected: compiles with no errors

- [ ] **Step 3: Commit**

```bash
git add crates/flotilla-core/Cargo.toml
git commit -m "chore: add http and bytes dependencies to flotilla-core"
```

### Task 2: Define `HttpClient` trait and `ReqwestHttpClient`

**Files:**
- Modify: `crates/flotilla-core/src/providers/mod.rs` (add trait + production impl after `CommandRunner`)

- [ ] **Step 1: Add the `HttpClient` trait and `ReqwestHttpClient`**

Add after the `ProcessCommandRunner` impl (around line 96), before `resolve_claude_path`:

```rust
/// Trait abstracting HTTP request execution so providers can be tested
/// without making real network calls.
///
/// Uses reqwest::Request as input (callers build with the reqwest builder API)
/// and returns http::Response<bytes::Bytes> (the standard Rust HTTP type that
/// reqwest is built on, trivially constructable in tests).
#[async_trait]
pub trait HttpClient: Send + Sync {
    async fn execute(
        &self,
        request: reqwest::Request,
    ) -> Result<http::Response<bytes::Bytes>, String>;
}

/// Production implementation that delegates to `reqwest::Client`.
pub struct ReqwestHttpClient {
    client: reqwest::Client,
}

impl ReqwestHttpClient {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }
}

impl Default for ReqwestHttpClient {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl HttpClient for ReqwestHttpClient {
    async fn execute(
        &self,
        request: reqwest::Request,
    ) -> Result<http::Response<bytes::Bytes>, String> {
        let resp = self.client.execute(request).await.map_err(|e| e.to_string())?;
        let status = resp.status();
        let headers = resp.headers().clone();
        let body = resp.bytes().await.map_err(|e| e.to_string())?;
        let mut builder = http::Response::builder().status(status);
        for (name, value) in headers.iter() {
            builder = builder.header(name, value);
        }
        builder.body(body).map_err(|e| e.to_string())
    }
}
```

Add imports at the top of the file: `use async_trait::async_trait;` is already there. No new imports needed since the types are fully qualified.

- [ ] **Step 2: Verify it compiles**

Run: `cargo check -p flotilla-core`
Expected: compiles with no errors

- [ ] **Step 3: Commit**

```bash
git add crates/flotilla-core/src/providers/mod.rs
git commit -m "feat: add HttpClient trait and ReqwestHttpClient implementation"
```

### Task 3: Add `ReplayHttpClient` to replay.rs

**Files:**
- Modify: `crates/flotilla-core/src/providers/replay.rs`

- [ ] **Step 1: Add `ReplayHttpClient` implementing `HttpClient` alongside existing types**

Add the new `ReplayHttpClient` struct, its `HttpClient` impl, the `http_client()` method on `ReplaySession`, and the `test_http_client` factory function. **Keep the existing `HttpResponse`, `ReplayHttp`, and `session.http()` in place for now** — they will be removed in Task 6 after the claude.rs tests are migrated. This preserves compilability at each commit.

Note: `ReplayHttpClient` uses **subset header matching** (only validates headers the fixture specifies), unlike the old `ReplayHttp` which did exact-match on the entire headers map. This is intentional — reqwest may add headers like `accept` that fixtures shouldn't need to specify.

```rust
/// An `HttpClient` implementation that replays canned HTTP interactions
/// from a `ReplaySession`.
pub struct ReplayHttpClient {
    session: ReplaySession,
}

impl ReplayHttpClient {
    pub fn new(session: ReplaySession) -> Self {
        Self { session }
    }
}

#[async_trait]
impl super::HttpClient for ReplayHttpClient {
    async fn execute(
        &self,
        request: reqwest::Request,
    ) -> Result<http::Response<bytes::Bytes>, String> {
        let interaction = self.session.next("http");
        let Interaction::Http {
            method: expected_method,
            url: expected_url,
            request_headers: expected_headers,
            request_body: expected_body,
            status,
            response_body,
            response_headers,
        } = interaction
        else {
            panic!("ReplayHttpClient: expected http interaction");
        };

        // Validate request matches fixture
        assert_eq!(
            request.method().as_str(),
            expected_method,
            "ReplayHttpClient: method mismatch for URL '{}'",
            request.url()
        );
        assert_eq!(
            request.url().as_str(),
            expected_url,
            "ReplayHttpClient: URL mismatch"
        );

        // Validate headers the fixture cares about
        for (key, expected_value) in &expected_headers {
            let actual = request
                .headers()
                .get(key)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            assert_eq!(
                actual, expected_value,
                "ReplayHttpClient: header '{key}' mismatch for '{expected_method} {expected_url}'"
            );
        }

        // Validate body if fixture specifies one
        if let Some(ref expected) = expected_body {
            let actual_body = request
                .body()
                .and_then(|b| b.as_bytes())
                .map(|b| String::from_utf8_lossy(b).to_string())
                .unwrap_or_default();
            assert_eq!(
                actual_body, *expected,
                "ReplayHttpClient: body mismatch for '{expected_method} {expected_url}'"
            );
        }

        // Build response from fixture data
        let mut builder = http::Response::builder().status(status);
        for (key, value) in &response_headers {
            builder = builder.header(key.as_str(), value.as_str());
        }
        builder
            .body(bytes::Bytes::from(response_body))
            .map_err(|e| e.to_string())
    }
}
```

Also add a convenience method on `ReplaySession`:

```rust
/// Create a `ReplayHttpClient` that replays HTTP interactions from this session.
pub fn http_client(&self) -> ReplayHttpClient {
    ReplayHttpClient::new(self.clone())
}
```

And add a `test_http_client` factory function (matching the existing `test_runner` / `test_gh_api` pattern):

```rust
/// Create an `HttpClient` for a test session.
/// In replay mode: returns a `ReplayHttpClient`.
/// (Recording mode for HTTP not yet supported — use RECORD=1 with curl
/// or browser DevTools to capture fixture YAML manually.)
pub fn test_http_client(session: &ReplaySession) -> Arc<dyn super::HttpClient> {
    Arc::new(session.http_client())
}
```

- [ ] **Step 2: Update the `replay_http_request_round_trip` test**

Update the existing test to use the new `ReplayHttpClient` and `HttpClient` trait:

```rust
#[tokio::test]
async fn replay_http_client_round_trip() {
    use crate::providers::HttpClient;

    let yaml = r#"
interactions:
  - channel: http
    method: GET
    url: "https://example.test/v1/sessions"
    request_headers:
      authorization: "Bearer token-1"
      anthropic-version: "2023-06-01"
    status: 200
    response_body: '{"data":[]}'
"#;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("http.yaml");
    std::fs::write(&path, yaml).unwrap();

    let session = ReplaySession::from_file(&path, Masks::new());
    let client = session.http_client();

    let request = reqwest::Client::new()
        .get("https://example.test/v1/sessions")
        .header("authorization", "Bearer token-1")
        .header("anthropic-version", "2023-06-01")
        .build()
        .unwrap();

    let response = client.execute(request).await.expect("replay should work");
    assert_eq!(response.status().as_u16(), 200);
    assert_eq!(response.body().as_ref(), br#"{"data":[]}"#);
    session.assert_complete();
}
```

- [ ] **Step 3: Run tests**

Run: `cargo test -p flotilla-core replay`
Expected: all replay tests pass

- [ ] **Step 4: Commit**

```bash
git add crates/flotilla-core/src/providers/replay.rs
git commit -m "feat: add ReplayHttpClient implementing HttpClient trait"
```

## Chunk 2: Refactor ClaudeCodingAgent

### Task 4: Create fixture YAML files for claude.rs tests

**Files:**
- Create: `crates/flotilla-core/src/providers/coding_agent/fixtures/claude_sessions.yaml`
- Create: `crates/flotilla-core/src/providers/coding_agent/fixtures/claude_auth_retry.yaml`
- Create: `crates/flotilla-core/src/providers/coding_agent/fixtures/claude_auth_failure.yaml`
- Create: `crates/flotilla-core/src/providers/coding_agent/fixtures/claude_archive_ok.yaml`
- Create: `crates/flotilla-core/src/providers/coding_agent/fixtures/claude_archive_fail.yaml`

- [ ] **Step 1: Create fixtures directory**

```bash
mkdir -p crates/flotilla-core/src/providers/coding_agent/fixtures
```

- [ ] **Step 2: Create `claude_sessions.yaml`**

This fixture covers the `fetch_sessions_inner` test — a 200 response with mixed session statuses:

```yaml
interactions:
- channel: http
  method: GET
  url: "https://api.test/v1/sessions"
  request_headers:
    authorization: "Bearer abc123"
    anthropic-beta: "ccr-byoc-2025-07-29"
    anthropic-version: "2023-06-01"
  status: 200
  response_body: >-
    {"data":[
      {"id":"old","title":"Older","session_status":"running",
       "updated_at":"2026-03-01T00:00:00Z",
       "session_context":{"model":"opus","outcomes":[]}},
      {"id":"skip","title":"Archived","session_status":"archived",
       "updated_at":"2026-03-03T00:00:00Z",
       "session_context":{"model":"opus","outcomes":[]}},
      {"id":"new","title":"Newer","session_status":"idle",
       "updated_at":"2026-03-02T00:00:00Z",
       "session_context":{"model":"sonnet","outcomes":[]}}
    ]}
```

- [ ] **Step 3: Create `claude_auth_retry.yaml`**

First request returns 401, second succeeds after token refresh:

```yaml
interactions:
- channel: http
  method: GET
  url: "https://api.test/v1/sessions"
  request_headers:
    authorization: "Bearer expired"
    anthropic-beta: "ccr-byoc-2025-07-29"
    anthropic-version: "2023-06-01"
  status: 401
  response_body: "{}"
- channel: http
  method: GET
  url: "https://api.test/v1/sessions"
  request_headers:
    authorization: "Bearer fresh"
    anthropic-beta: "ccr-byoc-2025-07-29"
    anthropic-version: "2023-06-01"
  status: 200
  response_body: >-
    {"data":[
      {"id":"s1","title":"Recovered","session_status":"running",
       "updated_at":"2026-03-02T00:00:00Z",
       "session_context":{"model":"","outcomes":[]}}
    ]}
```

- [ ] **Step 4: Create `claude_auth_failure.yaml`**

Both requests fail auth — tests graceful degradation to empty list:

```yaml
interactions:
- channel: http
  method: GET
  url: "https://api.test/v1/sessions"
  request_headers:
    authorization: "Bearer bad-1"
    anthropic-beta: "ccr-byoc-2025-07-29"
    anthropic-version: "2023-06-01"
  status: 403
  response_body: "{}"
- channel: http
  method: GET
  url: "https://api.test/v1/sessions"
  request_headers:
    authorization: "Bearer bad-2"
    anthropic-beta: "ccr-byoc-2025-07-29"
    anthropic-version: "2023-06-01"
  status: 401
  response_body: "{}"
```

- [ ] **Step 5: Create `claude_archive_ok.yaml`**

```yaml
interactions:
- channel: http
  method: PATCH
  url: "https://api.test/v1/sessions/s-ok"
  request_headers:
    authorization: "Bearer archive-token"
    anthropic-beta: "ccr-byoc-2025-07-29"
    anthropic-version: "2023-06-01"
  request_body: '{"session_status":"archived"}'
  status: 200
  response_body: "{}"
```

- [ ] **Step 6: Create `claude_archive_fail.yaml`**

```yaml
interactions:
- channel: http
  method: PATCH
  url: "https://api.test/v1/sessions/s-fail"
  request_headers:
    authorization: "Bearer archive-token"
    anthropic-beta: "ccr-byoc-2025-07-29"
    anthropic-version: "2023-06-01"
  request_body: '{"session_status":"archived"}'
  status: 500
  response_body: "boom"
```

- [ ] **Step 7: Commit**

```bash
git add crates/flotilla-core/src/providers/coding_agent/fixtures/
git commit -m "test: add fixture YAML files for claude coding agent tests"
```

### Task 5: Refactor `ClaudeCodingAgent` to use `Arc<dyn HttpClient>`

**Files:**
- Modify: `crates/flotilla-core/src/providers/coding_agent/claude.rs`
- Modify: `crates/flotilla-core/src/providers/discovery.rs:198-202`

- [ ] **Step 1: Remove the `ClaudeHttp` abstraction and inject `HttpClient`**

In `claude.rs`, remove:
- The `HttpResponse` struct (lines 145-148)
- The `ClaudeHttp` trait (lines 150-159)
- The `ReqwestClaudeHttp` struct and impl (lines 161-187)

Change `ClaudeCodingAgent` to store an `Arc<dyn super::super::HttpClient>`:

```rust
pub struct ClaudeCodingAgent {
    provider_name: String,
    runner: Arc<dyn CommandRunner>,
    http: Arc<dyn super::super::HttpClient>,
    reqwest_client: reqwest::Client,
    sessions_cache: Mutex<SessionsCache>,
}

impl ClaudeCodingAgent {
    pub fn new(
        provider_name: String,
        runner: Arc<dyn CommandRunner>,
        http: Arc<dyn super::super::HttpClient>,
    ) -> Self {
        Self {
            provider_name,
            runner,
            http,
            reqwest_client: reqwest::Client::new(),
            sessions_cache: Mutex::new(SessionsCache {
                sessions: Vec::new(),
                fetched_at: None,
                known_ids: std::collections::HashSet::new(),
            }),
        }
    }
}
```

- [ ] **Step 2: Refactor `fetch_sessions` to use `self.http` directly**

Remove the `_with_http` and `_with_base` method chain. Replace with methods that use `self.http` and accept `base_url` for testability:

```rust
impl ClaudeCodingAgent {
    fn build_request(
        client: &reqwest::Client,
        method: &str,
        url: &str,
        access_token: &str,
        json_body: Option<serde_json::Value>,
    ) -> Result<reqwest::Request, String> {
        let method = reqwest::Method::from_bytes(method.as_bytes())
            .map_err(|e| format!("invalid HTTP method: {e}"))?;
        let mut builder = client
            .request(method, url)
            .header("authorization", format!("Bearer {access_token}"))
            .header("anthropic-beta", "ccr-byoc-2025-07-29")
            .header("anthropic-version", "2023-06-01");
        if let Some(body) = json_body {
            builder = builder.json(&body);
        }
        builder.build().map_err(|e| e.to_string())
    }

    async fn fetch_sessions(&self, base_url: &str) -> Result<Vec<WebSession>, String> {
        match self.fetch_sessions_inner(base_url).await {
            Ok(sessions) => Ok(sessions),
            Err(e) if e.contains("authentication") || e.contains("missing field `data`") => {
                debug!("session fetch failed, clearing auth cache and retrying: {e}");
                invalidate_auth_cache();
                match self.fetch_sessions_inner(base_url).await {
                    Ok(sessions) => Ok(sessions),
                    Err(e) if e.contains("authentication") => {
                        if !AUTH_WARNED.swap(true, Ordering::Relaxed) {
                            warn!("Claude sessions unavailable: insufficient OAuth scopes");
                        }
                        debug!("Claude auth error detail: {e}");
                        Ok(vec![])
                    }
                    Err(e) => Err(e),
                }
            }
            Err(e) => Err(e),
        }
    }

    async fn fetch_sessions_inner(&self, base_url: &str) -> Result<Vec<WebSession>, String> {
        let token = get_oauth_token(&*self.runner).await?;
        let request = Self::build_request(
            &self.reqwest_client,
            "GET",
            &sessions_url_for(base_url),
            &token.access_token,
            None,
        )?;
        let resp = self.http.execute(request).await?;
        let status = resp.status().as_u16();
        let body = std::str::from_utf8(resp.body()).map_err(|e| e.to_string())?;

        if status == 401 || status == 403 {
            return Err(format!("authentication error (HTTP {status})"));
        }
        if !(200..300).contains(&status) {
            return Err(format!("session fetch failed (HTTP {status}): {body}"));
        }

        let parsed: SessionsResponse =
            serde_json::from_str(body).map_err(|e| format!("session parse error: {e}"))?;

        let mut sessions: Vec<WebSession> = parsed
            .data
            .into_iter()
            .filter(|s| s.session_status != "archived")
            .collect();
        sessions.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        Ok(sessions)
    }

    async fn archive_session_inner(
        &self,
        session_id: &str,
        base_url: &str,
    ) -> Result<(), String> {
        info!("archiving session {session_id}");
        let token = get_oauth_token(&*self.runner).await?;
        let request = Self::build_request(
            &self.reqwest_client,
            "PATCH",
            &session_url_for(base_url, session_id),
            &token.access_token,
            Some(serde_json::json!({"session_status": "archived"})),
        )?;
        let resp = self.http.execute(request).await?;
        let status = resp.status().as_u16();
        if (200..300).contains(&status) {
            Ok(())
        } else {
            let body = std::str::from_utf8(resp.body()).unwrap_or("<binary>");
            Err(format!("archive session failed (HTTP {status}): {body}"))
        }
    }
}
```

- [ ] **Step 3: Update the `CodingAgent` trait implementation**

```rust
#[async_trait]
impl super::CodingAgent for ClaudeCodingAgent {
    // ... display_name stays the same ...

    async fn list_sessions(
        &self,
        criteria: &RepoCriteria,
    ) -> Result<Vec<(String, CloudAgentSession)>, String> {
        // ... cache logic stays the same, but change:
        // Self::fetch_sessions(&*self.runner).await?
        // to:
        // self.fetch_sessions(CLAUDE_API_BASE_URL).await?
        // ... rest stays the same ...
    }

    async fn archive_session(&self, session_id: &str) -> Result<(), String> {
        self.archive_session_inner(session_id, CLAUDE_API_BASE_URL).await
    }

    // attach_command stays the same
}
```

- [ ] **Step 4: Update discovery.rs wiring**

At `discovery.rs:198-202`, add the `HttpClient` argument:

```rust
        registry.coding_agents.insert(
            "claude".to_string(),
            Arc::new(ClaudeCodingAgent::new(
                "claude".to_string(),
                Arc::clone(&runner),
                Arc::new(crate::providers::ReqwestHttpClient::new()),
            )),
        );
```

- [ ] **Step 5: Verify compilation**

Run: `cargo check -p flotilla-core`
Expected: compiles (tests may not pass yet — fixture data may need tuning)

- [ ] **Step 6: Commit**

```bash
git add crates/flotilla-core/src/providers/coding_agent/claude.rs
git add crates/flotilla-core/src/providers/discovery.rs
git commit -m "refactor: replace ClaudeHttp with injected HttpClient trait"
```

### Task 6: Rewrite claude.rs tests to use fixture files and `ReplayHttpClient`

**Files:**
- Modify: `crates/flotilla-core/src/providers/coding_agent/claude.rs` (test module)

- [ ] **Step 1: Rewrite the test module**

Replace the entire `#[cfg(test)] mod tests` block. Remove:
- `ReplayClaudeHttp` struct and impl
- `http_interaction()` helper
- `interaction_headers()` helper
- All in-memory `replay::Interaction` construction

Also in `replay.rs`, now remove the old types that were kept for compilation in Task 3:
- `HttpResponse` struct (lines 52-57)
- `ReplayHttp` struct and its `impl` block (lines 383-437)
- `session.http()` method (lines 226-229)
- The old `replay_http_request_round_trip` test (replaced by `replay_http_client_round_trip` in Task 3)

Replace with tests that follow the fixture pattern used by other providers:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::coding_agent::CodingAgent;
    use crate::providers::replay;
    use std::sync::Arc;
    use tokio::sync::Mutex as AsyncMutex;

    static TEST_LOCK: LazyLock<AsyncMutex<()>> = LazyLock::new(|| AsyncMutex::new(()));

    fn fixture(name: &str) -> String {
        format!(
            "{}/src/providers/coding_agent/fixtures/{}",
            env!("CARGO_MANIFEST_DIR"),
            name
        )
    }

    fn now_epoch_secs() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
    }

    fn token_json(access_token: &str, expires_at: i64) -> String {
        serde_json::json!({
            "claudeAiOauth": {
                "accessToken": access_token,
                "expiresAt": expires_at
            }
        })
        .to_string()
    }

    fn reset_auth_state() {
        invalidate_auth_cache();
        AUTH_WARNED.store(false, Ordering::Relaxed);
    }

    fn mock_runner(responses: Vec<Result<String, String>>) -> Arc<dyn CommandRunner> {
        Arc::new(crate::providers::testing::MockRunner::new(responses))
    }

    fn make_agent(
        runner: Arc<dyn CommandRunner>,
        http: Arc<dyn crate::providers::HttpClient>,
    ) -> ClaudeCodingAgent {
        ClaudeCodingAgent::new("claude".into(), runner, http)
    }

    #[tokio::test]
    async fn oauth_token_is_cached_until_near_expiry() {
        let _test_lock = TEST_LOCK.lock().await;
        reset_auth_state();
        let runner = crate::providers::testing::MockRunner::new(vec![
            Ok(token_json("token-1", now_epoch_secs() + 3600)),
        ]);
        let token1 = get_oauth_token(&runner).await.expect("first token");
        let token2 = get_oauth_token(&runner).await.expect("cached token");
        assert_eq!(token1.access_token, "token-1");
        assert_eq!(token2.access_token, "token-1");
    }

    #[tokio::test]
    async fn oauth_token_refreshes_when_expiring_soon() {
        let _test_lock = TEST_LOCK.lock().await;
        reset_auth_state();
        let runner = crate::providers::testing::MockRunner::new(vec![
            Ok(token_json("old-token", now_epoch_secs() + 10)),
            Ok(token_json("new-token", now_epoch_secs() + 3600)),
        ]);
        let first = get_oauth_token(&runner).await.expect("first token");
        let second = get_oauth_token(&runner).await.expect("refreshed token");
        assert_eq!(first.access_token, "old-token");
        assert_eq!(second.access_token, "new-token");
    }

    #[tokio::test]
    async fn fetch_sessions_inner_filters_archived_sorts_and_sends_auth_header() {
        let _test_lock = TEST_LOCK.lock().await;
        reset_auth_state();

        let runner = mock_runner(vec![
            Ok(token_json("abc123", now_epoch_secs() + 3600)),
        ]);
        let session = replay::ReplaySession::from_file(
            &fixture("claude_sessions.yaml"),
            replay::Masks::new(),
        );
        let http = replay::test_http_client(&session);
        let agent = make_agent(runner, http);

        let sessions = agent.fetch_sessions_inner("https://api.test")
            .await
            .expect("fetch sessions");
        session.assert_complete();

        assert_eq!(sessions.len(), 2, "archived sessions should be filtered");
        assert_eq!(sessions[0].id, "new", "sessions should be sorted desc");
        assert_eq!(sessions[1].id, "old");
    }

    #[tokio::test]
    async fn fetch_sessions_retries_after_auth_error() {
        let _test_lock = TEST_LOCK.lock().await;
        reset_auth_state();

        let runner = mock_runner(vec![
            Ok(token_json("expired", now_epoch_secs() + 3600)),
            Ok(token_json("fresh", now_epoch_secs() + 3600)),
        ]);
        let session = replay::ReplaySession::from_file(
            &fixture("claude_auth_retry.yaml"),
            replay::Masks::new(),
        );
        let http = replay::test_http_client(&session);
        let agent = make_agent(runner, http);

        let sessions = agent.fetch_sessions("https://api.test")
            .await
            .expect("retry should succeed");
        session.assert_complete();

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "s1");
    }

    #[tokio::test]
    async fn fetch_sessions_returns_empty_after_second_auth_error() {
        let _test_lock = TEST_LOCK.lock().await;
        reset_auth_state();

        let runner = mock_runner(vec![
            Ok(token_json("bad-1", now_epoch_secs() + 3600)),
            Ok(token_json("bad-2", now_epoch_secs() + 3600)),
        ]);
        let session = replay::ReplaySession::from_file(
            &fixture("claude_auth_failure.yaml"),
            replay::Masks::new(),
        );
        let http = replay::test_http_client(&session);
        let agent = make_agent(runner, http);

        let sessions = agent.fetch_sessions("https://api.test")
            .await
            .expect("auth failures should degrade gracefully");
        session.assert_complete();

        assert!(sessions.is_empty());
    }

    #[tokio::test]
    async fn list_sessions_uses_cache_and_maps_fields() {
        // This test doesn't hit HTTP — it pre-populates the cache.
        let _test_lock = TEST_LOCK.lock().await;
        reset_auth_state();

        let runner = mock_runner(vec![]);
        // http won't be called — cache is pre-populated
        let http: Arc<dyn crate::providers::HttpClient> =
            Arc::new(crate::providers::ReqwestHttpClient::new());
        let agent = make_agent(runner, http);
        {
            let mut cache = agent.sessions_cache.lock().unwrap();
            cache.sessions = vec![
                WebSession {
                    id: "one".into(),
                    title: "One".into(),
                    session_status: "running".into(),
                    created_at: String::new(),
                    updated_at: "2026-03-05T00:00:00Z".into(),
                    session_context: SessionContext {
                        model: "sonnet".into(),
                        outcomes: vec![SessionOutcome {
                            git_info: Some(SessionGitInfo {
                                branches: vec!["refs/heads/feat-a".into()],
                                repo: Some("owner/repo".into()),
                            }),
                        }],
                    },
                },
                WebSession {
                    id: "two".into(),
                    title: "Two".into(),
                    session_status: "something-else".into(),
                    created_at: String::new(),
                    updated_at: "2026-03-04T00:00:00Z".into(),
                    session_context: SessionContext {
                        model: String::new(),
                        outcomes: vec![SessionOutcome { git_info: None }],
                    },
                },
                WebSession {
                    id: "skip".into(),
                    title: "Skip".into(),
                    session_status: "running".into(),
                    created_at: String::new(),
                    updated_at: "2026-03-03T00:00:00Z".into(),
                    session_context: SessionContext {
                        model: "opus".into(),
                        outcomes: vec![SessionOutcome {
                            git_info: Some(SessionGitInfo {
                                branches: vec!["refs/heads/feat-b".into()],
                                repo: Some("other/repo".into()),
                            }),
                        }],
                    },
                },
            ];
            cache.fetched_at = Some(Instant::now());
        }

        let sessions = agent
            .list_sessions(&RepoCriteria {
                repo_slug: Some("owner/repo".into()),
            })
            .await
            .expect("list sessions");

        assert_eq!(sessions.len(), 2);
        let one = sessions
            .iter()
            .find(|(id, _)| id == "one")
            .expect("one session");
        assert_eq!(one.1.status, SessionStatus::Running);
        assert_eq!(one.1.model.as_deref(), Some("sonnet"));
        assert!(one
            .1
            .correlation_keys
            .contains(&CorrelationKey::Branch("feat-a".into())));

        let two = sessions
            .iter()
            .find(|(id, _)| id == "two")
            .expect("two session");
        assert_eq!(two.1.status, SessionStatus::Idle);
        assert!(two.1.model.is_none());
    }

    #[tokio::test]
    async fn archive_session_succeeds() {
        let _test_lock = TEST_LOCK.lock().await;
        reset_auth_state();

        let runner = mock_runner(vec![
            Ok(token_json("archive-token", now_epoch_secs() + 3600)),
        ]);
        let session = replay::ReplaySession::from_file(
            &fixture("claude_archive_ok.yaml"),
            replay::Masks::new(),
        );
        let http = replay::test_http_client(&session);
        let agent = make_agent(runner, http);

        agent.archive_session_inner("s-ok", "https://api.test")
            .await
            .expect("archive should succeed");
        session.assert_complete();
    }

    #[tokio::test]
    async fn archive_session_returns_error_on_failure() {
        let _test_lock = TEST_LOCK.lock().await;
        reset_auth_state();

        let runner = mock_runner(vec![
            Ok(token_json("archive-token", now_epoch_secs() + 3600)),
        ]);
        let session = replay::ReplaySession::from_file(
            &fixture("claude_archive_fail.yaml"),
            replay::Masks::new(),
        );
        let http = replay::test_http_client(&session);
        let agent = make_agent(runner, http);

        let err = agent.archive_session_inner("s-fail", "https://api.test")
            .await
            .expect_err("archive should fail");
        session.assert_complete();

        assert!(err.contains("HTTP 500"));
        assert!(err.contains("boom"));
    }

    #[tokio::test]
    async fn attach_command_formats_teleport_command() {
        let _test_lock = TEST_LOCK.lock().await;
        let runner = mock_runner(vec![]);
        let http: Arc<dyn crate::providers::HttpClient> =
            Arc::new(crate::providers::ReqwestHttpClient::new());
        let agent = make_agent(runner, http);
        let cmd = agent.attach_command("abc123").await.expect("attach command");
        assert_eq!(cmd, "claude --teleport abc123");
    }
}
```

- [ ] **Step 2: Run all tests**

Run: `cargo test -p flotilla-core`
Expected: all tests pass

- [ ] **Step 3: Run clippy**

Run: `cargo clippy -p flotilla-core --all-targets --locked -- -D warnings`
Expected: no warnings

- [ ] **Step 4: Commit**

```bash
git add crates/flotilla-core/src/providers/coding_agent/claude.rs crates/flotilla-core/src/providers/replay.rs
git commit -m "test: rewrite claude tests to use fixture files and HttpClient"
```

## Chunk 3: Cleanup and Documentation

> **Prerequisite:** Tasks 1-6 (Chunks 1-2) must be complete before starting Chunk 3.

### Task 7: Update the examples/list_sessions.rs if it exists

**Files:**
- Modify: `crates/flotilla-core/examples/list_sessions.rs`

- [ ] **Step 1: Update constructor call**

Add `use flotilla_core::providers::ReqwestHttpClient;` to the imports, then add `Arc::new(ReqwestHttpClient::new())` as the third argument to `ClaudeCodingAgent::new`.

- [ ] **Step 2: Verify it compiles**

Run: `cargo check -p flotilla-core --examples`

- [ ] **Step 3: Commit**

```bash
git add crates/flotilla-core/examples/list_sessions.rs
git commit -m "chore: update list_sessions example for HttpClient parameter"
```

### Task 8: Add record/replay usage guide to AGENTS.md

**Files:**
- Modify: `AGENTS.md` (or create a section if it doesn't cover testing)

- [ ] **Step 1: Add testing section**

Add a `## Testing Providers with Record/Replay` section to AGENTS.md. The content should cover:

1. **Pattern** — the 5-step workflow: create fixture files, load replay session, get replay implementations (`test_runner`, `test_gh_api`, `test_http_client`), inject into provider, assert + `session.assert_complete()`

2. **Fixture format** — show examples of all three channel types: `command`, `gh_api`, `http`. Note that `{repo}` in `cwd` fields is a mask placeholder (see Masks section).

3. **Recording** — `RECORD=1 cargo test -p flotilla-core test_name` captures real interactions for `CommandRunner` and `GhApi`. **Note: HTTP recording is not yet supported** — create HTTP fixtures manually or capture with curl/DevTools and format as YAML.

4. **Masks** — show `Masks::new()` / `masks.add()` pattern, note longer values must be added before shorter prefixes.

- [ ] **Step 2: Commit**

```bash
git add AGENTS.md
git commit -m "docs: add record/replay testing guide for agents"
```

### Task 9: Final verification

- [ ] **Step 1: Run full test suite**

Run: `cargo test --locked`
Expected: all tests pass

- [ ] **Step 2: Run clippy**

Run: `cargo clippy --all-targets --locked -- -D warnings`
Expected: no warnings

- [ ] **Step 3: Run fmt**

Run: `cargo fmt` then `cargo fmt --check`
Expected: no formatting changes needed (or applied automatically)
