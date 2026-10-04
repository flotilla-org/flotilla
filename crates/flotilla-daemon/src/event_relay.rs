//! Outbound consumer of ADR 0041 mailboxes. Transport is kept behind a small
//! session seam so the state machine can be exercised without a network socket.
use std::{collections::VecDeque, path::PathBuf, sync::Arc, time::Duration};

use async_trait::async_trait;
use flotilla_core::{config::RelayConfig, in_process::InProcessDaemon};
use flotilla_relay_protocol::{ConsumerFrame, StreamFrame, Subject};
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{client::IntoClientRequest, http::HeaderValue, Message},
};
use tracing::warn;

#[async_trait]
trait RelaySession: Send {
    async fn next(&mut self) -> Result<StreamFrame, String>;
    async fn ack(&mut self, cursor: u64) -> Result<(), String>;
}

#[async_trait]
trait RelayConnector: Send + Sync {
    async fn connect(&self, cursor: u64) -> Result<Box<dyn RelaySession>, String>;
}

#[async_trait]
trait RefreshTarget: Send + Sync {
    async fn hint(&self, subject: &Subject) -> Result<(), String>;
    async fn resync(&self) -> Result<(), String>;
    fn healthy(&self, healthy: bool);
}

struct DaemonRefreshTarget {
    daemon: Arc<InProcessDaemon>,
}

#[async_trait]
impl RefreshTarget for DaemonRefreshTarget {
    async fn hint(&self, subject: &Subject) -> Result<(), String> {
        self.daemon.refresh_change_request_hint(subject).await
    }

    async fn resync(&self) -> Result<(), String> {
        self.daemon.refresh_demanded_owned_change_requests().await
    }
    fn healthy(&self, healthy: bool) {
        self.daemon.set_change_request_relay_healthy(healthy);
    }
}

#[derive(Serialize, Deserialize)]
struct SavedCursor {
    install_id: String,
    cursor: u64,
}

struct CursorFile {
    path: PathBuf,
    install_id: String,
}

impl CursorFile {
    fn load(&self) -> Result<u64, String> {
        match std::fs::read(&self.path) {
            Ok(bytes) => {
                let saved: SavedCursor = serde_json::from_slice(&bytes).map_err(|e| format!("decode relay cursor: {e}"))?;
                Ok(if saved.install_id == self.install_id { saved.cursor } else { 0 })
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(0),
            Err(error) => Err(format!("read relay cursor: {error}")),
        }
    }

    fn save(&self, cursor: u64) -> Result<(), String> {
        let parent = self.path.parent().ok_or("relay cursor path has no parent")?;
        std::fs::create_dir_all(parent).map_err(|e| format!("create relay state directory: {e}"))?;
        let temporary = self.path.with_extension(format!("tmp-{}", uuid::Uuid::new_v4()));
        let body = serde_json::to_vec(&SavedCursor { install_id: self.install_id.clone(), cursor })
            .map_err(|e| format!("encode relay cursor: {e}"))?;
        std::fs::write(&temporary, body).map_err(|e| format!("write relay cursor: {e}"))?;
        std::fs::rename(&temporary, &self.path).map_err(|e| format!("replace relay cursor: {e}"))
    }
}

struct RelayClient {
    connector: Arc<dyn RelayConnector>,
    target: Arc<dyn RefreshTarget>,
    cursor_file: CursorFile,
}

impl RelayClient {
    async fn run(self) {
        let mut cursor = match self.cursor_file.load() {
            Ok(cursor) => cursor,
            Err(error) => {
                warn!(%error, "relay cursor unavailable; starting from zero");
                0
            }
        };
        let mut delay = Duration::from_secs(1);
        loop {
            match self.connector.connect(cursor).await {
                Ok(mut session) => {
                    let mut healthy = false;
                    loop {
                        match session.next().await {
                            Ok(frame) => {
                                if !healthy {
                                    self.target.healthy(true);
                                    healthy = true;
                                    delay = Duration::from_secs(1);
                                }
                                let gap = matches!(frame, StreamFrame::Gap { .. });
                                match self.process(frame, &mut cursor, &mut *session).await {
                                    Ok(()) => {}
                                    Err(error) => {
                                        warn!(%error, "relay frame processing failed");
                                        break;
                                    }
                                }
                                if gap {
                                    healthy = false;
                                }
                            }
                            Err(error) => {
                                warn!(%error, "relay connection lost");
                                break;
                            }
                        }
                    }
                    self.target.healthy(false);
                    if healthy {
                        if let Err(error) = self.target.resync().await {
                            warn!(%error, "relay disconnect resync failed");
                        }
                    }
                }
                Err(error) => {
                    warn!(%error, "relay connection failed");
                }
            }
            tokio::time::sleep(delay).await;
            delay = (delay * 2).min(Duration::from_secs(60));
        }
    }

    async fn process(&self, frame: StreamFrame, cursor: &mut u64, session: &mut dyn RelaySession) -> Result<(), String> {
        match frame {
            StreamFrame::Hint { delivery } => {
                if delivery.cursor <= *cursor {
                    return Ok(());
                }
                if let Ok(subject) = delivery.hint.subject.parse::<Subject>() {
                    if let Err(error) = self.target.hint(&subject).await {
                        warn!(%error, subject = %subject, "relay hint refresh failed; polling will retry");
                    }
                }
                self.advance(delivery.cursor, cursor, session).await?;
            }
            StreamFrame::Gap { latest_cursor, .. } => {
                self.target.healthy(false);
                self.target.resync().await?;
                self.advance(latest_cursor, cursor, session).await?;
            }
            StreamFrame::Ready { .. } | StreamFrame::Acked { .. } => {}
            StreamFrame::Error { message } => return Err(message),
        }
        Ok(())
    }

    async fn advance(&self, next: u64, cursor: &mut u64, session: &mut dyn RelaySession) -> Result<(), String> {
        self.cursor_file.save(next)?;
        *cursor = next;
        session.ack(next).await
    }
}

struct HttpRelayConnector {
    endpoint: url::Url,
    install_id: String,
    token_file: PathBuf,
    client: reqwest::Client,
}

impl HttpRelayConnector {
    fn new(config: RelayConfig) -> Result<Self, String> {
        let endpoint = url::Url::parse(&config.endpoint).map_err(|e| format!("invalid relay endpoint: {e}"))?;
        if !matches!(endpoint.scheme(), "https" | "http") {
            return Err("relay endpoint must be http or https".into());
        }
        if config.install_id.is_empty() || config.install_id.contains('/') {
            return Err("invalid relay install id".into());
        }
        Ok(Self { endpoint, install_id: config.install_id, token_file: config.consumer_token_file, client: reqwest::Client::new() })
    }

    fn stream_url(&self, cursor: u64) -> Result<url::Url, String> {
        let mut url = self.endpoint.clone();
        url.path_segments_mut()
            .map_err(|_| "relay endpoint cannot be a base URL".to_string())?
            .pop_if_empty()
            .push("i")
            .push(&self.install_id)
            .push("stream");
        url.query_pairs_mut().append_pair("cursor", &cursor.to_string());
        Ok(url)
    }

    fn token(&self) -> Result<String, String> {
        let token = std::fs::read_to_string(&self.token_file).map_err(|e| format!("read relay consumer credential: {e}"))?;
        let token = token.trim().to_string();
        if token.is_empty() {
            return Err("relay consumer credential is empty".into());
        }
        Ok(token)
    }
}

#[async_trait]
impl RelayConnector for HttpRelayConnector {
    async fn connect(&self, cursor: u64) -> Result<Box<dyn RelaySession>, String> {
        let token = self.token()?;
        let mut ws_url = self.stream_url(cursor)?;
        ws_url.set_scheme(if ws_url.scheme() == "https" { "wss" } else { "ws" }).map_err(|_| "invalid websocket scheme")?;
        let mut request = ws_url.as_str().into_client_request().map_err(|e| e.to_string())?;
        request.headers_mut().insert("Authorization", HeaderValue::from_str(&format!("Bearer {token}")).map_err(|e| e.to_string())?);
        match tokio::time::timeout(Duration::from_secs(10), connect_async(request)).await {
            Ok(Ok((socket, _))) => Ok(Box::new(WebsocketSession { socket })),
            result => {
                let error = match result {
                    Ok(Err(error)) => error.to_string(),
                    Err(_) => "websocket handshake timed out".to_string(),
                    Ok(Ok(_)) => unreachable!(),
                };
                tracing::debug!(%error, "relay websocket unavailable; using long poll");
                Ok(Box::new(LongPollSession {
                    client: self.client.clone(),
                    url: self.stream_url(cursor)?,
                    token,
                    pending: VecDeque::new(),
                    cursor,
                }))
            }
        }
    }
}

type Websocket = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;
struct WebsocketSession {
    socket: Websocket,
}

#[async_trait]
impl RelaySession for WebsocketSession {
    async fn next(&mut self) -> Result<StreamFrame, String> {
        loop {
            let message = match tokio::time::timeout(Duration::from_secs(45), self.socket.next()).await {
                Ok(message) => message,
                Err(_) => {
                    self.socket.send(Message::Ping(Vec::new().into())).await.map_err(|e| e.to_string())?;
                    tokio::time::timeout(Duration::from_secs(10), self.socket.next())
                        .await
                        .map_err(|_| "relay websocket heartbeat timed out".to_string())?
                }
            };
            match message {
                Some(Ok(Message::Text(text))) => return serde_json::from_str(&text).map_err(|e| e.to_string()),
                Some(Ok(Message::Close(_))) | None => return Err("websocket closed".into()),
                Some(Ok(_)) => {}
                Some(Err(error)) => return Err(error.to_string()),
            }
        }
    }
    async fn ack(&mut self, cursor: u64) -> Result<(), String> {
        let text = serde_json::to_string(&ConsumerFrame::Ack { cursor }).map_err(|e| e.to_string())?;
        self.socket.send(Message::Text(text.into())).await.map_err(|e| e.to_string())
    }
}

struct LongPollSession {
    client: reqwest::Client,
    url: url::Url,
    token: String,
    pending: VecDeque<StreamFrame>,
    cursor: u64,
}

#[async_trait]
impl RelaySession for LongPollSession {
    async fn next(&mut self) -> Result<StreamFrame, String> {
        if let Some(frame) = self.pending.pop_front() {
            return Ok(frame);
        }
        self.url.query_pairs_mut().clear().append_pair("cursor", &self.cursor.to_string()).append_pair("wait", "20");
        let response = self
            .client
            .get(self.url.clone())
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| e.to_string())?
            .error_for_status()
            .map_err(|e| e.to_string())?;
        self.pending = response.json::<VecDeque<StreamFrame>>().await.map_err(|e| e.to_string())?;
        Ok(self.pending.pop_front().unwrap_or(StreamFrame::Ready { cursor: self.cursor }))
    }

    async fn ack(&mut self, cursor: u64) -> Result<(), String> {
        let mut ack_url = self.url.clone();
        ack_url.set_query(None);
        ack_url.path_segments_mut().map_err(|_| "invalid ack URL")?.push("ack");
        self.client
            .post(ack_url)
            .bearer_auth(&self.token)
            .json(&ConsumerFrame::Ack { cursor })
            .send()
            .await
            .map_err(|e| e.to_string())?
            .error_for_status()
            .map_err(|e| e.to_string())?;
        self.cursor = cursor;
        Ok(())
    }
}

pub(crate) fn spawn(daemon: Arc<InProcessDaemon>, config: RelayConfig, state_dir: PathBuf) -> Result<tokio::task::JoinHandle<()>, String> {
    let install_id = config.install_id.clone();
    let connector = Arc::new(HttpRelayConnector::new(config)?);
    let target = Arc::new(DaemonRefreshTarget { daemon });
    let cursor_file = CursorFile { path: state_dir.join("relay-cursor.json"), install_id };
    Ok(tokio::spawn(RelayClient { connector, target, cursor_file }.run()))
}

#[cfg(test)]
mod tests {
    // HTTP audit (#1512): relay long-poll GET/ack POST and WebSocket upgrade
    // implement Flotilla's own ConsumerFrame/StreamFrame protocol, not an external
    // service with separately documented header requirements. The in-memory relay
    // scenarios below cover that protocol; there is no live replay fixture or
    // external-service stand-in for this owned boundary. HTTP transport coverage
    // remains separate from these lifecycle scenarios.
    use std::{collections::HashSet, sync::Mutex};

    use flotilla_relay_protocol::{Delivery, Hint, SubjectKind};

    use super::*;

    #[derive(Default)]
    struct MemoryTarget {
        owned: HashSet<Subject>,
        failing: HashSet<Subject>,
        refreshed: Mutex<Vec<Subject>>,
        resyncs: Mutex<usize>,
        health: Mutex<Vec<bool>>,
    }

    #[async_trait]
    impl RefreshTarget for MemoryTarget {
        async fn hint(&self, subject: &Subject) -> Result<(), String> {
            if self.failing.contains(subject) {
                return Err("forge unavailable".into());
            }
            if self.owned.contains(subject) {
                self.refreshed.lock().expect("refreshes").push(subject.clone());
            }
            Ok(())
        }
        async fn resync(&self) -> Result<(), String> {
            *self.resyncs.lock().expect("resyncs") += 1;
            Ok(())
        }
        fn healthy(&self, healthy: bool) {
            self.health.lock().expect("health").push(healthy);
        }
    }

    struct MemoryRelay {
        horizon: u64,
        latest: u64,
        deliveries: Vec<Delivery>,
        acks: Arc<Mutex<Vec<u64>>>,
    }

    struct MemorySession {
        frames: VecDeque<StreamFrame>,
        acks: Arc<Mutex<Vec<u64>>>,
    }

    #[async_trait]
    impl RelaySession for MemorySession {
        async fn next(&mut self) -> Result<StreamFrame, String> {
            self.frames.pop_front().ok_or_else(|| "relay disconnected".into())
        }
        async fn ack(&mut self, cursor: u64) -> Result<(), String> {
            self.acks.lock().expect("acks").push(cursor);
            Ok(())
        }
    }

    fn hint(cursor: u64, subject: &str) -> Delivery {
        Delivery {
            cursor,
            hint: Hint { source: "github".into(), subject: subject.into(), kind: "pull_request".into(), delivery_id: cursor.to_string() },
        }
    }

    #[async_trait]
    impl RelayConnector for MemoryRelay {
        async fn connect(&self, cursor: u64) -> Result<Box<dyn RelaySession>, String> {
            let frames = if cursor < self.horizon {
                VecDeque::from([StreamFrame::Gap { oldest_cursor: self.deliveries.first().map(|d| d.cursor), latest_cursor: self.latest }])
            } else {
                let mut frames = self
                    .deliveries
                    .iter()
                    .filter(|delivery| delivery.cursor > cursor)
                    .cloned()
                    .map(|delivery| StreamFrame::Hint { delivery })
                    .collect::<VecDeque<_>>();
                if frames.is_empty() {
                    frames.push_back(StreamFrame::Ready { cursor });
                }
                frames
            };
            Ok(Box::new(MemorySession { frames, acks: Arc::clone(&self.acks) }))
        }
    }

    #[tokio::test]
    async fn owned_hint_is_normalized_and_acted_on_once() {
        let subject = Subject::github(SubjectKind::ChangeRequest, "flotilla-org", "flotilla", 2051);
        let target = Arc::new(MemoryTarget { owned: HashSet::from([subject.clone()]), ..Default::default() });
        let acks = Arc::new(Mutex::new(Vec::new()));
        let relay = Arc::new(MemoryRelay {
            horizon: 0,
            latest: 2,
            deliveries: vec![hint(1, "cr/GitHub.com/Flotilla-Org/Flotilla/2051"), hint(2, "cr/github.com/other/repo/10")],
            acks: Arc::clone(&acks),
        });
        let dir = tempfile::tempdir().expect("tempdir");
        let cursor_file = CursorFile { path: dir.path().join("cursor"), install_id: "test".into() };
        let client = RelayClient { connector: relay.clone(), target: target.clone(), cursor_file };
        let mut session = relay.connect(0).await.expect("connect");
        let mut cursor = 0;
        let frame = session.next().await.expect("hint");
        client.process(frame.clone(), &mut cursor, &mut *session).await.expect("hint");
        client.process(frame, &mut cursor, &mut *session).await.expect("duplicate");
        let frame = session.next().await.expect("unowned hint");
        client.process(frame, &mut cursor, &mut *session).await.expect("unowned hint");
        assert_eq!(*target.refreshed.lock().expect("refreshes"), vec![subject]);
        assert_eq!(*acks.lock().expect("acks"), vec![1, 2]);
        assert_eq!(client.cursor_file.load().expect("persisted cursor"), 2);
    }

    #[tokio::test]
    async fn expired_cursor_gaps_and_resyncs_before_ack() {
        let target = Arc::new(MemoryTarget::default());
        let acks = Arc::new(Mutex::new(Vec::new()));
        let relay = Arc::new(MemoryRelay {
            horizon: 8,
            latest: 12,
            deliveries: vec![hint(8, "cr/github.com/other/repo/10")],
            acks: Arc::clone(&acks),
        });
        let dir = tempfile::tempdir().expect("tempdir");
        let cursor_file = CursorFile { path: dir.path().join("cursor"), install_id: "test".into() };
        cursor_file.save(4).expect("initial cursor");
        let client = RelayClient { connector: relay.clone(), target: target.clone(), cursor_file };
        let mut cursor = client.cursor_file.load().expect("cursor");
        let mut session = relay.connect(cursor).await.expect("reconnect");
        let frame = session.next().await.expect("gap");
        client.process(frame, &mut cursor, &mut *session).await.expect("gap");
        assert_eq!(*target.resyncs.lock().expect("resyncs"), 1);
        assert_eq!(*acks.lock().expect("acks"), vec![12]);
        assert_eq!(client.cursor_file.load().expect("cursor"), 12);
        assert_eq!(*target.health.lock().expect("health"), vec![false]);
    }

    #[tokio::test]
    async fn failing_refresh_does_not_block_later_hints() {
        let failing = Subject::github(SubjectKind::ChangeRequest, "flotilla-org", "flotilla", 1);
        let healthy = Subject::github(SubjectKind::ChangeRequest, "flotilla-org", "flotilla", 2);
        let target =
            Arc::new(MemoryTarget { owned: HashSet::from([healthy.clone()]), failing: HashSet::from([failing]), ..Default::default() });
        let acks = Arc::new(Mutex::new(Vec::new()));
        let relay = Arc::new(MemoryRelay {
            horizon: 0,
            latest: 2,
            deliveries: vec![hint(1, "cr/github.com/flotilla-org/flotilla/1"), hint(2, "cr/github.com/flotilla-org/flotilla/2")],
            acks: Arc::clone(&acks),
        });
        let dir = tempfile::tempdir().expect("tempdir");
        let client = RelayClient {
            connector: relay.clone(),
            target: target.clone(),
            cursor_file: CursorFile { path: dir.path().join("cursor"), install_id: "test".into() },
        };
        let mut session = relay.connect(0).await.expect("connect");
        let mut cursor = 0;
        for _ in 0..2 {
            let frame = session.next().await.expect("hint");
            client.process(frame, &mut cursor, &mut *session).await.expect("continue after forge failure");
        }
        assert_eq!(*target.refreshed.lock().expect("refreshes"), vec![healthy]);
        assert_eq!(*acks.lock().expect("acks"), vec![1, 2]);
    }
}
