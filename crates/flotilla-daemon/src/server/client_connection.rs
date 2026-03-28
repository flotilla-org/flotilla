use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use flotilla_core::{agents::SharedAgentStateStore, daemon::DaemonHandle, in_process::InProcessDaemon};
use flotilla_protocol::{Message, Request};
use flotilla_transport::message::MessageSession;
use tokio::sync::{watch, Notify};
use tracing::{error, info, warn};

use super::{remote_commands::RemoteCommandRouter, request_dispatch::RequestDispatcher};

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

    pub(super) async fn run(mut self, session: Arc<MessageSession>, first_id: u64, first_request: Request) {
        let count = self.client_count.fetch_add(1, Ordering::SeqCst) + 1;
        info!(%count, "client connected");
        self.client_notify.notify_one();

        let event_session = Arc::clone(&session);
        let mut event_rx = self.daemon.subscribe();
        let event_task = tokio::spawn(async move {
            loop {
                match event_rx.recv().await {
                    Ok(event) => {
                        let msg = Message::Event { event: Box::new(event) };
                        if event_session.write(msg).await.is_err() {
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
        if session.write(first_response).await.is_ok() {
            loop {
                tokio::select! {
                    message_result = session.read() => {
                        match message_result {
                            Ok(Some(msg)) => {
                                match msg {
                                    Message::Request { id, request } => {
                                        let response = request_dispatcher.dispatch(id, request).await;
                                        if session.write(response).await.is_err() {
                                            break;
                                        }
                                    }
                                    other => {
                                        warn!(msg = ?other, "unexpected message type from client");
                                        break;
                                    }
                                }
                            }
                            Err(e) => {
                                error!(err = %e, "error reading from client");
                                break;
                            }
                            Ok(None) => break,
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
