//! The surface WebSocket, `GET /v1/surface`: table and named-query subscriptions, the
//! connection-owned requests and command progress, on one ordered channel per connection.
//! Every subscription is a table read of [`TableSource`]; this module only carries it.
use std::{collections::HashMap, sync::Arc, time::Duration};

use axum::{
    extract::{Request, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::Response,
    Extension,
};
use flotilla_daemon_api::daemon::DaemonHandle;
use flotilla_protocol::{DaemonEvent, QueryId, WaitSubscriptionRequest};
use futures::{stream::BoxStream, SinkExt, StreamExt};
use rigging_http_wire::Encoding;
use serde::{Deserialize, Deserializer};
use serde_json::{json, Map, Value};
use tokio::{sync::mpsc, task::JoinHandle, time::Instant};
use tokio_tungstenite::{
    tungstenite::{
        handshake::derive_accept_key,
        protocol::{frame::coding::CloseCode, CloseFrame, Role, WebSocketConfig},
        Error as SocketError, Message as Frame,
    },
    WebSocketStream,
};

use super::{CallerAuth, HttpRouter, RouterState};
use crate::server::tables::{Failure, Query, QueryFeed, Resume, TableSource};

pub(super) const PATH: &str = "/v1/surface";
pub(super) const CBOR: &str = "flotilla.surface.v1+cbor";
pub(super) const JSON: &str = "flotilla.surface.v1+json";
/// The largest client frame; a larger one is refused by the WebSocket reader before it is read.
pub(super) const FRAME_LIMIT: usize = 64 * 1024;
/// Server frames queued for one connection; a consumer that falls this far behind is closed.
const SEND_QUEUE: usize = 1024;
/// A subscription's whole table in one snapshot, up to the table reader's page limit.
const SNAPSHOT_LIMIT: &str = "10000";
const PING_INTERVAL: Duration = Duration::from_secs(20);
/// A connection silent this long, pongs included, is closed.
const IDLE_LIMIT: Duration = Duration::from_secs(60);
/// Requests one connection may hold open at once.
pub(super) const OPEN_LIMIT: usize = 256;
/// The server's own message types, beside the table messages.
#[cfg(test)]
pub(super) const SERVER_TYPES: &[&str] = &["hello", "error", "command_step", "command_outcome", "wait_fired"];
/// Close code for a consumer whose send queue overflowed; it resumes by cursor.
const OVERFLOW: u16 = 1013;
const TOO_BIG: u16 = 1009;

pub(super) const CLIENT_TYPES: &[&str] =
    &["subscribe", "unsubscribe", "cancel", "fetch_more", "observe_focus", "wait_subscribe", "command"];

type Socket = WebSocketStream<hyper_util::rt::TokioIo<hyper::upgrade::Upgraded>>;

pub(super) async fn upgrade(
    State(RouterState(state)): State<RouterState>,
    Extension(caller): Extension<CallerAuth>,
    mut request: Request,
) -> Response {
    let headers = request.headers();
    let errors = Encoding::for_errors(headers);
    if let Some((status, message)) = caller.user_refusal() {
        return errors.error_code(status, "unauthorized", message);
    }
    let Some((protocol, encoding)) = subprotocol(headers) else {
        return errors.error_code(StatusCode::NOT_ACCEPTABLE, "unsupported_format", format!("offer the {CBOR} or {JSON} subprotocol"));
    };
    let tokens = |name: header::HeaderName, wanted: &str| {
        headers
            .get_all(name)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .flat_map(|value| value.split(','))
            .any(|token| token.trim().eq_ignore_ascii_case(wanted))
    };
    let key = headers.get(header::SEC_WEBSOCKET_KEY).map(|key| derive_accept_key(key.as_bytes()));
    let version = headers.get(header::SEC_WEBSOCKET_VERSION).is_some_and(|version| version == "13");
    let (true, true, true, Some(accept)) = (tokens(header::UPGRADE, "websocket"), tokens(header::CONNECTION, "upgrade"), version, key)
    else {
        return errors.error_code(StatusCode::UPGRADE_REQUIRED, "invalid_request", "a WebSocket version 13 upgrade is required");
    };
    let upgrading = hyper::upgrade::on(&mut request);
    tokio::spawn(async move {
        let Ok(upgraded) = upgrading.await else { return };
        let config = WebSocketConfig::default().max_frame_size(Some(FRAME_LIMIT)).max_message_size(Some(FRAME_LIMIT));
        let socket = WebSocketStream::from_raw_socket(hyper_util::rt::TokioIo::new(upgraded), Role::Server, Some(config)).await;
        Connection::new(state, caller, encoding).run(socket).await;
    });
    let mut response = Response::new(axum::body::Body::empty());
    *response.status_mut() = StatusCode::SWITCHING_PROTOCOLS;
    let response_headers = response.headers_mut();
    response_headers.insert(header::UPGRADE, HeaderValue::from_static("websocket"));
    response_headers.insert(header::CONNECTION, HeaderValue::from_static("upgrade"));
    response_headers.insert(header::SEC_WEBSOCKET_PROTOCOL, HeaderValue::from_static(protocol));
    response_headers.insert(header::SEC_WEBSOCKET_ACCEPT, HeaderValue::from_str(&accept).expect("base64 is a header value"));
    response
}

/// The offered subprotocol this server speaks, CBOR when both are offered.
fn subprotocol(headers: &HeaderMap) -> Option<(&'static str, Encoding)> {
    let offered: Vec<_> = headers
        .get_all(header::SEC_WEBSOCKET_PROTOCOL)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .collect();
    [(CBOR, Encoding::Cbor), (JSON, Encoding::Json)].into_iter().find(|(protocol, _)| offered.contains(protocol))
}

/// A client message's routing id: any string or integer, echoed on every reply.
type Id = Value;

fn key(id: &Id) -> String {
    id.to_string()
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientMessage {
    Subscribe(Subscribe),
    Unsubscribe { id: Id },
    Cancel { id: Id },
    FetchMore { id: Id, query: QueryId },
    ObserveFocus {},
    WaitSubscribe { id: Id, subscription: WaitSubscriptionRequest },
    Command { id: Id, command: String },
}

#[derive(Deserialize)]
struct Subscribe {
    id: Id,
    table: Option<String>,
    query: Option<QueryId>,
    #[serde(rename = "where", default, deserialize_with = "texts")]
    conditions: Vec<String>,
    #[serde(default, deserialize_with = "list")]
    sort: Option<String>,
    #[serde(default, deserialize_with = "list")]
    columns: Option<String>,
    namespace: Option<String>,
    count: Option<String>,
    cursor: Option<Cursor>,
}

#[derive(Deserialize)]
struct Cursor {
    incarnation: String,
    #[serde(deserialize_with = "integer")]
    sequence: u64,
}

/// One text or a list of them.
fn texts<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Texts {
        One(String),
        Many(Vec<String>),
    }
    Ok(match Texts::deserialize(deserializer)? {
        Texts::One(text) => vec![text],
        Texts::Many(texts) => texts,
    })
}

/// A comma-separated list, as the GET routes take it, or a list of names.
fn list<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<String>, D::Error> {
    texts(deserializer).map(|names| Some(names.join(",")))
}

/// An unsigned integer, or the JSON profile's `{"u64": "…"}`.
fn integer<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
    let value = Value::deserialize(deserializer)?;
    value
        .as_u64()
        .or_else(|| value.get("u64").and_then(Value::as_str).and_then(|text| text.parse().ok()))
        .ok_or_else(|| serde::de::Error::custom("expected an unsigned integer"))
}

/// One decoded client frame: a map with its `type`. CBOR goes through the profile's safe reader;
/// JSON refuses duplicate keys as that reader does.
fn decode(encoding: Encoding, bytes: &[u8]) -> Result<Map<String, Value>, String> {
    let value = match encoding {
        Encoding::Cbor => {
            flotilla_cbor::from_slice::<Value>(bytes, &flotilla_cbor::Limits { max_message: FRAME_LIMIT, ..flotilla_cbor::DEFAULT_LIMITS })
                .map_err(|error| error.to_string())?
        }
        Encoding::Json => {
            let mut deserializer = serde_json::Deserializer::from_slice(bytes);
            let Strict(value) = Strict::deserialize(&mut deserializer).map_err(|error| error.to_string())?;
            deserializer.end().map_err(|error| error.to_string())?;
            value
        }
    };
    match value {
        Value::Object(map) => Ok(map),
        _ => Err("a message is a map".into()),
    }
}

/// A JSON value whose maps have distinct keys.
struct Strict(Value);

impl<'de> Deserialize<'de> for Strict {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = Strict;
            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a JSON value")
            }
            fn visit_bool<E>(self, value: bool) -> Result<Strict, E> {
                Ok(Strict(Value::Bool(value)))
            }
            fn visit_i64<E>(self, value: i64) -> Result<Strict, E> {
                Ok(Strict(json!(value)))
            }
            fn visit_u64<E>(self, value: u64) -> Result<Strict, E> {
                Ok(Strict(json!(value)))
            }
            fn visit_f64<E>(self, value: f64) -> Result<Strict, E> {
                Ok(Strict(json!(value)))
            }
            fn visit_str<E>(self, value: &str) -> Result<Strict, E> {
                Ok(Strict(Value::String(value.to_owned())))
            }
            fn visit_string<E>(self, value: String) -> Result<Strict, E> {
                Ok(Strict(Value::String(value)))
            }
            fn visit_unit<E>(self) -> Result<Strict, E> {
                Ok(Strict(Value::Null))
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Strict, A::Error> {
                let mut values = Vec::new();
                while let Some(Strict(value)) = seq.next_element()? {
                    values.push(value);
                }
                Ok(Strict(Value::Array(values)))
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Strict, A::Error> {
                let mut values = Map::new();
                while let Some((name, Strict(value))) = map.next_entry::<String, Strict>()? {
                    if values.insert(name.clone(), value).is_some() {
                        return Err(serde::de::Error::custom(format!("duplicate key `{name}`")));
                    }
                }
                Ok(Strict(Value::Object(values)))
            }
        }
        deserializer.deserialize_any(Visitor)
    }
}

/// A server message: `body`'s fields, plus the `id` it answers.
fn tagged(body: impl serde::Serialize, id: Option<&Id>) -> Value {
    let mut value = body_of(body);
    if let (Some(id), Value::Object(map)) = (id, &mut value) {
        map.insert("id".into(), id.clone());
    }
    value
}

/// A server message body; one that cannot be represented becomes an `encoding` error.
fn body_of(body: impl serde::Serialize) -> Value {
    serde_json::to_value(body).unwrap_or_else(|reason| error("encoding", reason.to_string(), None))
}

fn error(code: &str, message: impl Into<String>, id: Option<&Id>) -> Value {
    tagged(json!({"type": "error", "code": code, "message": message.into()}), id)
}

fn failure(failure: Failure, id: &Id) -> Value {
    error(&failure.code, failure.message, Some(id))
}

/// What the connection loop does next.
enum Event {
    Inbound(Option<Result<Frame, SocketError>>),
    Outbound(Value),
    Finished((String, u64)),
    Overflow,
    Ping,
}

/// Messages reach the socket through one bounded queue; a producer that finds it full closes
/// the connection rather than wait or buffer.
#[derive(Clone)]
struct Outbox {
    queue: mpsc::Sender<Value>,
    overflow: mpsc::Sender<()>,
}

impl Outbox {
    /// Queues `message`; false once the connection is closing.
    fn send(&self, message: Value) -> bool {
        match self.queue.try_send(message) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_)) => {
                let _ = self.overflow.try_send(());
                false
            }
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        }
    }
}

struct Connection {
    state: Arc<HttpRouter>,
    caller: CallerAuth,
    encoding: Encoding,
    feed: Option<Arc<QueryFeed>>,
    /// Open requests by id: subscriptions, command observations, wait subscriptions and
    /// in-flight fetches. Each ends when its task does, or when it is cancelled.
    open: HashMap<String, (u64, JoinHandle<()>)>,
    /// Numbers each started request, so a late end of one does not remove its id's successor.
    started: u64,
}

impl Connection {
    fn new(state: Arc<HttpRouter>, caller: CallerAuth, encoding: Encoding) -> Self {
        let feed = state.daemon.as_ref().map(|daemon| QueryFeed::new(Arc::clone(daemon), state.incarnation.clone()));
        Self { state, caller, encoding, feed, open: HashMap::new(), started: 0 }
    }

    async fn run(mut self, mut socket: Socket) {
        let hello = json!({
            "type": "hello",
            "build": flotilla_daemon_api::build_info::build_id(),
            "fingerprint": flotilla_protocol::PROTOCOL_FINGERPRINT,
            "incarnation": self.state.incarnation,
        });
        let (queue, mut outbound) = mpsc::channel(SEND_QUEUE);
        let (overflow, mut overflowed) = mpsc::channel(1);
        let (finished, mut ended) = mpsc::unbounded_channel::<(String, u64)>();
        let outbox = Outbox { queue, overflow };
        let mut ping = tokio::time::interval_at(Instant::now() + PING_INTERVAL, PING_INTERVAL);
        let mut heard = Instant::now();
        let mut close = None;
        if self.write(&mut socket, hello).await {
            loop {
                let event = tokio::select! {
                    biased;
                    _ = overflowed.recv() => Event::Overflow,
                    frame = socket.next() => Event::Inbound(frame),
                    Some(message) = outbound.recv() => Event::Outbound(message),
                    Some(done) = ended.recv() => Event::Finished(done),
                    _ = ping.tick() => Event::Ping,
                };
                match event {
                    Event::Overflow => {
                        close = Some((OVERFLOW, "send queue overflow; resume by cursor"));
                        break;
                    }
                    Event::Inbound(None) | Event::Inbound(Some(Ok(Frame::Close(_)))) => break,
                    Event::Inbound(Some(Err(error))) => {
                        if matches!(error, SocketError::Capacity(_)) {
                            close = Some((TOO_BIG, "frame exceeds the size limit"));
                        }
                        break;
                    }
                    Event::Inbound(Some(Ok(frame))) => {
                        heard = Instant::now();
                        let bytes = match (self.encoding, frame) {
                            (Encoding::Cbor, Frame::Binary(bytes)) => bytes.to_vec(),
                            (Encoding::Json, Frame::Text(text)) => text.as_bytes().to_vec(),
                            (_, Frame::Ping(_) | Frame::Pong(_)) => continue,
                            _ => {
                                outbox.send(error("invalid_message", "this subprotocol carries the other frame type", None));
                                continue;
                            }
                        };
                        self.receive(&bytes, &outbox, &finished);
                    }
                    Event::Outbound(message) => {
                        if !self.write(&mut socket, message).await {
                            break;
                        }
                    }
                    Event::Finished((id, number)) => {
                        if self.open.get(&id).is_some_and(|(open, _)| *open == number) {
                            self.open.remove(&id);
                        }
                    }
                    Event::Ping => {
                        if heard.elapsed() > IDLE_LIMIT {
                            close = Some((1001, "idle"));
                            break;
                        }
                        if let CallerAuth::Session(session) = &self.caller {
                            if !self.state.logins.is_live(*session) {
                                close = Some((1008, "session ended"));
                                break;
                            }
                        }
                        if socket.send(Frame::Ping(Default::default())).await.is_err() {
                            break;
                        }
                    }
                }
            }
        }
        for (_, (_, task)) in self.open.drain() {
            task.abort();
        }
        if let Some((code, reason)) = close {
            let _ = socket.send(Frame::Close(Some(CloseFrame { code: CloseCode::from(code), reason: reason.into() }))).await;
        }
        if let Some(feed) = &self.feed {
            feed.close().await;
        }
    }

    async fn write(&self, socket: &mut Socket, message: Value) -> bool {
        let bytes = match self.encoding.encode(&message) {
            Ok(bytes) => bytes,
            Err(reason) => match self.encoding.encode(&error("encoding", reason, message.get("id"))) {
                Ok(bytes) => bytes,
                Err(_) => return false,
            },
        };
        let frame = match self.encoding {
            Encoding::Cbor => Frame::Binary(bytes.into()),
            Encoding::Json => Frame::Text(String::from_utf8(bytes).expect("JSON is UTF-8").into()),
        };
        socket.send(frame).await.is_ok()
    }

    /// Handles one client frame; work that waits runs as an open request of its own.
    fn receive(&mut self, bytes: &[u8], outbox: &Outbox, finished: &mpsc::UnboundedSender<(String, u64)>) {
        let map = match decode(self.encoding, bytes) {
            Ok(map) => map,
            Err(reason) => {
                outbox.send(error("invalid_message", reason, None));
                return;
            }
        };
        let id = map.get("id").cloned();
        let kind = map.get("type").and_then(Value::as_str).unwrap_or_default().to_owned();
        if !CLIENT_TYPES.contains(&kind.as_str()) {
            outbox.send(error("unknown_type", format!("unknown message type `{kind}`"), id.as_ref()));
            return;
        }
        let message = match serde_json::from_value::<ClientMessage>(Value::Object(map)) {
            Ok(message) => message,
            Err(reason) => {
                outbox.send(error("invalid_message", reason.to_string(), id.as_ref()));
                return;
            }
        };
        match message {
            ClientMessage::Unsubscribe { id } | ClientMessage::Cancel { id } => match self.open.remove(&key(&id)) {
                Some((_, task)) => task.abort(),
                None => {
                    outbox.send(error("not_found", "no open request has this id", Some(&id)));
                }
            },
            ClientMessage::ObserveFocus { .. } => {}
            ClientMessage::Subscribe(subscribe) => {
                let id = subscribe.id.clone();
                let work = self.subscribe(subscribe);
                self.start(id, work, outbox, finished);
            }
            ClientMessage::FetchMore { id, query } => {
                let work = self.fetch_more(query);
                self.start(id, work, outbox, finished);
            }
            ClientMessage::WaitSubscribe { id, subscription } => {
                let work = self.wait(subscription);
                self.start(id, work, outbox, finished);
            }
            ClientMessage::Command { id, command } => {
                let work = self.command(command);
                self.start(id, work, outbox, finished);
            }
        }
    }

    /// Runs one open request: `work` opens a stream of message bodies, each sent with `id`.
    fn start(
        &mut self,
        id: Id,
        work: futures::future::BoxFuture<'static, Result<BoxStream<'static, Value>, Failure>>,
        outbox: &Outbox,
        finished: &mpsc::UnboundedSender<(String, u64)>,
    ) {
        let name = key(&id);
        if self.open.get(&name).is_some_and(|(_, task)| !task.is_finished()) {
            outbox.send(error("duplicate_id", "a request with this id is open", Some(&id)));
            return;
        }
        if self.open.len() >= OPEN_LIMIT {
            self.open.retain(|_, (_, task)| !task.is_finished());
        }
        if self.open.len() >= OPEN_LIMIT && !self.open.contains_key(&name) {
            outbox.send(error("too_many_requests", format!("a connection holds at most {OPEN_LIMIT} open requests"), Some(&id)));
            return;
        }
        self.started += 1;
        let number = self.started;
        let (outbox, finished) = (outbox.clone(), finished.clone());
        let task = tokio::spawn(async move {
            match work.await {
                Ok(mut messages) => {
                    while let Some(message) = messages.next().await {
                        if !outbox.send(tagged(message, Some(&id))) {
                            return;
                        }
                    }
                }
                Err(reason) => {
                    outbox.send(failure(reason, &id));
                }
            }
            let _ = finished.send((key(&id), number));
        });
        self.open.insert(name, (number, task));
    }

    fn subscribe(&self, subscribe: Subscribe) -> futures::future::BoxFuture<'static, Result<BoxStream<'static, Value>, Failure>> {
        let catalog = self.state.catalog(self.feed.clone());
        Box::pin(async move {
            let table = match (subscribe.table, &subscribe.query) {
                (Some(table), _) => table,
                (None, Some(query)) => query.table_set().tables[0].name.clone(),
                (None, None) => return Err(Failure::new("invalid_request", "subscribe names a table or a query")),
            };
            let source = TableSource::open(table, &catalog)?.with_query_id(subscribe.query)?;
            let description = source.describe().await?;
            let mut params: Vec<(String, String)> = vec![("watch".into(), "true".into()), ("limit".into(), SNAPSHOT_LIMIT.into())];
            params.extend(subscribe.conditions.into_iter().map(|condition| ("where".into(), condition)));
            for (name, value) in
                [("sort", subscribe.sort), ("columns", subscribe.columns), ("namespace", subscribe.namespace), ("count", subscribe.count)]
            {
                params.extend(value.map(|value| (name.to_owned(), value)));
            }
            let query = Query::parse(&params, &description.table)?;
            let resume = subscribe.cursor.map(|cursor| Resume { incarnation: cursor.incarnation, sequence: cursor.sequence });
            let messages = source.messages(description, query, resume).await?;
            Ok(messages.map(body_of).boxed())
        })
    }

    fn fetch_more(&self, query: QueryId) -> futures::future::BoxFuture<'static, Result<BoxStream<'static, Value>, Failure>> {
        let feed = self.feed.clone();
        Box::pin(async move {
            let feed = feed.ok_or_else(|| Failure::new("unavailable", "named queries need the daemon"))?;
            if !feed.holds(&query) {
                return Err(Failure::new("invalid_request", format!("query is not subscribed on this connection: {query}")));
            }
            feed.daemon().fetch_more(&query).await.map_err(|reason| Failure::new("fetch_more", reason))?;
            Ok(futures::stream::empty().boxed())
        })
    }

    fn wait(
        &self,
        subscription: WaitSubscriptionRequest,
    ) -> futures::future::BoxFuture<'static, Result<BoxStream<'static, Value>, Failure>> {
        let feed = self.feed.clone();
        Box::pin(async move {
            let feed = feed.ok_or_else(|| Failure::new("unavailable", "wait subscriptions need the daemon"))?;
            let daemon = Arc::clone(feed.daemon());
            // Each wait is a watcher of its own, so ending the request releases just it.
            let watcher = uuid::Uuid::new_v4();
            // Listen before subscribing, so a leaf already true fires into this stream.
            let events = daemon.subscribe();
            let subscription_id = daemon.subscribe_wait(watcher, subscription).await.map_err(|reason| Failure::new("wait", reason))?;
            let release = WaitRelease { daemon, watcher };
            let fires = futures::stream::unfold(Some((events, release)), move |state| async move {
                let (mut events, release) = state?;
                loop {
                    match events.recv().await {
                        Ok(DaemonEvent::LeafFired(fire)) if fire.watcher_id == watcher && fire.subscription_id == subscription_id => {
                            let mut body = body_of(&fire);
                            if body["type"] != "error" {
                                body["type"] = json!("wait_fired");
                            }
                            return Some((body, Some((events, release))));
                        }
                        Ok(_) => continue,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            let lost = error("wait_lagged", "fires may have been missed; subscribe again", None);
                            return Some((lost, None));
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
                    }
                }
            });
            Ok(fires.boxed())
        })
    }

    fn command(&self, command: String) -> futures::future::BoxFuture<'static, Result<BoxStream<'static, Value>, Failure>> {
        let (state, caller) = (Arc::clone(&self.state), self.caller.clone());
        Box::pin(async move {
            let (Some(daemon), Some(commands)) = (state.daemon.as_ref(), state.commands.as_ref()) else {
                return Err(Failure::new("unavailable", "the command service is unavailable"));
            };
            let progress = commands.observe(daemon, &caller, &command).await.map_err(|(status, message)| {
                let code = match status {
                    StatusCode::NOT_FOUND => "not_found",
                    StatusCode::BAD_REQUEST => "invalid_request",
                    StatusCode::FORBIDDEN | StatusCode::UNAUTHORIZED => "unauthorized",
                    _ => "command",
                };
                Failure::new(code, message)
            })?;
            Ok(progress.map(body_of).boxed())
        })
    }
}

/// Releases one wait subscription when its request ends, by cancel or by connection close.
struct WaitRelease {
    daemon: Arc<flotilla_core::in_process::InProcessDaemon>,
    watcher: uuid::Uuid,
}

impl Drop for WaitRelease {
    fn drop(&mut self) {
        let (daemon, watcher) = (Arc::clone(&self.daemon), self.watcher);
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move { daemon.unsubscribe_waits(watcher).await });
        }
    }
}
