use std::{collections::BTreeMap, path::Path, sync::Arc};

use axum::{body::to_bytes, extract::State, http::StatusCode, response::IntoResponse, Json, Router};
use flotilla_core::{providers::ProcessCommandRunner, tls};
use flotilla_protocol::LeafAddress;
use serde_json::{json, Value};
use tokio::sync::Mutex;

use super::project_ledger_comment;

#[derive(Default)]
struct Comments {
    items: Vec<Value>,
    writes: Vec<String>,
}

// HTTP boundary stand-in for Forgejo. Contract sources: the instance's
// /swagger.v1.json (CreateIssueCommentOption, EditIssueCommentOption), and
// https://forgejo.org/docs/latest/user/api/usage/ (token auth and pagination).
async fn forgejo(State(state): State<Arc<Mutex<Comments>>>, request: axum::extract::Request) -> axum::response::Response {
    if request.headers().get("authorization").and_then(|value| value.to_str().ok()) != Some("token contract-token") {
        return StatusCode::FORBIDDEN.into_response();
    }
    let method = request.method().clone();
    let path = request.uri().path().to_string();
    let query = request.uri().query().unwrap_or_default().to_string();
    let listing = "/api/v1/repos/acme/repo/issues/42/comments";
    if method == "GET" && path == listing {
        let parameters: BTreeMap<_, _> = url::form_urlencoded::parse(query.as_bytes()).into_owned().collect();
        let page = parameters.get("page").and_then(|value| value.parse::<usize>().ok()).unwrap_or(1);
        let limit = parameters.get("limit").and_then(|value| value.parse::<usize>().ok()).unwrap_or(30);
        if page == 0 || limit == 0 {
            return StatusCode::UNPROCESSABLE_ENTITY.into_response();
        }
        let state = state.lock().await;
        let items: Vec<_> = state.items.iter().skip((page - 1) * limit).take(limit).cloned().collect();
        return (StatusCode::OK, Json(items)).into_response();
    }
    let edit_id = path.strip_prefix("/api/v1/repos/acme/repo/issues/comments/").and_then(|id| id.parse::<u64>().ok());
    if !(method == "POST" && path == listing || method == "PATCH" && edit_id.is_some()) {
        return StatusCode::NOT_FOUND.into_response();
    }
    if request.headers().get("content-type").and_then(|value| value.to_str().ok()) != Some("application/json") {
        return StatusCode::UNPROCESSABLE_ENTITY.into_response();
    }
    let bytes = to_bytes(request.into_body(), 128 * 1024).await.expect("bounded request body");
    let Ok(input) = serde_json::from_slice::<Value>(&bytes) else {
        return StatusCode::UNPROCESSABLE_ENTITY.into_response();
    };
    let Some(body) = input.get("body").and_then(Value::as_str) else {
        return StatusCode::UNPROCESSABLE_ENTITY.into_response();
    };
    let mut state = state.lock().await;
    let (status, comment) = if let Some(id) = edit_id {
        let Some(comment) = state.items.iter_mut().find(|comment| comment["id"] == id) else {
            return StatusCode::NOT_FOUND.into_response();
        };
        comment["body"] = json!(body);
        (StatusCode::OK, comment.clone())
    } else {
        let id = 1 + state.items.iter().filter_map(|comment| comment["id"].as_u64()).max().unwrap_or(0);
        let comment = json!({"id": id, "body": body, "html_url": format!("https://forgejo.example/acme/repo/pulls/42#issuecomment-{id}")});
        state.items.push(comment.clone());
        (StatusCode::CREATED, comment)
    };
    state.writes.push(method.to_string());
    (status, Json(comment)).into_response()
}

// #2600 and the operator's #1512 substitution: real shell/curl requests must
// satisfy Forgejo's auth, JSON, endpoints and success/error status contracts.
#[tokio::test]
async fn forgejo_projection_satisfies_http_comment_contract() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("loopback contract server");
    let api = format!("http://{}/api/v1", listener.local_addr().expect("server address"));
    let state = Arc::new(Mutex::new(Comments::default()));
    let app = Router::new().fallback(forgejo).with_state(Arc::clone(&state));
    let server = tokio::spawn(async move { axum::serve(listener, app).await.expect("contract server") });
    let directory = tempfile::tempdir().expect("credential directory");
    let token = directory.path().join("token");
    std::fs::write(&token, "contract-token").expect("staged test credential");
    let env = BTreeMap::from([("FORGEJO_TOKEN_FILE".into(), token.display().to_string()), ("FORGEJO_API_URL".into(), api.clone())]);
    let address = LeafAddress::ChangeRequest { service: "forgejo.example".into(), scope: "acme/repo".into(), number: 42 };
    let runner = ProcessCommandRunner;
    let first = b"## Decision ledger\nfirst";
    let revised = b"## Decision ledger\nrevised";
    let project = async |body: &[u8]| {
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            project_ledger_comment("contract", "coder", body, &address, &runner, Path::new("/"), &env),
        )
        .await
        .expect("bounded projection")
        .expect("Forgejo contract projection")
    };
    let url = project(first).await;
    assert_eq!(state.lock().await.writes, ["POST"]);
    assert_eq!(project(first).await, url);
    assert_eq!(state.lock().await.writes, ["POST"]);
    assert_eq!(project(revised).await, url);
    assert_eq!(state.lock().await.writes, ["POST", "PATCH"]);
    assert_eq!(project(revised).await, url);
    let older = state.lock().await.items[0].clone();
    let mut newer = older.clone();
    newer["id"] = json!(9);
    newer["html_url"] = json!("https://forgejo.example/acme/repo/pulls/42#issuecomment-9");
    state.lock().await.items.insert(0, newer);
    assert!(project(first).await.ends_with("issuecomment-9"));
    {
        let state = state.lock().await;
        assert_eq!(state.writes, ["POST", "PATCH", "PATCH"]);
        assert_eq!(state.items.len(), 2);
        assert_eq!(state.items[1], older);
        assert!(state.items[0]["body"].as_str().expect("updated body").starts_with("## Decision ledger\nfirst"));
    }
    // Verify the stand-in refuses missing auth, invalid bodies and missing IDs;
    // projection must propagate an authenticated forge refusal rather than POST.
    let client = tls::client_builder().no_proxy().timeout(std::time::Duration::from_secs(5)).build().expect("contract HTTP client");
    let listing = format!("{api}/repos/acme/repo/issues/42/comments");
    assert_eq!(client.get(&listing).send().await.expect("unauthorized request").status(), StatusCode::FORBIDDEN);
    for invalid in [json!({}), json!({"body": 42})] {
        assert_eq!(
            client
                .post(&listing)
                .header("Authorization", "token contract-token")
                .json(&invalid)
                .send()
                .await
                .expect("invalid body")
                .status(),
            StatusCode::UNPROCESSABLE_ENTITY
        );
    }
    assert_eq!(
        client
            .patch(format!("{api}/repos/acme/repo/issues/comments/999"))
            .header("Authorization", "token contract-token")
            .json(&json!({"body":"missing"}))
            .send()
            .await
            .expect("missing comment")
            .status(),
        StatusCode::NOT_FOUND
    );
    std::fs::write(&token, "wrong-token").expect("invalid staged credential");
    let error =
        project_ledger_comment("contract", "coder", first, &address, &runner, Path::new("/"), &env).await.expect_err("auth refusal");
    assert!(error.contains("403"), "{error}");
    assert_eq!(state.lock().await.writes.len(), 3);
    server.abort();
}
