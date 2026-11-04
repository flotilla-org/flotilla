//! The surface WebSocket through the router and the TCP listener policy, over an in-memory
//! stream: no listener is bound.
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::Duration,
};

use axum::{
    body::{to_bytes, Body},
    http::{header, Request, StatusCode},
    Router,
};
use flotilla_core::in_process::InProcessDaemon;
use flotilla_protocol::{
    AgentHookTerminalRef, Command, CommandAction, EnvironmentId, HostName, QueryCursor, QueryId, Request as RpcRequest,
    Response as RpcResponse,
};
use flotilla_resources::{Convoy, ConvoySpec, InputMeta};
use flotilla_store::ResourceBackend;
use flotilla_store_memory::InMemoryBackend;
use futures::{SinkExt, StreamExt};
use rigging_http_serve::{Authentication, Caller, TcpPolicy};
use serde_json::{json, Value};
use tokio::io::DuplexStream;
use tokio_tungstenite::{
    tungstenite::{self, client::IntoClientRequest, protocol::frame::coding::CloseCode, Message as Frame},
    WebSocketStream,
};
use tower::ServiceExt;

use super::{surface, CallerAuth, HttpRouter, ListenerOrigin, LoginStore};

const HOST: &str = "surface.test";
const WAIT: Duration = Duration::from_secs(10);

fn router(backend: ResourceBackend) -> Router {
    HttpRouter::builder()
        .backend(backend)
        .logins(Arc::new(LoginStore::new(Arc::new(flotilla_resources::SystemClock))))
        .build()
        .into_router()
}

/// One client connection, served by `router` as the TCP listener serves one: its Host policy
/// and the caller its authentication would yield.
struct Client {
    socket: WebSocketStream<DuplexStream>,
    cbor: bool,
}

async fn connect(router: &Router, caller: CallerAuth, protocols: &[&str]) -> Result<Client, tungstenite::Error> {
    connect_from(router, caller, false, None, protocols).await
}

/// As [`connect`], authenticated by cookie or not, with an `Origin` header or none.
async fn connect_from(
    router: &Router,
    caller: CallerAuth,
    uses_cookie: bool,
    origin: Option<&str>,
    protocols: &[&str],
) -> Result<Client, tungstenite::Error> {
    let (client, server) = tokio::io::duplex(1 << 16);
    let policy = TcpPolicy::new([HOST.to_owned()], move |_| Some(Authentication { caller: Caller::Custom(caller.clone()), uses_cookie }));
    tokio::spawn(policy.serve_connection(server, router.clone(), std::future::pending()));
    let mut request = format!("ws://{HOST}{}", surface::PATH).into_client_request()?;
    if !protocols.is_empty() {
        request.headers_mut().insert(header::SEC_WEBSOCKET_PROTOCOL, protocols.join(", ").parse().unwrap());
    }
    if let Some(origin) = origin {
        request.headers_mut().insert(header::ORIGIN, origin.parse().unwrap());
    }
    let (socket, response) = tokio_tungstenite::client_async(request, client).await?;
    let selected = response.headers().get(header::SEC_WEBSOCKET_PROTOCOL).and_then(|value| value.to_str().ok()).map(str::to_owned);
    let mut client = Client { socket, cbor: selected.as_deref() == Some(surface::CBOR) };
    assert_eq!(client.next().await["type"], "hello");
    Ok(client)
}

async fn user(router: &Router, protocol: &str) -> Client {
    connect(router, CallerAuth::Bearer, &[protocol]).await.expect("connect")
}

impl Client {
    async fn send(&mut self, message: Value) {
        let frame = if self.cbor {
            Frame::Binary(flotilla_cbor::to_vec(&message, &Default::default()).unwrap().into())
        } else {
            Frame::Text(message.to_string().into())
        };
        self.socket.send(frame).await.expect("send");
    }

    async fn send_raw(&mut self, frame: Frame) {
        self.socket.send(frame).await.expect("send");
    }

    /// The next message; a close or a silent connection fails the test.
    async fn next(&mut self) -> Value {
        self.next_within(WAIT).await
    }

    async fn next_within(&mut self, limit: Duration) -> Value {
        loop {
            let frame = tokio::time::timeout(limit, self.socket.next()).await.expect("a message in time").expect("open").expect("frame");
            return match frame {
                Frame::Binary(bytes) => flotilla_cbor::from_slice(&bytes, &rigging_http_wire::LIMITS).unwrap(),
                Frame::Text(text) => serde_json::from_str(&text).unwrap(),
                Frame::Ping(_) | Frame::Pong(_) => continue,
                other => panic!("expected a message, got {other:?}"),
            };
        }
    }

    /// Messages up to and including the first that `last` accepts.
    async fn until(&mut self, last: impl Fn(&Value) -> bool) -> Vec<Value> {
        let mut seen = Vec::new();
        loop {
            let message = self.next().await;
            let done = last(&message);
            seen.push(message);
            if done {
                return seen;
            }
        }
    }

    /// A round trip: the server answers an unknown type in order, so every frame sent before it
    /// has been handled.
    async fn fence(&mut self) -> Vec<Value> {
        self.send(json!({"type": "fence", "id": "fence"})).await;
        let mut seen = self.until(|message| message["id"] == "fence").await;
        let answer = seen.pop().unwrap();
        assert_eq!(answer["code"], "unknown_type");
        seen
    }

    async fn close_code(&mut self) -> Option<CloseCode> {
        loop {
            match tokio::time::timeout(WAIT, self.socket.next()).await.expect("closed in time") {
                Some(Ok(Frame::Close(frame))) => return frame.map(|frame| frame.code),
                Some(Ok(_)) => continue,
                Some(Err(_)) | None => return None,
            }
        }
    }
}

fn of<'a>(messages: &'a [Value], id: &str) -> Vec<&'a Value> {
    messages.iter().filter(|message| message["id"] == id).collect()
}

fn types(messages: &[&Value]) -> Vec<String> {
    messages.iter().map(|message| message["type"].as_str().unwrap().to_owned()).collect()
}

fn subscribe(id: &str) -> Value {
    json!({"type": "subscribe", "id": id, "table": "convoys", "namespace": "test", "columns": ["spec"]})
}

async fn create(backend: &ResourceBackend, name: &str) {
    backend
        .using::<Convoy>("test")
        .create(&InputMeta::builder().name(name.into()).build(), &ConvoySpec::builder().workflow_ref("workflow".into()).build())
        .await
        .unwrap();
}

async fn delete(backend: &ResourceBackend, name: &str) {
    backend.using::<Convoy>("test").delete(name).await.unwrap();
}

/// A subscriber's copy of its table, built only from the messages it received.
#[derive(Default)]
struct Mirror {
    keys: Vec<String>,
    rows: BTreeMap<String, Value>,
    synced: bool,
    sequence: u64,
}

impl Mirror {
    /// Applies one subscription message, asserting the channel order: nothing about rows before
    /// the description, no change before the snapshot is complete, and sequences never going back.
    fn apply(&mut self, message: &Value) {
        let key = |row: &Value, keys: &[String]| Value::Array(keys.iter().map(|key| row[key].clone()).collect()).to_string();
        if let Some(sequence) = message.get("sequence").and_then(Value::as_u64) {
            assert!(sequence >= self.sequence, "sequence went back: {message}");
            self.sequence = sequence;
        }
        match message["type"].as_str().unwrap() {
            "resync" => {
                *self = Mirror::default();
            }
            "table" => {
                assert!(!self.synced, "a description inside a live stream: {message}");
                self.keys = serde_json::from_value(message["schema"]["primaryKey"].clone()).unwrap();
                self.rows.clear();
            }
            "rows" => {
                assert!(!self.keys.is_empty() && !self.synced, "rows outside a snapshot: {message}");
                for row in message["rows"].as_array().unwrap() {
                    self.rows.insert(key(row, &self.keys), row.clone());
                }
            }
            "synced" => {
                assert!(!self.keys.is_empty(), "synced before a description: {message}");
                self.synced = true;
            }
            "upsert" => {
                assert!(self.synced, "a change before its snapshot: {message}");
                self.rows.insert(Value::Array(message["key"].as_array().unwrap().clone()).to_string(), message["row"].clone());
            }
            "delete" => {
                assert!(self.synced, "a change before its snapshot: {message}");
                self.rows.remove(&Value::Array(message["key"].as_array().unwrap().clone()).to_string());
            }
            "progress" => assert!(self.synced, "progress before the snapshot: {message}"),
            "count" => {}
            other => panic!("not a table message: {other}"),
        }
    }

    fn names(&self) -> BTreeSet<String> {
        self.rows.values().map(|row| row["metadata.name"].as_str().unwrap().to_owned()).collect()
    }
}

// #3249: a subscription receives `table`, `rows`, `synced`, then each change in order, and both
// subprotocols carry equal content. Two clients on one router see the same source changes.
#[tokio::test]
async fn subscriptions_stream_snapshot_then_changes_alike_on_both_subprotocols() {
    let backend = ResourceBackend::new(InMemoryBackend::default());
    create(&backend, "a").await;
    let router = router(backend.clone());
    let mut clients = [user(&router, surface::CBOR).await, user(&router, surface::JSON).await];
    let mut received = Vec::new();
    for client in &mut clients {
        client.send(subscribe("s")).await;
        received.push(client.until(|message| message["type"] == "synced").await);
    }
    create(&backend, "b").await;
    delete(&backend, "a").await;
    for (client, received) in clients.iter_mut().zip(&mut received) {
        received.extend(client.until(|message| message["type"] == "delete").await);
    }
    assert_eq!(received[0], received[1]);
    let messages: Vec<_> = received[0].iter().collect();
    assert_eq!(types(&messages), ["table", "rows", "synced", "upsert", "delete"]);
    let mut mirror = Mirror::default();
    for message in &messages {
        assert_eq!(message["id"], "s");
        mirror.apply(message);
    }
    assert_eq!(mirror.names(), BTreeSet::from(["b".to_owned()]));
}

// #3249: a cursor names the incarnation and the last sequence applied. A current one resumes
// with `synced` alone; an older sequence, the current sequence of another incarnation, or the
// current position of a subscription with other parameters gets `resync` and a fresh `table`,
// `rows`, `synced`.
#[tokio::test]
async fn cursors_resume_only_their_own_current_position() {
    let backend = ResourceBackend::new(InMemoryBackend::default());
    create(&backend, "a").await;
    let router = router(backend.clone());
    let mut client = user(&router, surface::CBOR).await;
    client.send(subscribe("first")).await;
    let snapshot = client.until(|message| message["type"] == "synced").await;
    let incarnation = snapshot[0]["incarnation"].clone();
    let stale = snapshot.last().unwrap()["sequence"].clone();
    create(&backend, "b").await;
    let current = client.until(|message| message["type"] == "upsert").await.pop().unwrap()["sequence"].clone();
    let fresh = vec!["resync", "table", "rows", "synced"];
    for (id, incarnation, sequence, columns, expected) in [
        ("current", incarnation.clone(), current.clone(), json!(["spec"]), vec!["synced"]),
        ("stale", incarnation.clone(), stale, json!(["spec"]), fresh.clone()),
        ("elsewhere", json!("another-incarnation"), current.clone(), json!(["spec"]), fresh.clone()),
        ("reshaped", incarnation, current, json!(["metadata.name"]), fresh),
    ] {
        let mut request = subscribe(id);
        request["columns"] = columns;
        request["cursor"] = json!({"incarnation": incarnation, "sequence": sequence});
        client.send(request).await;
        let messages = client.until(|message| message["id"] == id && message["type"] == "synced").await;
        assert_eq!(types(&of(&messages, id)), expected, "{id}");
    }
}

// #3249: `unsubscribe` ends a subscription and `cancel` ends any open request by its id; neither
// sends anything more for it. An id that names no open request is an error, and an id already
// open cannot be reused.
#[tokio::test]
async fn unsubscribe_and_cancel_end_requests_by_id() {
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let router = router(backend.clone());
    let mut client = user(&router, surface::JSON).await;
    for id in ["kept", "unsubscribed", "cancelled"] {
        client.send(subscribe(id)).await;
        client.until(|message| message["id"] == id && message["type"] == "synced").await;
    }
    client.send(subscribe("kept")).await;
    client.send(json!({"type": "unsubscribe", "id": "unsubscribed"})).await;
    client.send(json!({"type": "cancel", "id": "cancelled"})).await;
    client.send(json!({"type": "cancel", "id": "never"})).await;
    let answered = client.fence().await;
    assert_eq!(of(&answered, "kept")[0]["code"], "duplicate_id");
    assert_eq!(of(&answered, "never")[0]["code"], "not_found");
    create(&backend, "a").await;
    let changes = client.until(|message| message["id"] == "kept" && message["type"] == "upsert").await;
    client.send(json!({"type": "unsubscribe", "id": "unsubscribed"})).await;
    let after = client.fence().await;
    assert!(of(&changes, "unsubscribed").is_empty() && of(&changes, "cancelled").is_empty(), "{changes:?}");
    assert_eq!(of(&after, "unsubscribed")[0]["code"], "not_found");
}

// #3249: the upgrade admits users only, as the table routes do: anonymous callers get 401,
// environment and hook tokens 403. A client offering neither subprotocol is refused before the
// upgrade, and one offering both gets CBOR.
#[tokio::test]
async fn the_upgrade_admits_users_offering_a_surface_subprotocol() {
    let router = router(ResourceBackend::new(InMemoryBackend::default()));
    let status = |result: Result<Client, tungstenite::Error>| match result {
        Err(tungstenite::Error::Http(response)) => response.status(),
        Err(other) => panic!("{other}"),
        Ok(_) => StatusCode::SWITCHING_PROTOCOLS,
    };
    let hook = CallerAuth::Hook {
        terminal: AgentHookTerminalRef { namespace: "flotilla".into(), session_name: "crew".into() },
        generation: "1".into(),
    };
    let environment = CallerAuth::Environment { environment: EnvironmentId::new("env-work"), peer: None };
    for (caller, protocols, expected) in [
        (CallerAuth::Anonymous, vec![surface::CBOR], StatusCode::UNAUTHORIZED),
        (environment, vec![surface::CBOR], StatusCode::FORBIDDEN),
        (hook, vec![surface::JSON], StatusCode::FORBIDDEN),
        (CallerAuth::Bearer, vec![], StatusCode::NOT_ACCEPTABLE),
        (CallerAuth::Bearer, vec!["chat"], StatusCode::NOT_ACCEPTABLE),
        (CallerAuth::Socket(None), vec![surface::JSON], StatusCode::SWITCHING_PROTOCOLS),
    ] {
        assert_eq!(status(connect(&router, caller.clone(), &protocols).await), expected, "{caller:?} {protocols:?}");
    }
    let client = connect(&router, CallerAuth::Bearer, &[surface::JSON, surface::CBOR]).await.unwrap();
    assert!(client.cbor);
}

// Review of #4298: browsers exempt WebSocket upgrades from CORS, so the TCP listener's Origin
// policy guards `/v1/surface` before the upgrade: a foreign `Origin` is refused, and so is a
// cookie-authenticated upgrade with no `Origin`; a same-origin one is admitted.
#[tokio::test]
async fn the_upgrade_refuses_foreign_and_missing_origins() {
    let router = router(ResourceBackend::new(InMemoryBackend::default()));
    let logins = LoginStore::new(Arc::new(flotilla_resources::SystemClock));
    let (session, _) = logins.redeem(&logins.mint_code().unwrap().code).unwrap().unwrap();
    let session = CallerAuth::Session(session);
    let same = format!("http://{HOST}");
    for (caller, cookie, origin, expected) in [
        (CallerAuth::Bearer, false, Some("http://elsewhere.test"), StatusCode::FORBIDDEN),
        (session.clone(), true, Some("http://elsewhere.test"), StatusCode::FORBIDDEN),
        (session.clone(), true, None, StatusCode::FORBIDDEN),
        (session, true, Some(same.as_str()), StatusCode::SWITCHING_PROTOCOLS),
    ] {
        let status = match connect_from(&router, caller, cookie, origin, &[surface::CBOR]).await {
            Err(tungstenite::Error::Http(response)) => response.status(),
            Err(other) => panic!("{other}"),
            Ok(_) => StatusCode::SWITCHING_PROTOCOLS,
        };
        assert_eq!(status, expected, "cookie {cookie}, origin {origin:?}");
    }
}

// #3249: frames are refused, not decoded, past the reader's limits. The CBOR profile's safe reader
// rejects duplicate keys, tags and trailing bytes, a frame of the other subprotocol's type is
// refused, and an unknown type is answered with `error`; after each the connection stays up. An
// oversized frame closes it.
#[tokio::test]
async fn malformed_frames_get_errors_and_oversized_frames_close() {
    let router = router(ResourceBackend::new(InMemoryBackend::default()));
    let mut client = user(&router, surface::CBOR).await;
    let valid = flotilla_cbor::to_vec(&json!({"type": "cancel", "id": "x"}), &Default::default()).unwrap();
    let duplicate = vec![0xa2, 0x64, b't', b'y', b'p', b'e', 0x61, b'a', 0x64, b't', b'y', b'p', b'e', 0x61, b'b'];
    let tagged = [vec![0xc1], valid.clone()].concat();
    let trailing = [valid.clone(), vec![0x00]].concat();
    for bytes in [duplicate, tagged, trailing] {
        client.send_raw(Frame::Binary(bytes.into())).await;
        assert_eq!(client.next().await["code"], "invalid_message");
    }
    client.send_raw(Frame::Text("{}".into())).await;
    assert_eq!(client.next().await["code"], "invalid_message");
    client.send(json!({"type": "cancel", "id": ["x"]})).await;
    assert_eq!(client.next().await["code"], "invalid_message");
    client.send(json!({"type": "teleport", "id": 7})).await;
    let unknown = client.next().await;
    assert_eq!((unknown["code"].as_str(), unknown["id"].as_u64()), (Some("unknown_type"), Some(7)));
    client.send_raw(Frame::Binary(valid.into())).await;
    assert_eq!(client.next().await["code"], "not_found");

    // The server may close before the whole frame is written.
    let _ = client.socket.send(Frame::Binary(vec![0; surface::FRAME_LIMIT + 1].into())).await;
    assert_eq!(client.close_code().await, Some(CloseCode::Size));
}

// Behaviour (#3249): whatever interleaving of subscribe, unsubscribe and source changes, every
// subscription sees its description, then its snapshot, then changes, with sequences never going
// back; and each live subscription's copy converges on the store. Operations draw from three
// names so creates, recreates and deletes collide.
#[hegel::test]
fn interleaved_subscriptions_and_changes_keep_each_channel_ordered(tc: hegel::TestCase) {
    use hegel::generators as gs;
    enum Op {
        Subscribe,
        Unsubscribe(usize),
        Toggle(String),
        Edit(String),
    }
    let names = || gs::sampled_from(vec!["a".to_owned(), "b".to_owned(), "c".to_owned()]);
    let ops: Vec<Op> = (0..tc.draw(gs::integers::<usize>().min_value(1).max_value(10)))
        .map(|_| match tc.draw(gs::integers::<u8>().max_value(3)) {
            0 => Op::Subscribe,
            1 => Op::Unsubscribe(tc.draw(gs::integers::<usize>().max_value(4))),
            2 => Op::Toggle(tc.draw(names())),
            _ => Op::Edit(tc.draw(names())),
        })
        .collect();
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    runtime.block_on(async move {
        let backend = ResourceBackend::new(InMemoryBackend::default());
        let convoys = backend.using::<Convoy>("test");
        let router = router(backend.clone());
        let mut client = user(&router, surface::CBOR).await;
        let mut mirrors: BTreeMap<String, Mirror> = BTreeMap::new();
        let mut live = Vec::new();
        let mut received = Vec::new();
        for (step, op) in ops.into_iter().enumerate() {
            match op {
                Op::Subscribe => {
                    let id = format!("s{step}");
                    client.send(json!({"type": "subscribe", "id": id, "table": "convoys", "namespace": "test"})).await;
                    mirrors.insert(id.clone(), Mirror::default());
                    live.push(id);
                }
                Op::Unsubscribe(index) if !live.is_empty() => {
                    let id = live.remove(index % live.len());
                    client.send(json!({"type": "unsubscribe", "id": id})).await;
                }
                Op::Unsubscribe(_) => {}
                Op::Toggle(name) => match convoys.get(&name).await {
                    Ok(_) => convoys.delete(&name).await.unwrap(),
                    Err(_) => create(&backend, &name).await,
                },
                Op::Edit(name) => {
                    if let Ok(convoy) = convoys.get(&name).await {
                        let spec = ConvoySpec::builder().workflow_ref(format!("workflow-{step}")).build();
                        let meta = InputMeta::builder().name(name.clone()).build();
                        convoys.update(&meta, &convoy.metadata.resource_version, &spec).await.unwrap();
                    }
                }
            }
            received.extend(client.fence().await);
        }
        create(&backend, "fence").await;
        let expected: BTreeSet<String> = convoys.list().await.unwrap().items.into_iter().map(|convoy| convoy.metadata.name).collect();
        let converged = |mirrors: &BTreeMap<String, Mirror>| live.iter().all(|id| mirrors[id].names() == expected);
        let mut applied = 0;
        loop {
            for message in &received[applied..] {
                if let Some(mirror) = message["id"].as_str().and_then(|id| mirrors.get_mut(id)) {
                    mirror.apply(message);
                }
            }
            applied = received.len();
            if converged(&mirrors) {
                break;
            }
            received.push(client.next().await);
        }
    });
}

/// A daemon with its aggregator running and the HTTP command service installed.
struct Daemon {
    _temp: tempfile::TempDir,
    daemon: Arc<InProcessDaemon>,
    router: Router,
    _runtime: crate::runtime::DaemonRuntime,
}

impl Daemon {
    async fn start() -> Self {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"surface-test\"\n").unwrap();
        let config = Arc::new(flotilla_config::ConfigStore::with_base(temp.path()));
        let daemon = InProcessDaemon::new_with_resource_backend(
            vec![],
            Arc::clone(&config),
            flotilla_discovery_testkit::fake_discovery(false),
            HostName::new("surface-test"),
            ResourceBackend::new(InMemoryBackend::default()),
            Arc::new(crate::peer::HostPeerOverlay::default()),
        )
        .await;
        let options = crate::runtime::RuntimeOptions {
            namespace: "flotilla".into(),
            heartbeat_interval: Duration::from_secs(300),
            controller_resync_interval: Duration::from_secs(300),
            start_controllers: false,
            ..crate::runtime::RuntimeOptions::default()
        };
        let runtime = crate::runtime::DaemonRuntime::start_with_options(Arc::clone(&daemon), config, None, options).await.expect("runtime");
        let manager = Arc::new(tokio::sync::Mutex::new(crate::peer::PeerManager::new(daemon.node_id().clone())));
        let remote = crate::server::build_remote_command_router(&daemon, &manager);
        let (shutdown, _) = tokio::sync::mpsc::unbounded_channel();
        let router =
            HttpRouter::for_daemon(Arc::clone(&daemon), Arc::new(LoginStore::new(daemon.clock_for_scenarios())), ListenerOrigin::default())
                .with_commands(remote, shutdown, temp.path().join("daemon.sock"))
                .into_router();
        Self { _temp: temp, daemon, router, _runtime: runtime }
    }

    async fn call(&self, method: &str, path: &str, body: Option<Value>) -> Value {
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT, "application/json")
            .body(body.map_or_else(Body::empty, |body| Body::from(body.to_string())))
            .unwrap();
        request.extensions_mut().insert(CallerAuth::Bearer);
        let response = self.router.clone().oneshot(request).await.unwrap();
        assert!(response.status().is_success(), "{method} {path}: {}", response.status());
        serde_json::from_slice(&to_bytes(response.into_body(), 1 << 20).await.unwrap()).unwrap()
    }

    async fn convoy(&self, name: &str) {
        self.daemon
            .resource_backend()
            .using::<Convoy>("flotilla")
            .create(&InputMeta::builder().name(name.into()).build(), &ConvoySpec::builder().workflow_ref("workflow".into()).build())
            .await
            .unwrap();
    }

    /// The framed protocol's result set for `query`, as `SubscribeQueries` replays it.
    async fn framed(&self, query: &QueryId) -> flotilla_protocol::ResultSet {
        let manager = Arc::new(tokio::sync::Mutex::new(crate::peer::PeerManager::new(self.daemon.node_id().clone())));
        let remote = crate::server::build_remote_command_router(&self.daemon, &manager);
        let dispatcher = crate::server::request_dispatch::RequestDispatcher::new(
            &self.daemon,
            &remote,
            self.daemon.agent_state_store(),
            uuid::Uuid::new_v4(),
            Default::default(),
        );
        let request = RpcRequest::SubscribeQueries { queries: vec![QueryCursor { query: query.clone(), since: None }] };
        let flotilla_protocol::Message::Response { response, .. } = dispatcher.dispatch(1, request).await else { panic!("response") };
        let flotilla_protocol::ResponseResult::Ok { response } = *response else { panic!("framed subscribe failed") };
        let RpcResponse::SubscribeQueries(events) = *response else { panic!("subscribe response") };
        events
            .into_iter()
            .find_map(|event| match event {
                flotilla_protocol::DaemonEvent::ResultSet(set) => Some(*set),
                _ => None,
            })
            .expect("a replayed result set")
    }
}

// #3249: a named query's table carries the content of its framed result set: the same rows at
// the same sequence, under its row type's description. The framed rows are the oracle here,
// read through `SubscribeQueries` on a connection of their own.
#[tokio::test]
async fn named_query_tables_carry_the_framed_result_set() {
    let daemon = Daemon::start().await;
    daemon.convoy("first").await;
    let query = QueryId::Convoys { scope: None };
    let mut client = user(&daemon.router, surface::JSON).await;
    client.send(json!({"type": "subscribe", "id": "q", "query": query})).await;
    let mut mirror = Mirror::default();
    for message in client.until(|message| message["type"] == "synced").await {
        mirror.apply(&message);
    }
    daemon.convoy("second").await;
    loop {
        let framed = daemon.framed(&query).await;
        if framed.seq == mirror.sequence && framed.rows.len() == 2 {
            let names: BTreeSet<_> = framed.rows.as_convoys().unwrap().iter().map(|row| row.name.clone()).collect();
            let carried: BTreeSet<_> = mirror.rows.values().map(|row| row["name"].as_str().unwrap().to_owned()).collect();
            assert_eq!(carried, names);
            let root = &framed.rows.table_set().tables[0].name;
            let shredded = framed.rows.tables(root).remove(root).unwrap();
            assert_eq!(mirror.rows.len(), shredded.len());
            for row in shredded {
                let carried = mirror.rows.values().find(|carried| carried["resource_name"] == row["resource_name"]).unwrap();
                for (column, value) in carried.as_object().unwrap() {
                    assert_eq!(row.get(column).unwrap_or(&Value::Null), value, "{column}");
                }
            }
            break;
        }
        mirror.apply(&client.next().await);
    }
}

fn issues() -> QueryId {
    QueryId::Issues { scope: flotilla_protocol::QueryScope::new("flotilla", "demo"), search: None, label: None }
}

// #3249: connection-owned state lives on the socket: a named query held by a connection is
// released from the aggregator when it closes, and `fetch_more` is refused for a query the
// connection does not hold. The aggregator's demand is watched, not polled.
#[tokio::test]
async fn closing_the_connection_releases_its_named_queries() {
    let daemon = Daemon::start().await;
    let query = issues();
    let mut client = user(&daemon.router, surface::CBOR).await;
    client.send(json!({"type": "fetch_more", "id": "early", "query": query})).await;
    assert_eq!(client.next().await["code"], "invalid_request");
    client.send(json!({"type": "subscribe", "id": "q", "query": query})).await;
    client.until(|message| message["type"] == "synced").await;
    let mut demand = daemon.daemon.aggregator_projection_state().await.subscribe_demand();
    assert!(demand.borrow().contains_key(&query));
    drop(client);
    tokio::time::timeout(WAIT, demand.wait_for(|demand| !demand.contains_key(&query))).await.expect("released on close").unwrap();
}

// Review of #4298: a GET of an issues query returns its rows with no subscription holding it;
// the read takes the query's demand and releases it when it ends.
#[tokio::test]
async fn a_get_reads_an_issues_query_no_subscription_holds() {
    let daemon = Daemon::start().await;
    let query = issues();
    let encoded: String = serde_json::to_string(&query).unwrap().bytes().map(|byte| format!("%{byte:02X}")).collect();
    let mut request = Request::builder()
        .uri(format!("/tables/issue_row/rows?query={encoded}"))
        .header(header::ACCEPT, "application/jsonl")
        .body(Body::empty())
        .unwrap();
    request.extensions_mut().insert(CallerAuth::Bearer);
    let response = daemon.router.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    let messages: Vec<Value> =
        body.split(|byte| *byte == b'\n').filter(|line| !line.is_empty()).map(|line| serde_json::from_slice(line).unwrap()).collect();
    assert_eq!(messages[0]["type"], "table");
    let rows: usize =
        messages.iter().filter(|message| message["type"] == "rows").map(|message| message["rows"].as_array().unwrap().len()).sum();
    let mut demand = daemon.daemon.aggregator_projection_state().await.subscribe_demand();
    tokio::time::timeout(WAIT, demand.wait_for(|demand| !demand.contains_key(&query))).await.expect("released after the read").unwrap();
    assert_eq!(rows, daemon.framed(&query).await.rows.len());
}

// Review of #4298: a named-query cursor binds to the materialisation it was read from. A current
// cursor resumes with `synced`. Once an issues query's last subscriber has left and it is
// materialised again, its sequence restarts and its incarnation changes, so a cursor from the
// earlier materialisation gets `resync` at whatever sequence it names.
#[tokio::test]
async fn named_query_cursors_resume_only_their_own_materialisation() {
    let daemon = Daemon::start().await;
    let mut client = user(&daemon.router, surface::JSON).await;
    let convoys = QueryId::Convoys { scope: None };
    client.send(json!({"type": "subscribe", "id": "convoys", "query": convoys})).await;
    let snapshot = client.until(|message| message["type"] == "synced").await;
    let cursor = json!({"incarnation": snapshot[0]["incarnation"], "sequence": snapshot.last().unwrap()["sequence"]});
    client.send(json!({"type": "subscribe", "id": "current", "query": convoys, "cursor": cursor})).await;
    let resumed = client.until(|message| message["id"] == "current" && message["type"] == "synced").await;
    assert_eq!(types(&of(&resumed, "current")), ["synced"]);

    let query = issues();
    client.send(json!({"type": "subscribe", "id": "held", "query": query})).await;
    let first = client.until(|message| message["id"] == "held" && message["type"] == "synced").await;
    let earlier = of(&first, "held")[0]["incarnation"].clone();
    let mut demand = daemon.daemon.aggregator_projection_state().await.subscribe_demand();
    client.send(json!({"type": "unsubscribe", "id": "held"})).await;
    tokio::time::timeout(WAIT, demand.wait_for(|demand| !demand.contains_key(&query))).await.expect("released").unwrap();
    client.send(json!({"type": "subscribe", "id": "again", "query": query})).await;
    let again = client.until(|message| message["id"] == "again" && message["type"] == "synced").await;
    let again = of(&again, "again");
    assert_ne!(again[0]["incarnation"], earlier);
    let cursor = json!({"incarnation": earlier, "sequence": again.last().unwrap()["sequence"]});
    client.send(json!({"type": "subscribe", "id": "old", "query": query, "cursor": cursor})).await;
    let old = client.until(|message| message["id"] == "old" && message["type"] == "synced").await;
    assert_eq!(types(&of(&old, "old")), ["resync", "table", "rows", "synced"]);
}

// #3249: `wait_subscribe` answers with `wait_fired` when its leaf becomes true, and the fire ends
// the request; a wait cancelled before its leaf turns true never fires.
#[tokio::test]
async fn waits_fire_once_and_cancelled_waits_do_not() {
    use flotilla_protocol::{Leaf, LeafAddress, LeafOperator};
    use flotilla_resources::{ConvoyPhase, ConvoyStatus};
    let daemon = Daemon::start().await;
    daemon.convoy("watched").await;
    let leaf = Leaf {
        address: LeafAddress::Convoy { name: "watched".into() },
        field_path: ".status.phase".into(),
        operator: LeafOperator::Equal,
        literal: "Failed".into(),
    };
    let subscription = json!({"namespace": "flotilla", "leaves": [leaf]});
    let mut client = user(&daemon.router, surface::CBOR).await;
    for id in ["cancelled", "w"] {
        client.send(json!({"type": "wait_subscribe", "id": id, "subscription": subscription})).await;
    }
    client.send(json!({"type": "cancel", "id": "cancelled"})).await;
    assert!(client.fence().await.is_empty());
    let convoys = daemon.daemon.resource_backend().using::<Convoy>("flotilla");
    let current = convoys.get("watched").await.unwrap();
    let status = ConvoyStatus { phase: ConvoyPhase::Failed, ..Default::default() };
    convoys.update_status("watched", &current.metadata.resource_version, &status).await.unwrap();
    let mut received = client.until(|message| message["type"] == "wait_fired").await;
    let fired = received.last().unwrap().clone();
    assert_eq!((fired["id"].as_str(), fired["value"].as_str()), (Some("w"), Some("Failed")));
    assert_eq!(fired["leaf"], serde_json::to_value(&leaf).unwrap());
    client.send(json!({"type": "cancel", "id": "w"})).await;
    received.extend(client.fence().await);
    assert!(of(&received, "cancelled").is_empty(), "{received:?}");
    assert_eq!(of(&received, "w").last().unwrap()["code"], "not_found");
}

// #3249: a consumer that stops reading is closed with 1013 once its queue of 1,024 frames is full,
// rather than buffered without bound or slowing the producers; it resumes by cursor.
#[tokio::test]
async fn a_consumer_that_falls_behind_is_closed_with_1013() {
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let router = router(backend.clone());
    let mut client = user(&router, surface::CBOR).await;
    for index in 0..200 {
        client.send(subscribe(&format!("s{index}"))).await;
    }
    let mut synced = 0;
    while synced < 200 {
        synced += usize::from(client.next().await["type"] == "synced");
    }
    for index in 0..30 {
        create(&backend, &format!("c{index}")).await;
    }
    assert_eq!(client.close_code().await, Some(CloseCode::Again));
}

// Review of #4298: a client that stops reading cannot hold the connection open. A frame that
// cannot be written within the write limit closes it, releasing what it holds; under paused time.
#[tokio::test(start_paused = true)]
async fn a_client_that_never_reads_is_dropped() {
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let router = router(backend.clone());
    // Few, large frames: the socket's buffer fills long before the send queue would overflow.
    for index in 0..300 {
        create(&backend, &format!("c{index}")).await;
    }
    let mut client = user(&router, surface::CBOR).await;
    for index in 0..10 {
        client.send(json!({"type": "subscribe", "id": format!("s{index}"), "table": "convoys", "namespace": "test"})).await;
    }
    tokio::time::sleep(surface::WRITE_LIMIT * 2).await;
    let probe = client.socket.send(Frame::Text("{}".into())).await;
    assert!(probe.is_err(), "the server still reads from a client that never reads");
}

// #3249: a subscription with nothing to report still hears from the server at least every 15
// seconds, under paused time.
#[tokio::test(start_paused = true)]
async fn quiet_subscriptions_send_progress_at_least_every_15_seconds() {
    let router = router(ResourceBackend::new(InMemoryBackend::default()));
    let mut client = user(&router, surface::CBOR).await;
    client.send(subscribe("quiet")).await;
    client.until(|message| message["type"] == "synced").await;
    for _ in 0..3 {
        let since = tokio::time::Instant::now();
        let message = client.next_within(Duration::from_secs(16)).await;
        assert_eq!((message["type"].as_str(), message["id"].as_str()), (Some("progress"), Some("quiet")));
        assert!(since.elapsed() <= Duration::from_secs(15), "{:?}", since.elapsed());
    }
}

// #3249: a connection may follow a command started over plain HTTP, receiving its steps as
// `command_step` and its outcome as `command_outcome`, in the shapes `/v1/commands/{id}` returns.
#[tokio::test]
async fn commands_started_over_http_report_progress_on_the_socket() {
    let daemon = Daemon::start().await;
    let command = Command::builder()
        .action(CommandAction::ResourceWatch {
            namespace: "flotilla".into(),
            kind: "convoy".into(),
            name: None,
            include_replicas: false,
            replica_sources: false,
            cursor: None,
        })
        .build();
    let accepted = daemon.call("POST", "/v1/commands", Some(serde_json::to_value(command).unwrap())).await;
    let id = accepted["id"].as_str().unwrap().to_owned();
    let mut client = user(&daemon.router, surface::CBOR).await;
    client.send(json!({"type": "command", "id": "c", "command": "00000000-0000-0000-0000-000000000000"})).await;
    assert_eq!(client.next().await["code"], "not_found");
    client.send(json!({"type": "command", "id": "c", "command": id})).await;
    let step = client.next().await;
    assert_eq!((step["type"].as_str(), step["id"].as_str()), (Some("command_step"), Some("c")));
    let record = daemon.call("GET", &format!("/v1/commands/{id}"), None).await;
    assert_eq!(step["step"], record["steps"][0]);
    daemon.call("DELETE", &format!("/v1/commands/{id}"), None).await;
    let outcome = client.until(|message| message["type"] == "command_outcome").await.pop().unwrap();
    let record = daemon.call("GET", &format!("/v1/commands/{id}"), None).await;
    for field in ["state", "outcome", "progress_truncated", "observation_error"] {
        assert_eq!(outcome[field], record[field], "{field}");
    }
    assert_eq!(outcome["state"], "finished");
}

// Review of #4298: a request's id is free again once it ends, so a connection issuing many short
// requests never accumulates them; at most `OPEN_LIMIT` requests are open at once, and one more is
// refused with `too_many_requests` while the connection stays up.
#[tokio::test]
async fn ended_requests_free_their_ids_and_open_requests_are_bounded() {
    let router = router(ResourceBackend::new(InMemoryBackend::default()));
    let mut client = user(&router, surface::CBOR).await;
    let query = QueryId::Convoys { scope: None };
    for round in 0..surface::OPEN_LIMIT + 8 {
        let id = if round % 2 == 0 { "reused".to_owned() } else { format!("fresh-{round}") };
        client.send(json!({"type": "fetch_more", "id": id, "query": query})).await;
        assert_eq!(client.next().await["code"], "unavailable", "round {round}");
    }
    for index in 0..surface::OPEN_LIMIT {
        client.send(subscribe(&format!("s{index}"))).await;
    }
    client.send(subscribe("one-more")).await;
    let refused = client.until(|message| message["id"] == "one-more").await.pop().unwrap();
    assert_eq!(refused["code"], "too_many_requests");
    client.send(json!({"type": "cancel", "id": "s0"})).await;
    client.send(subscribe("one-more")).await;
    client.until(|message| message["id"] == "one-more" && message["type"] == "synced").await;
}
