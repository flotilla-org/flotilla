//! Workers glue: the public Worker routes requests to the install's Durable Object, which holds
//! that install's credentials and mailbox in its own SQLite storage.
use std::{cell::RefCell, time::Duration};

use flotilla_relay_protocol::{github, StreamFrame};
use futures::{
    channel::oneshot,
    future::{select, Either},
    StreamExt,
};
use worker::*;

use crate::{
    route::{self, Method as RouteMethod, Route, RouteError},
    service::{self, AdminOutcome, Disconnect, Ingress, IngressHeaders, Reply},
    store::{RetentionPolicy, Sql, SqlValue, Store, StoreError, StoreResult},
};

fn route_method(method: Method) -> RouteMethod {
    match method {
        Method::Get => RouteMethod::Get,
        Method::Post => RouteMethod::Post,
        Method::Delete => RouteMethod::Delete,
        _ => RouteMethod::Other,
    }
}

fn unauthorized() -> Result<Response> {
    Response::error("unauthorized", 401)
}

fn optional_setting(env: &Env, name: &str) -> Option<String> {
    env.var(name).ok().map(|value| value.to_string())
}

#[event(fetch)]
async fn main(req: Request, env: Env, _ctx: Context) -> Result<Response> {
    let path = req.path();
    let route = match route::parse(route_method(req.method()), &path) {
        Ok(route) => route,
        Err(RouteError::InvalidInstall) => return Response::error("invalid install", 400),
        Err(RouteError::NotFound) => return Response::error("not found", 404),
    };
    match route {
        Route::Admin { .. } => {
            let expected = optional_setting(&env, "RELAY_OPERATOR_TOKEN_SHA256");
            if !service::operator_authorized(expected.as_deref(), req.headers().get("Authorization")?.as_deref()) {
                return unauthorized();
            }
        }
        Route::Ingress { source, .. } => {
            if let Err(outcome) = service::source(source) {
                return reply(outcome);
            }
        }
        Route::Ack { .. } | Route::Stream { .. } => {}
    }
    if service::body_too_large(route, req.headers().get("Content-Length")?.as_deref()) {
        return Response::error("payload too large", 413);
    }
    let stub = env.durable_object("MAILBOX")?.id_from_name(route.install())?.get_stub()?;
    stub.fetch_with_request(req).await
}

/// The Durable Object's SQLite storage behind the store's [`Sql`] seam.
struct DurableSql(SqlStorage);

impl Sql for DurableSql {
    fn exec(&self, query: &str, params: &[SqlValue]) -> StoreResult<Vec<Vec<SqlValue>>> {
        let bindings = params
            .iter()
            .map(|value| match value {
                SqlValue::Null => SqlStorageValue::Null,
                SqlValue::Integer(value) => SqlStorageValue::Integer(*value),
                SqlValue::Text(value) => SqlStorageValue::String(value.clone()),
            })
            .collect::<Vec<_>>();
        let cursor = self.0.exec(query, bindings).map_err(|error| StoreError(error.to_string()))?;
        cursor
            .raw()
            .map(|row| {
                row.map_err(|error| StoreError(error.to_string()))?
                    .into_iter()
                    .map(|value| match value {
                        SqlStorageValue::Null => Ok(SqlValue::Null),
                        SqlStorageValue::Integer(value) => Ok(SqlValue::Integer(value)),
                        SqlStorageValue::Boolean(value) => Ok(SqlValue::Integer(value.into())),
                        SqlStorageValue::String(value) => Ok(SqlValue::Text(value)),
                        other => Err(StoreError(format!("unsupported SQL value {other:?}"))),
                    })
                    .collect()
            })
            .collect()
    }
}

fn store_error(error: StoreError) -> Error {
    Error::RustError(error.0)
}

fn now_ms() -> u64 {
    Date::now().as_millis()
}

fn random(buffer: &mut [u8]) {
    getrandom::getrandom(buffer).expect("Workers provide crypto.getRandomValues");
}

fn query_u64(req: &Request, name: &str) -> Result<Option<u64>> {
    Ok(req.url()?.query_pairs().find(|(key, _)| key == name).and_then(|(_, value)| value.parse().ok()))
}

/// Reads at most `limit` bytes of body, or `None` when the body is larger.
async fn read_limited(req: &mut Request, limit: usize) -> Result<Option<Vec<u8>>> {
    let mut body = service::LimitedBody::new(limit);
    // A bodyless request (an admin POST without a supplied secret) has no stream to read.
    if req.inner().body().is_none() {
        return Ok(Some(body.into_bytes()));
    }
    let mut stream = req.stream()?;
    while let Some(chunk) = stream.next().await {
        if !body.push(&chunk?) {
            return Ok(None);
        }
    }
    Ok(Some(body.into_bytes()))
}

fn frame_text(frame: &StreamFrame) -> Result<String> {
    serde_json::to_string(frame).map_err(|error| Error::RustError(error.to_string()))
}

fn send_frame(ws: &WebSocket, frame: &StreamFrame) -> Result<()> {
    ws.send_with_str(frame_text(frame)?)
}

fn reply(reply: Reply) -> Result<Response> {
    match reply {
        Reply::Json { status, body } => {
            let headers = Headers::new();
            headers.set("Content-Type", "application/json")?;
            Ok(Response::ok(body)?.with_status(status).with_headers(headers))
        }
        Reply::Empty => Ok(Response::empty()?.with_status(204)),
        Reply::Error { status, message } => Response::error(message, status),
    }
}

#[durable_object]
pub struct MailboxObject {
    state: State,
    store: std::result::Result<Store<DurableSql>, String>,
    /// Long-polls waiting for the next append. In memory only: a pending request keeps the
    /// object resident, and a restarted object has no pending requests.
    waiters: RefCell<Vec<oneshot::Sender<()>>>,
}

impl DurableObject for MailboxObject {
    fn new(state: State, env: Env) -> Self {
        let retention = optional_setting(&env, "RELAY_RETENTION_SECS");
        let cap = optional_setting(&env, "RELAY_SUBJECT_CAP");
        let store = RetentionPolicy::from_settings(retention.as_deref(), cap.as_deref())
            .and_then(|policy| Store::open(DurableSql(state.storage().sql()), policy).map_err(|error| error.0));
        Self { state, store, waiters: RefCell::new(Vec::new()) }
    }

    async fn fetch(&self, mut req: Request) -> Result<Response> {
        let store = match &self.store {
            Ok(store) => store,
            Err(message) => return Response::error(format!("relay misconfigured: {message}"), 500),
        };
        let path = req.path();
        let Ok(route) = route::parse(route_method(req.method()), &path) else { return Response::error("not found", 404) };
        let authorization = req.headers().get("Authorization")?;
        match route {
            Route::Admin { install, op } => {
                let Some(body) = read_limited(&mut req, service::body_limit(route).expect("admin body limit")).await? else {
                    return Response::error("payload too large", 413);
                };
                let AdminOutcome { reply: outcome, disconnect } =
                    service::admin(store, install, op, &body, now_ms(), &mut random).map_err(store_error)?;
                let sockets = match disconnect {
                    Disconnect::None => Vec::new(),
                    Disconnect::Token(id) => self.state.get_websockets_with_tag(&id),
                    Disconnect::All => self.state.get_websockets(),
                };
                for socket in sockets {
                    let _ = socket.close(Some(1008), Some("credential revoked"));
                }
                reply(outcome)
            }
            Route::Ingress { source, .. } => {
                let Ok(source) = service::source(source) else { return Response::error("unknown source", 404) };
                let header = |name| req.headers().get(name);
                let (signature, event, delivery_id) =
                    (header(github::SIGNATURE_HEADER)?, header(github::EVENT_HEADER)?, header(github::DELIVERY_HEADER)?);
                let Some(payload) = read_limited(&mut req, service::body_limit(route).expect("ingress body limit")).await? else {
                    return Response::error("payload too large", 413);
                };
                let headers =
                    IngressHeaders { signature: signature.as_deref(), event: event.as_deref(), delivery_id: delivery_id.as_deref() };
                match service::ingress(store, source, headers, &payload, now_ms()).map_err(store_error)? {
                    Ingress::Unauthorized => unauthorized(),
                    Ingress::BadRequest(message) => Response::error(message, 400),
                    Ingress::Accepted(deliveries) => {
                        if !deliveries.is_empty() {
                            self.broadcast(&deliveries)?;
                        }
                        Response::from_json(&deliveries)
                    }
                }
            }
            Route::Stream { .. } => {
                let Some(token_id) = service::consumer_token_id(store, authorization.as_deref()).map_err(store_error)? else {
                    return unauthorized();
                };
                let Some(cursor) = query_u64(&req, "cursor")? else { return Response::error("cursor required", 400) };
                let websocket = req.headers().get("Upgrade")?.is_some_and(|value| value.eq_ignore_ascii_case("websocket"));
                if websocket {
                    let pair = WebSocketPair::new()?;
                    // Tagged with the token id so revoking the token closes the connection.
                    self.state.accept_websocket_with_tags(&pair.server, &[&token_id]);
                    let frames = service::websocket_replay(store, cursor, now_ms()).map_err(store_error)?;
                    for frame in &frames {
                        send_frame(&pair.server, frame)?;
                    }
                    Response::from_websocket(pair.client)
                } else {
                    let wait = Duration::from_secs(service::wait_secs(query_u64(&req, "wait")?));
                    let frames = store.read(cursor, now_ms()).map_err(store_error)?;
                    if !frames.is_empty() || wait.is_zero() {
                        return Response::from_json(&frames);
                    }
                    let (notify, woken) = oneshot::channel();
                    {
                        let mut waiters = self.waiters.borrow_mut();
                        // Drop waiters whose polls already timed out or disconnected.
                        waiters.retain(|waiter| !waiter.is_canceled());
                        waiters.push(notify);
                    }
                    match select(woken, Delay::from(wait)).await {
                        Either::Left(_) => Response::from_json(&store.read(cursor, now_ms()).map_err(store_error)?),
                        Either::Right((_, woken)) => {
                            // Release this poll's waiter now rather than at the next poll or append.
                            drop(woken);
                            self.waiters.borrow_mut().retain(|waiter| !waiter.is_canceled());
                            Response::from_json(&Vec::<StreamFrame>::new())
                        }
                    }
                }
            }
            Route::Ack { .. } => {
                if service::consumer_token_id(store, authorization.as_deref()).map_err(store_error)?.is_none() {
                    return unauthorized();
                }
                let Some(body) = read_limited(&mut req, service::body_limit(route).expect("ack body limit")).await? else {
                    return Response::error("payload too large", 413);
                };
                match service::ack(store, &body).map_err(store_error)? {
                    Ok(frame) => Response::from_json(&frame),
                    Err(message) => Response::error(message, 400),
                }
            }
        }
    }

    async fn websocket_message(&self, ws: WebSocket, message: WebSocketIncomingMessage) -> Result<()> {
        let Ok(store) = &self.store else { return Ok(()) };
        let WebSocketIncomingMessage::String(text) = message else { return Ok(()) };
        let reply = service::websocket_ack(store, &text).map_err(store_error)?;
        send_frame(&ws, &reply)
    }

    async fn websocket_close(&self, _ws: WebSocket, _code: usize, _reason: String, _clean: bool) -> Result<()> {
        Ok(())
    }

    async fn websocket_error(&self, _ws: WebSocket, _error: Error) -> Result<()> {
        Ok(())
    }
}

impl MailboxObject {
    /// Pushes new deliveries to connected websockets and wakes pending long-polls.
    fn broadcast(&self, deliveries: &[flotilla_relay_protocol::Delivery]) -> Result<()> {
        let messages = service::live_frames(deliveries).iter().map(frame_text).collect::<Result<Vec<_>>>()?;
        for socket in self.state.get_websockets() {
            for message in &messages {
                let _ = socket.send_with_str(message);
            }
        }
        for waiter in self.waiters.borrow_mut().drain(..) {
            let _ = waiter.send(());
        }
        Ok(())
    }
}
