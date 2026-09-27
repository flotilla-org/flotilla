use std::{collections::HashMap, time::Duration};

use flotilla_relay_protocol::{github_hints, verify_github_signature, ConsumerFrame, Hint, Mailbox, StreamFrame};
use serde::Deserialize;
use subtle::ConstantTimeEq;
use worker::*;

use crate::route::{self, Method as RouteMethod, Route, RouteError};

const MAX_PAYLOAD_BYTES: usize = 1024 * 1024;

#[derive(Deserialize)]
struct InstallConfig {
    consumer_token: String,
    sources: HashMap<String, String>,
}

fn install_config(env: &Env, install: &str) -> Result<Option<InstallConfig>> {
    let mut configs: HashMap<String, InstallConfig> = serde_json::from_str(&env.secret("RELAY_INSTALLS")?.to_string())
        .map_err(|error| Error::RustError(format!("invalid RELAY_INSTALLS: {error}")))?;
    Ok(configs.remove(install))
}

fn bearer_token(req: &Request) -> Result<Option<String>> {
    Ok(req.headers().get("Authorization")?.and_then(|value| value.strip_prefix("Bearer ").map(str::to_owned)))
}

fn authorized(req: &Request, expected: &str) -> Result<bool> {
    let Some(actual) = bearer_token(req)? else { return Ok(false) };
    Ok(bool::from(actual.as_bytes().ct_eq(expected.as_bytes())))
}

fn internal_request(method: Method, path: &str, body: Option<String>, cursor: Option<u64>, websocket: bool) -> Result<Request> {
    let mut init = RequestInit::new();
    init.with_method(method);
    if let Some(body) = body {
        init.with_body(Some(js_sys::JsString::from(body).into()));
    }
    let headers = Headers::new();
    if websocket {
        headers.set("Upgrade", "websocket")?;
    }
    init.with_headers(headers);
    let suffix = cursor.map_or(String::new(), |cursor| format!("?cursor={cursor}"));
    Request::new_with_init(&format!("https://relay.internal/{path}{suffix}"), &init)
}

#[event(fetch)]
async fn main(mut req: Request, env: Env, _ctx: Context) -> Result<Response> {
    let path = req.path();
    let method = match req.method() {
        Method::Get => RouteMethod::Get,
        Method::Post => RouteMethod::Post,
        _ => RouteMethod::Other,
    };
    let route = match route::parse(method, &path) {
        Ok(route) => route,
        Err(RouteError::InvalidInstall) => return Response::error("invalid install", 400),
        Err(RouteError::NotFound) => return Response::error("not found", 404),
    };
    let install = route.install();
    let Some(config) = install_config(&env, install)? else { return Response::error("unknown install", 404) };
    let namespace = env.durable_object("MAILBOX")?;
    let stub = namespace.id_from_name(install)?.get_stub()?;
    match route {
        Route::Ingress { source, .. } => {
            let Some(secret) = config.sources.get(source) else { return Response::error("unknown source", 404) };
            if source != "github" {
                return Response::error("unsupported source", 400);
            }
            let Some(signature) = req.headers().get("X-Hub-Signature-256")? else { return Response::error("missing signature", 401) };
            let Some(event) = req.headers().get("X-GitHub-Event")? else { return Response::error("missing event", 400) };
            let Some(delivery_id) = req.headers().get("X-GitHub-Delivery")? else { return Response::error("missing delivery id", 400) };
            let payload = req.bytes().await?;
            if payload.len() > MAX_PAYLOAD_BYTES {
                return Response::error("payload too large", 413);
            }
            if !verify_github_signature(secret, &payload, &signature) {
                return Response::error("invalid signature", 401);
            }
            let hints = match github_hints(&event, &delivery_id, &payload) {
                Ok(hints) if hints.is_empty() => return Response::empty(),
                Ok(hints) => hints,
                Err(_) => return Response::error("invalid event", 400),
            };
            // Only the reduced hint crosses into persistent Durable Object storage.
            let body = serde_json::to_string(&hints).map_err(|error| Error::RustError(error.to_string()))?;
            let request = internal_request(Method::Post, "append", Some(body), None, false)?;
            stub.fetch_with_request(request).await
        }
        Route::Stream { .. } | Route::Ack { .. } => {
            if !authorized(&req, &config.consumer_token)? {
                return Response::error("unauthorized", 401);
            }
            let cursor = req.url()?.query_pairs().find(|(key, _)| key == "cursor").and_then(|(_, value)| value.parse::<u64>().ok());
            if req.method() == Method::Get && cursor.is_none() {
                return Response::error("cursor required", 400);
            }
            let websocket = req.headers().get("Upgrade")?.is_some_and(|value| value.eq_ignore_ascii_case("websocket"));
            let method = req.method();
            let body = if method == Method::Post { Some(req.text().await?) } else { None };
            let internal = internal_request(method, if body.is_some() { "ack" } else { "stream" }, body, cursor, websocket)?;
            stub.fetch_with_request(internal).await
        }
    }
}

#[durable_object]
pub struct MailboxObject {
    state: State,
}

impl DurableObject for MailboxObject {
    fn new(state: State, _env: Env) -> Self {
        Self { state }
    }

    async fn fetch(&self, mut req: Request) -> Result<Response> {
        match (req.method(), req.path().as_str()) {
            (Method::Post, "/append") => {
                let hints: Vec<Hint> = req.json().await?;
                let mut mailbox = self.load().await?;
                let deliveries: Vec<_> = hints.into_iter().map(|hint| mailbox.append(hint, Date::now().as_millis())).collect();
                self.save(&mailbox).await?;
                for socket in self.state.get_websockets() {
                    for delivery in &deliveries {
                        let frame = StreamFrame::Hint { delivery: delivery.clone() };
                        let message = serde_json::to_string(&frame).map_err(|error| Error::RustError(error.to_string()))?;
                        let _ = socket.send_with_str(&message);
                    }
                }
                Response::from_json(&deliveries)
            }
            (Method::Post, "/ack") => {
                let frame: ConsumerFrame = req.json().await?;
                let mailbox = self.load().await?;
                let ConsumerFrame::Ack { cursor } = frame;
                match mailbox.ack(cursor) {
                    Ok(frame) => Response::from_json(&frame),
                    Err(message) => Response::error(message, 400),
                }
            }
            (Method::Get, "/stream") => {
                let Some(cursor) =
                    req.url()?.query_pairs().find(|(key, _)| key == "cursor").and_then(|(_, value)| value.parse::<u64>().ok())
                else {
                    return Response::error("cursor required", 400);
                };
                let websocket = req.headers().get("Upgrade")?.is_some_and(|value| value.eq_ignore_ascii_case("websocket"));
                if websocket {
                    let pair = WebSocketPair::new()?;
                    self.state.accept_web_socket(&pair.server);
                    let frames = self.read(cursor).await?;
                    if frames.is_empty() {
                        let mailbox = self.load().await?;
                        send_frame(&pair.server, &StreamFrame::Ready { cursor: mailbox.latest_cursor() })?;
                    } else {
                        for frame in frames {
                            send_frame(&pair.server, &frame)?;
                        }
                    }
                    Response::from_websocket(pair.client)
                } else {
                    // Hold an empty poll briefly; each poll re-reads durable state.
                    for _ in 0..20 {
                        let frames = self.read(cursor).await?;
                        if !frames.is_empty() {
                            return Response::from_json(&frames);
                        }
                        Delay::from(Duration::from_secs(1)).await;
                    }
                    Response::from_json(&Vec::<StreamFrame>::new())
                }
            }
            _ => Response::error("not found", 404),
        }
    }

    async fn websocket_message(&self, ws: WebSocket, message: WebSocketIncomingMessage) -> Result<()> {
        let WebSocketIncomingMessage::String(text) = message else { return Ok(()) };
        let frame: ConsumerFrame = serde_json::from_str(&text).map_err(|error| Error::RustError(error.to_string()))?;
        let ConsumerFrame::Ack { cursor } = frame;
        let mailbox = self.load().await?;
        match mailbox.ack(cursor) {
            Ok(reply) => send_frame(&ws, &reply),
            Err(message) => send_frame(&ws, &StreamFrame::Error { message: message.into() }),
        }
    }

    async fn websocket_close(&self, _ws: WebSocket, _code: usize, _reason: String, _clean: bool) -> Result<()> {
        Ok(())
    }
    async fn websocket_error(&self, _ws: WebSocket, _error: Error) -> Result<()> {
        Ok(())
    }
}

impl MailboxObject {
    async fn load(&self) -> Result<Mailbox> {
        Ok(self.state.storage().get("mailbox").await?.unwrap_or_default())
    }
    async fn save(&self, mailbox: &Mailbox) -> Result<()> {
        self.state.storage().put("mailbox", mailbox).await
    }
    async fn read(&self, cursor: u64) -> Result<Vec<StreamFrame>> {
        let mut mailbox = self.load().await?;
        let before = mailbox.retained_count();
        let frames = mailbox.read(cursor, Date::now().as_millis());
        if mailbox.retained_count() != before {
            self.save(&mailbox).await?;
        }
        Ok(frames)
    }
}

fn send_frame(ws: &WebSocket, frame: &StreamFrame) -> Result<()> {
    let message = serde_json::to_string(frame).map_err(|error| Error::RustError(error.to_string()))?;
    ws.send_with_str(message)
}
