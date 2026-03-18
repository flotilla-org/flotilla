use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use flotilla_core::{agents::SharedAgentStateStore, daemon::DaemonHandle, in_process::InProcessDaemon};
use flotilla_protocol::{Message, Request};
use tokio::sync::{watch, Notify};
use tracing::{error, info, warn};

use super::{
    remote_commands::RemoteCommandRouter,
    request_dispatch::RequestDispatcher,
    shared::{write_message, ConnectionLines, ConnectionWriter},
};

pub(super) struct ClientConnection {
    daemon: Arc<InProcessDaemon>,
    shutdown_rx: watch::Receiver<bool>,
    remote_command_router: RemoteCommandRouter,
    client_count: Arc<AtomicUsize>,
    client_notify: Arc<Notify>,
    agent_state_store: SharedAgentStateStore,
}

impl ClientConnection {
    pub(super) fn new(
        daemon: Arc<InProcessDaemon>,
        shutdown_rx: watch::Receiver<bool>,
        remote_command_router: RemoteCommandRouter,
        client_count: Arc<AtomicUsize>,
        client_notify: Arc<Notify>,
        agent_state_store: SharedAgentStateStore,
    ) -> Self {
        Self { daemon, shutdown_rx, remote_command_router, client_count, client_notify, agent_state_store }
    }

    pub(super) async fn run(mut self, mut lines: ConnectionLines, writer: ConnectionWriter, first_id: u64, first_request: Request) {
        let count = self.client_count.fetch_add(1, Ordering::SeqCst) + 1;
        info!(%count, "client connected");
        self.client_notify.notify_one();

        let event_writer = Arc::clone(&writer);
        let mut event_rx = self.daemon.subscribe();
        let event_task = tokio::spawn(async move {
            loop {
                match event_rx.recv().await {
                    Ok(event) => {
                        let msg = Message::Event { event: Box::new(event) };
                        if write_message(&event_writer, &msg).await.is_err() {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        warn!(skipped = n, "event subscriber lagged");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });

        let request_dispatcher = RequestDispatcher::new(&self.daemon, &self.remote_command_router, &self.agent_state_store);
        let first_response = request_dispatcher.dispatch(first_id, first_request).await;
        if write_message(&writer, &first_response).await.is_ok() {
            loop {
                tokio::select! {
                    line_result = lines.next_line() => {
                        match line_result {
                            Ok(Some(line)) => {
                                let msg: Message = match serde_json::from_str(&line) {
                                    Ok(m) => m,
                                    Err(e) => {
                                        warn!(err = %e, "failed to parse message");
                                        continue;
                                    }
                                };
                                match msg {
                                    Message::Request { id, request } => {
                                        let response = request_dispatcher.dispatch(id, request).await;
                                        if write_message(&writer, &response).await.is_err() {
                                            break;
                                        }
                                    }
                                    other => {
                                        warn!(msg = ?other, "unexpected message type from client");
                                        break;
                                    }
                                }
                            }
                            Ok(None) => break,
                            Err(e) => {
                                error!(err = %e, "error reading from client");
                                break;
                            }
                        }
                    }
                    _ = self.shutdown_rx.changed() => {
                        if *self.shutdown_rx.borrow() {
                            break;
                        }
                    }
                }
            }
        }

        event_task.abort();
        let count = self.client_count.fetch_sub(1, Ordering::SeqCst) - 1;
        info!(%count, "client disconnected");
        self.client_notify.notify_one();
    }
}
