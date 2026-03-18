use std::sync::Arc;

use async_trait::async_trait;
use flotilla_core::in_process::InProcessDaemon;
use flotilla_protocol::{GoodbyeReason, Message, PeerWireMessage};
use tokio::{
    io::{BufReader, BufWriter},
    sync::{mpsc, Mutex},
};

use crate::peer::{PeerManager, PeerSender};

pub(super) struct SocketPeerSender {
    pub(super) tx: tokio::sync::Mutex<Option<mpsc::Sender<Message>>>,
}

#[async_trait]
impl PeerSender for SocketPeerSender {
    async fn send(&self, msg: PeerWireMessage) -> Result<(), String> {
        let tx = self.tx.lock().await.as_ref().cloned().ok_or_else(|| "socket peer outbound channel closed".to_string())?;
        tx.send(Message::Peer(Box::new(msg))).await.map_err(|_| "socket peer outbound channel closed".to_string())
    }

    async fn retire(&self, reason: GoodbyeReason) -> Result<(), String> {
        let tx = self.tx.lock().await.take();
        if let Some(tx) = tx {
            tx.send(Message::Peer(Box::new(PeerWireMessage::Goodbye { reason })))
                .await
                .map_err(|_| "socket peer outbound channel closed".to_string())?;
        }
        Ok(())
    }
}

pub(super) type ConnectionLines = tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>;
pub(super) type ConnectionWriter = Arc<tokio::sync::Mutex<BufWriter<tokio::net::unix::OwnedWriteHalf>>>;

pub(super) async fn write_message(
    writer: &tokio::sync::Mutex<BufWriter<tokio::net::unix::OwnedWriteHalf>>,
    msg: &Message,
) -> Result<(), ()> {
    let mut w = writer.lock().await;
    flotilla_protocol::framing::write_message_line(&mut *w, msg).await.map_err(|_| ())
}

pub(super) async fn sync_peer_query_state(peer_manager: &Arc<Mutex<PeerManager>>, daemon: &Arc<InProcessDaemon>) {
    let (configured, summaries, routes) = {
        let pm = peer_manager.lock().await;
        (pm.configured_peer_names(), pm.get_peer_host_summaries().clone(), pm.topology_routes())
    };

    daemon.set_configured_peer_names(configured).await;
    daemon.set_peer_host_summaries(summaries).await;
    daemon.set_topology_routes(routes).await;
}
