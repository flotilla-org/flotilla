//! Runtime integration tests: the built Worker under a local Workers runtime (`wrangler dev`).
//!
//! Needs `wrangler` and `worker-build` on PATH (override the former with `WRANGLER`), so it is
//! ignored by default. Run with:
//!
//! ```text
//! cargo test -p flotilla-relay --test runtime --locked -- --ignored
//! ```
#![cfg(not(target_arch = "wasm32"))]

use std::{
    net::TcpListener,
    path::Path,
    process::Stdio,
    time::{Duration, Instant},
};

use flotilla_relay_protocol::{
    admin::{InstallCreated, InstallDescription, IssuedSecret, IssuedToken},
    github, Delivery, StreamFrame,
};
use futures::{SinkExt, StreamExt};
use reqwest::{Method, StatusCode};
use serde_json::json;
use tokio::{
    net::TcpStream,
    process::{Child, Command},
};
use tokio_tungstenite::{
    tungstenite::{client::IntoClientRequest, Message},
    MaybeTlsStream, WebSocketStream,
};

const OPERATOR_TOKEN: &str = "runtime-test-operator-token";
const SUBJECT_CAP: u64 = 3;

struct Relay {
    base: String,
    http: reqwest::Client,
    child: Child,
    log: tempfile::NamedTempFile,
    _state: tempfile::TempDir,
}

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

fn operator_digest() -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(OPERATOR_TOKEN.as_bytes()))
}

impl Drop for Relay {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!("--- wrangler dev log ---\n{}", self.log());
        }
        // wrangler runs workerd and esbuild as children; stop the whole process group.
        if let Some(pid) = self.child.id() {
            let _ = std::process::Command::new("kill").args(["-KILL", &format!("-{pid}")]).status();
        }
    }
}

impl Relay {
    async fn start() -> Self {
        // The workspace's reqwest leaves the Rustls provider to the binary.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        // Build once up front. Left to itself, `wrangler dev` rebuilds and restarts whenever it
        // sees `src` change, which races the first requests.
        let build =
            Command::new("worker-build").arg("--release").current_dir(crate_dir).output().await.expect("run worker-build (is it on PATH?)");
        assert!(build.status.success(), "worker-build failed:\n{}", String::from_utf8_lossy(&build.stderr));

        // wrangler watches `main`. On macOS, file events for the build output just written can
        // reach its watcher after it starts, and the resulting reload kills in-flight requests.
        // Letting the output settle first stopped every such reload in repeated runs.
        tokio::time::sleep(Duration::from_secs(3)).await;
        let state = tempfile::tempdir().expect("state dir");
        let config = state.path().join("wrangler.toml");
        std::fs::write(&config, prebuilt_config(crate_dir)).expect("write wrangler config");
        let port = TcpListener::bind("127.0.0.1:0").expect("free port").local_addr().expect("address").port();
        let log = tempfile::NamedTempFile::new().expect("log file");
        let wrangler = std::env::var("WRANGLER").unwrap_or_else(|_| "wrangler".into());
        let child = Command::new(wrangler)
            .current_dir(state.path())
            .args(["dev", "--local", "--ip", "127.0.0.1", "--port", &port.to_string(), "--show-interactive-dev-session=false"])
            .arg("--config")
            .arg(&config)
            .arg("--persist-to")
            .arg(state.path().join("state"))
            .args(["--var", &format!("RELAY_OPERATOR_TOKEN_SHA256:{}", operator_digest())])
            .args(["--var", &format!("RELAY_SUBJECT_CAP:{SUBJECT_CAP}")])
            .stdin(Stdio::null())
            .stdout(log.reopen().expect("log handle"))
            .stderr(log.reopen().expect("log handle"))
            .process_group(0)
            .kill_on_drop(true)
            .spawn()
            .expect("spawn wrangler dev (is wrangler on PATH?)");
        let relay = Self { base: format!("127.0.0.1:{port}"), http: reqwest::Client::new(), child, log, _state: state };
        let deadline = Instant::now() + Duration::from_secs(120);
        // Our Worker answers `/` with 404; the runtime's proxy answers 503 while it (re)loads.
        while !matches!(relay.http.get(relay.url("/")).send().await, Ok(response) if response.status() == StatusCode::NOT_FOUND) {
            assert!(Instant::now() < deadline, "wrangler dev did not start:\n{}", relay.log());
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        relay
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.log.path()).unwrap_or_default()
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.base)
    }

    async fn request(&self, method: Method, path: &str, token: &str, body: Option<String>) -> (StatusCode, String) {
        let mut request = self.http.request(method, self.url(path)).bearer_auth(token);
        if let Some(body) = body {
            request = request.body(body);
        }
        let response = request.send().await.expect("request");
        (response.status(), response.text().await.expect("body"))
    }

    async fn admin<T: serde::de::DeserializeOwned>(&self, method: Method, path: &str, body: Option<String>) -> T {
        let (status, body) = self.request(method, &format!("/admin/installs/{path}"), OPERATOR_TOKEN, body).await;
        assert!(status.is_success(), "admin {path}: {status} {body}");
        serde_json::from_str(&body).expect("admin JSON")
    }

    /// Creates an install with a GitHub secret; returns its consumer token and the secret.
    async fn provision(&self, install: &str) -> (String, String) {
        let created: InstallCreated = self.admin(Method::POST, install, None).await;
        let secret: IssuedSecret = self.admin(Method::POST, &format!("{install}/sources/github/secrets"), None).await;
        (created.consumer_token.token, secret.secret)
    }

    async fn deliver(&self, install: &str, secret: &str, event: &str, delivery: &str, payload: &[u8]) -> (StatusCode, String) {
        let response = self
            .http
            .post(self.url(&format!("/i/{install}/github")))
            .header(github::SIGNATURE_HEADER, github::signature(secret, payload))
            .header(github::EVENT_HEADER, event)
            .header(github::DELIVERY_HEADER, delivery)
            .body(payload.to_vec())
            .send()
            .await
            .expect("deliver");
        (response.status(), response.text().await.expect("body"))
    }

    /// Sends a body one byte over GitHub's maximum; returns the status.
    ///
    /// The local runtime's proxy reads the whole body before the Worker sees the request, so
    /// this checks the refusal, not that it happens before the body is read.
    async fn oversized_delivery_status(&self, install: &str) -> StatusCode {
        let response = self
            .http
            .post(self.url(&format!("/i/{install}/github")))
            .header(github::SIGNATURE_HEADER, "sha256=00")
            .body(vec![b' '; github::MAX_PAYLOAD_BYTES + 1])
            .send()
            .await
            .expect("oversized delivery");
        response.status()
    }

    async fn deliver_pr(&self, install: &str, secret: &str, number: u64, delivery: &str) -> Vec<Delivery> {
        let payload = pr_payload(number);
        let (status, body) = self.deliver(install, secret, "pull_request", delivery, &payload).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        serde_json::from_str(&body).expect("deliveries")
    }

    async fn poll(&self, install: &str, token: &str, cursor: u64, wait: u64) -> Vec<StreamFrame> {
        let (status, body) = self.request(Method::GET, &format!("/i/{install}/stream?cursor={cursor}&wait={wait}"), token, None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        serde_json::from_str(&body).expect("frames")
    }

    async fn connect(&self, install: &str, token: &str, cursor: u64) -> Socket {
        let mut request = format!("ws://{}/i/{install}/stream?cursor={cursor}", self.base).into_client_request().expect("ws request");
        request.headers_mut().insert("Authorization", format!("Bearer {token}").parse().expect("header"));
        tokio_tungstenite::connect_async(request).await.expect("websocket connect").0
    }
}

/// The crate's `wrangler.toml` with the build step removed and `main` pointing at the
/// prebuilt shim, so the runtime serves exactly what `worker-build` produced.
fn prebuilt_config(crate_dir: &Path) -> String {
    let source = std::fs::read_to_string(crate_dir.join("wrangler.toml")).expect("read wrangler.toml");
    let main = format!("main = {:?}", crate_dir.join("build/worker/shim.mjs").display().to_string());
    let mut in_build = false;
    let config: Vec<String> = source
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if trimmed.starts_with('[') {
                in_build = trimmed == "[build]";
            }
            match () {
                _ if in_build => None,
                _ if trimmed.starts_with("main =") => Some(main.clone()),
                _ => Some(line.to_owned()),
            }
        })
        .collect();
    assert!(config.contains(&main), "wrangler.toml declares main");
    assert!(!config.iter().any(|line| line.starts_with("command =")), "build section removed");
    config.join("\n")
}

fn pr_payload(number: u64) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "action": "synchronize",
        "repository": { "name": "Hello-World", "owner": { "login": "Codertocat" } },
        "pull_request": { "number": number },
    }))
    .expect("payload")
}

fn subject(number: u64) -> String {
    format!("cr/github.com/codertocat/hello-world/{number}")
}

async fn next_frame(socket: &mut Socket) -> StreamFrame {
    loop {
        let message =
            tokio::time::timeout(Duration::from_secs(10), socket.next()).await.expect("frame within 10s").expect("open").expect("frame");
        match message {
            Message::Text(text) => return serde_json::from_str(&text).expect("stream frame"),
            Message::Ping(_) | Message::Pong(_) => continue,
            other => panic!("unexpected websocket message {other:?}"),
        }
    }
}

fn hint_subjects(frames: &[StreamFrame]) -> Vec<(u64, String)> {
    frames
        .iter()
        .map(|frame| match frame {
            StreamFrame::Hint { delivery } => (delivery.cursor, delivery.hint.subject.clone()),
            other => panic!("expected hint, got {other:?}"),
        })
        .collect()
}

#[tokio::test]
#[ignore = "needs wrangler and worker-build; run with --ignored"]
async fn worker_runtime_contract() {
    let relay = Relay::start().await;
    authentication_failures_are_uniform(&relay).await;
    signed_ingress_appends_and_coalesces(&relay).await;
    websocket_replays_then_broadcasts(&relay).await;
    long_poll_wakes_on_append_and_times_out(&relay).await;
    ack_over_http(&relay).await;
    pruned_cursor_gets_gap(&relay).await;
    rotation_and_deprovisioning(&relay).await;
}

async fn authentication_failures_are_uniform(relay: &Relay) {
    let (token, secret) = relay.provision("auth").await;
    let admin_denied = relay.request(Method::POST, "/admin/installs/other", "wrong-operator", None).await;
    assert_eq!(admin_denied.0, StatusCode::UNAUTHORIZED);

    // Unknown install and bad token are indistinguishable.
    let unknown = relay.request(Method::GET, "/i/no-such-install/stream?cursor=0", &token, None).await;
    let bad_token = relay.request(Method::GET, "/i/auth/stream?cursor=0", "wrong-token", None).await;
    assert_eq!(unknown.0, StatusCode::UNAUTHORIZED);
    assert_eq!(unknown, bad_token);
    let unknown_ack =
        relay.request(Method::POST, "/i/no-such-install/stream/ack", &token, Some(r#"{"type":"ack","cursor":0}"#.into())).await;
    assert_eq!(unknown_ack, bad_token);

    // Unknown install, wrong secret, and a source with no secret configured are indistinguishable.
    let payload = pr_payload(1);
    let unknown = relay.deliver("no-such-install", &secret, "pull_request", "d", &payload).await;
    let wrong_secret = relay.deliver("auth", "not-the-secret", "pull_request", "d", &payload).await;
    assert_eq!(unknown.0, StatusCode::UNAUTHORIZED, "{unknown:?}");
    assert_eq!(unknown, wrong_secret);
    let _: serde_json::Value = relay.admin(Method::POST, "unsourced", None).await;
    assert_eq!(relay.deliver("unsourced", &secret, "pull_request", "d", &payload).await, wrong_secret);

    // A source without an adapter is unknown to the relay, whatever the install.
    let gitlab = relay.http.post(relay.url("/i/auth/gitlab")).body("{}").send().await.expect("post");
    assert_eq!(gitlab.status(), StatusCode::NOT_FOUND);
    assert_eq!(relay.oversized_delivery_status("auth").await, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(relay.poll("auth", &token, 0, 0).await, [], "rejected deliveries record nothing");
}

async fn signed_ingress_appends_and_coalesces(relay: &Relay) {
    let (token, secret) = relay.provision("ingress").await;
    let fixture = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/check_run.json")).expect("fixture");
    let mut last = 0;
    for delivery in 0..5 {
        let (status, body) = relay.deliver("ingress", &secret, "check_run", &format!("d{delivery}"), &fixture).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let deliveries: Vec<Delivery> = serde_json::from_str(&body).expect("deliveries");
        last = deliveries[0].cursor;
    }
    // A redelivery of the latest delivery is accepted and dropped.
    let (status, body) = relay.deliver("ingress", &secret, "check_run", "d4", &fixture).await;
    assert_eq!((status, body.as_str()), (StatusCode::OK, "[]"));
    let (status, body) = relay.deliver("ingress", &secret, "push", "p", b"{}").await;
    assert_eq!((status, body.as_str()), (StatusCode::OK, "[]"), "unhandled events are accepted and ignored");
    assert_eq!(hint_subjects(&relay.poll("ingress", &token, 0, 0).await), [(last, subject(2))], "a burst occupies one entry");
    let description: InstallDescription = relay.admin(Method::GET, "ingress", None).await;
    assert_eq!((description.latest_cursor, description.retained_subjects), (5, 1));
}

async fn websocket_replays_then_broadcasts(relay: &Relay) {
    let (token, secret) = relay.provision("websocket").await;
    relay.deliver_pr("websocket", &secret, 1, "a").await;
    relay.deliver_pr("websocket", &secret, 2, "b").await;

    let mut socket = relay.connect("websocket", &token, 0).await;
    assert_eq!(hint_subjects(&[next_frame(&mut socket).await, next_frame(&mut socket).await]), [(1, subject(1)), (2, subject(2))]);
    relay.deliver_pr("websocket", &secret, 1, "c").await;
    assert_eq!(hint_subjects(&[next_frame(&mut socket).await]), [(3, subject(1))], "live broadcast");

    socket.send(Message::text(r#"{"type":"ack","cursor":3}"#)).await.expect("send ack");
    assert_eq!(next_frame(&mut socket).await, StreamFrame::Acked { cursor: 3 });
    socket.send(Message::text(r#"{"type":"ack","cursor":9}"#)).await.expect("send ack");
    assert!(matches!(next_frame(&mut socket).await, StreamFrame::Error { .. }));

    let mut caught_up = relay.connect("websocket", &token, 3).await;
    assert_eq!(next_frame(&mut caught_up).await, StreamFrame::Ready { cursor: 3 });
    let mut ahead = relay.connect("websocket", &token, 4).await;
    assert!(matches!(next_frame(&mut ahead).await, StreamFrame::Error { .. }));
}

async fn long_poll_wakes_on_append_and_times_out(relay: &Relay) {
    let (token, secret) = relay.provision("longpoll").await;
    let started = Instant::now();
    assert_eq!(relay.poll("longpoll", &token, 0, 1).await, [], "empty poll times out empty");
    assert!(started.elapsed() >= Duration::from_millis(900), "poll held for its wait");

    let started = Instant::now();
    let (frames, _) = tokio::join!(relay.poll("longpoll", &token, 0, 20), async {
        tokio::time::sleep(Duration::from_millis(500)).await;
        relay.deliver_pr("longpoll", &secret, 7, "x").await
    });
    assert_eq!(hint_subjects(&frames), [(1, subject(7))]);
    assert!(started.elapsed() < Duration::from_secs(10), "append woke the poll ({:?})", started.elapsed());
}

async fn ack_over_http(relay: &Relay) {
    let (token, secret) = relay.provision("ack").await;
    relay.deliver_pr("ack", &secret, 1, "a").await;
    let ack =
        |cursor: u64| relay.request(Method::POST, "/i/ack/stream/ack", &token, Some(format!(r#"{{"type":"ack","cursor":{cursor}}}"#)));
    let (status, body) = ack(1).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(serde_json::from_str::<StreamFrame>(&body).expect("frame"), StreamFrame::Acked { cursor: 1 });
    assert_eq!(ack(2).await.0, StatusCode::BAD_REQUEST);
}

async fn pruned_cursor_gets_gap(relay: &Relay) {
    let (token, secret) = relay.provision("gap").await;
    for number in 1..=SUBJECT_CAP + 1 {
        relay.deliver_pr("gap", &secret, number, "d").await;
    }
    let latest = SUBJECT_CAP + 1;
    assert_eq!(relay.poll("gap", &token, 0, 0).await, [StreamFrame::Gap { oldest_cursor: Some(2), latest_cursor: latest }]);
    let mut socket = relay.connect("gap", &token, 0).await;
    assert!(matches!(next_frame(&mut socket).await, StreamFrame::Gap { .. }));
    assert_eq!(relay.poll("gap", &token, 1, 0).await.len() as u64, SUBJECT_CAP, "cursor 1 saw the pruned subject");
    assert_eq!(relay.poll("gap", &token, latest, 0).await, [], "resume from latest_cursor after a gap");
}

async fn rotation_and_deprovisioning(relay: &Relay) {
    let (old_token, old_secret) = relay.provision("rotate").await;
    let new_token: IssuedToken = relay.admin(Method::POST, "rotate/tokens", None).await;
    let supplied = json!({ "secret": "a-new-webhook-secret-of-sufficient-length" }).to_string();
    let new_secret: IssuedSecret = relay.admin(Method::POST, "rotate/sources/github/secrets", Some(supplied)).await;

    // Both generations verify during the overlap.
    relay.deliver_pr("rotate", &old_secret, 1, "a").await;
    relay.deliver_pr("rotate", &new_secret.secret, 2, "b").await;
    let mut old_socket = relay.connect("rotate", &old_token, 2).await;
    assert_eq!(next_frame(&mut old_socket).await, StreamFrame::Ready { cursor: 2 });

    let description: InstallDescription = relay.admin(Method::GET, "rotate", None).await;
    let old_id = description.consumer_tokens.iter().find(|info| info.id != new_token.id).expect("old token").id.clone();
    let old_secret_id = description.sources["github"].iter().find(|info| info.id != new_secret.id).expect("old secret").id.clone();
    let body = serde_json::to_string(&description).expect("serialize");
    assert!(!body.contains(&old_token) && !body.contains(&old_secret), "description carries no credential material");

    let (status, _) = relay.request(Method::DELETE, &format!("/admin/installs/rotate/tokens/{old_id}"), OPERATOR_TOKEN, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = relay
        .request(Method::DELETE, &format!("/admin/installs/rotate/sources/github/secrets/{old_secret_id}"), OPERATOR_TOKEN, None)
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let closed = tokio::time::timeout(Duration::from_secs(10), old_socket.next()).await.expect("close within 10s");
    assert!(matches!(closed, None | Some(Ok(Message::Close(_))) | Some(Err(_))), "revoking a token closes its sockets: {closed:?}");
    assert_eq!(relay.request(Method::GET, "/i/rotate/stream?cursor=0", &old_token, None).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(relay.poll("rotate", &new_token.token, 0, 0).await.len(), 2);
    assert_eq!(relay.deliver("rotate", &old_secret, "pull_request", "c", &pr_payload(1)).await.0, StatusCode::UNAUTHORIZED);

    let (status, _) = relay.request(Method::DELETE, "/admin/installs/rotate", OPERATOR_TOKEN, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let gone = relay.request(Method::GET, "/i/rotate/stream?cursor=0", &new_token.token, None).await;
    assert_eq!(gone, relay.request(Method::GET, "/i/never-existed/stream?cursor=0", &new_token.token, None).await);
    assert_eq!(relay.request(Method::GET, "/admin/installs/rotate", OPERATOR_TOKEN, None).await.0, StatusCode::NOT_FOUND);
}
