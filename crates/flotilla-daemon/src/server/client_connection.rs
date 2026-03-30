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

    pub(super) async fn run(self, session: Arc<MessageSession>, first_id: u64, first_request: Request) {
        // Legacy clients without Hello handshake get a random session ID.
        let session_id = uuid::Uuid::new_v4();
        let (event_task, request_dispatcher, mut shutdown_rx) = self.start_session(&session, session_id);

        let first_response = request_dispatcher.dispatch(first_id, first_request).await;
        if session.write(first_response).await.is_ok() {
            request_loop(&session, &request_dispatcher, &mut shutdown_rx).await;
        }

        self.finish_session(event_task, session_id).await;
    }

    /// Run a stateful client session that began with a Hello handshake.
    ///
    /// Unlike `run`, the first message (Hello) has already been consumed and
    /// replied to, so the loop starts by awaiting the next message. The
    /// `client_session_id` ties cursor ownership to this connection for
    /// cleanup on disconnect.
    pub(super) async fn run_stateful(self, session: Arc<MessageSession>, client_session_id: uuid::Uuid) {
        let (event_task, request_dispatcher, mut shutdown_rx) = self.start_session(&session, client_session_id);
        request_loop(&session, &request_dispatcher, &mut shutdown_rx).await;
        self.finish_session(event_task, client_session_id).await;
    }

    /// Common setup: increment client count, subscribe to events, create dispatcher.
    ///
    /// Returns the event relay task, request dispatcher, and the shutdown receiver
    /// (moved out of `self` so the caller can pass it to `request_loop` without
    /// conflicting borrows).
    fn start_session(
        &self,
        session: &Arc<MessageSession>,
        session_id: uuid::Uuid,
    ) -> (tokio::task::JoinHandle<()>, RequestDispatcher<'_>, watch::Receiver<bool>) {
        let count = self.client_count.fetch_add(1, Ordering::SeqCst) + 1;
        info!(%count, "client connected");
        self.client_notify.notify_one();

        let event_session = Arc::clone(session);
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

        let request_dispatcher = RequestDispatcher::new(&self.daemon, &self.remote_command_router, &self.agent_state_store, session_id);
        (event_task, request_dispatcher, self.shutdown_rx.clone())
    }

    /// Common teardown: abort event relay, clean up session cursors, decrement client count.
    ///
    /// Closes any remote cursors owned by this session via the remote command
    /// router before calling `disconnect_client_session` for local cleanup.
    async fn finish_session(&self, event_task: tokio::task::JoinHandle<()>, session_id: uuid::Uuid) {
        event_task.abort();

        // Close remote cursors first — the remote command router forwards
        // QueryIssueClose to the target daemon that owns each cursor.
        self.remote_command_router.disconnect_session_cursors(session_id).await;

        // Clean up local cursors.
        self.daemon.disconnect_client_session(session_id).await;

        let count = self.client_count.fetch_sub(1, Ordering::SeqCst) - 1;
        info!(%count, "client disconnected");
        self.client_notify.notify_one();
    }
}

/// Read request messages from the session, dispatch them, and write responses.
///
/// Extracted as a free function to avoid borrow conflicts between the
/// `RequestDispatcher` (which borrows fields of `ClientConnection`) and the
/// mutable `shutdown_rx` receiver.
async fn request_loop(session: &Arc<MessageSession>, request_dispatcher: &RequestDispatcher<'_>, shutdown_rx: &mut watch::Receiver<bool>) {
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
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    break;
                }
            }
        }
    }
}
