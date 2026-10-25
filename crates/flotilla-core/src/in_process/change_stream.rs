//! Routed delivery pulls one bounded frame at a time. The producer retains a consistent
//! WAL read cut, queues two frames, and releases it when its consumer expires.
//! See the [implementation choice](https://github.com/flotilla-org/flotilla/blob/main/docs/implementation-choices/change-stream.md).

use flotilla_protocol::CommandValue;
use flotilla_schema::Timestamp;
use flotilla_schema_sqlite::{
    stream::{Frame, Service},
    StoreError,
};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::sync::{mpsc, Mutex, Semaphore};

struct Session {
    peer: String,
    receiver: Mutex<mpsc::Receiver<Result<Vec<u8>, String>>>,
    touched: std::sync::Mutex<tokio::time::Instant>,
}
type Receiver = Arc<Session>;
pub(super) struct Rounds {
    sessions: Arc<Mutex<HashMap<String, Receiver>>>,
    workers: Arc<Semaphore>,
}
impl Default for Rounds {
    fn default() -> Self {
        Self { sessions: Default::default(), workers: Arc::new(Semaphore::new(MAX_ROUNDS)) }
    }
}
const DEADLINE: Duration = Duration::from_secs(15);
// The whole-home mesh currently sizes for a few hosts. Reserve the blocking pool for
// other SQLite users. Replaced workers retain their permit until they exit; the global
// semaphore, rather than the one-current-session-per-peer rule, bounds worker count.
const MAX_ROUNDS: usize = 16;

impl Rounds {
    pub(super) async fn next(
        &self,
        peer: &str,
        service: Arc<Service>,
        after: Option<u64>,
        session: Option<String>,
        now: Timestamp,
    ) -> Result<CommandValue, String> {
        let (session, receiver) = if let Some(session) = session {
            let receiver = self.sessions.lock().await.get(&session).cloned().ok_or("change-stream session expired")?;
            if receiver.peer != peer {
                return Err("change-stream session belongs to another peer".into());
            }
            (session, receiver)
        } else {
            let session = uuid::Uuid::new_v4().to_string();
            let (tx, rx) = mpsc::channel(2);
            let receiver = Arc::new(Session {
                peer: peer.to_owned(),
                receiver: Mutex::new(rx),
                touched: std::sync::Mutex::new(tokio::time::Instant::now()),
            });
            let mut rounds = self.sessions.lock().await;
            // A reconnect starts a new round immediately. Dropping the abandoned
            // receiver makes its producer fail at its next send, releasing its permit
            // only when the blocking worker exits (which may still await a writer).
            rounds.retain(|_, session| session.peer != peer);
            if rounds.len() >= MAX_ROUNDS {
                return Err("too many change-stream sessions".into());
            }
            let permit = Arc::clone(&self.workers).try_acquire_owned().map_err(|_| "change-stream workers busy")?;
            rounds.insert(session.clone(), Arc::clone(&receiver));
            drop(rounds);
            let idle = Arc::downgrade(&receiver);
            let sessions = Arc::clone(&self.sessions);
            let key = session.clone();
            tokio::spawn(async move {
                loop {
                    let expires = {
                        let Some(session) = idle.upgrade() else {
                            return;
                        };
                        let touched = *session.touched.lock().expect("stream progress");
                        touched + DEADLINE
                    };
                    tokio::time::sleep_until(expires).await;
                    let Some(session) = idle.upgrade() else {
                        return;
                    };
                    let expired = tokio::time::Instant::now().duration_since(*session.touched.lock().expect("stream progress")) >= DEADLINE;
                    if expired {
                        sessions.lock().await.remove(&key);
                        return;
                    }
                }
            });
            let sessions = Arc::clone(&self.sessions);
            let key = session.clone();
            tokio::spawn(async move {
                let (frames, mut incoming) = mpsc::channel(2);
                let worker = tokio::task::spawn_blocking(move || {
                    let _permit = permit;
                    service.serve(after, now, |bytes| {
                        frames.blocking_send(bytes.to_vec()).map_err(|_| StoreError::Stream("routed consumer closed".into()))
                    })
                });
                while let Some(frame) = incoming.recv().await {
                    if !matches!(tokio::time::timeout(DEADLINE, tx.send(Ok(frame))).await, Ok(Ok(()))) {
                        sessions.lock().await.remove(&key);
                        return;
                    }
                }
                if let Err(error) = worker.await.map_err(|e| e.to_string()).and_then(|result| result.map_err(|e| e.to_string())) {
                    let _ = tokio::time::timeout(DEADLINE, tx.send(Err(error))).await;
                }
                // Even a producer that has completed must release an abandoned receiver.
                tx.closed().await;
                sessions.lock().await.remove(&key);
            });
            (session, receiver)
        };
        let now = tokio::time::Instant::now();
        let expired = now.duration_since(*receiver.touched.lock().expect("stream progress")) >= DEADLINE;
        if expired {
            self.sessions.lock().await.remove(&session);
            return Err("change-stream session expired".into());
        }
        *receiver.touched.lock().expect("stream progress") = now;
        let delivery = tokio::time::timeout(DEADLINE, async { receiver.receiver.lock().await.recv().await }).await;
        let frame = match delivery {
            Ok(Some(Ok(frame))) => frame,
            result => {
                self.sessions.lock().await.remove(&session);
                return Err(match result {
                    Ok(Some(Err(error))) => error,
                    Ok(None) => "routed stream closed".into(),
                    Err(_) => "routed frame timed out".into(),
                    _ => unreachable!(),
                });
            }
        };
        if matches!(Frame::decode(&frame).map_err(|e| e.to_string())?, Frame::Keepalive { .. }) {
            self.sessions.lock().await.remove(&session);
        }
        Ok(CommandValue::ChangeStreamFrame { session, frame })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flotilla_schema::KindRegistry;

    // A disconnected routed consumer loses its session after the idle deadline, while
    // progress renews that deadline. Completion removes the session immediately.
    #[tokio::test]
    async fn routed_sessions_expire_on_idle_and_renew_on_progress() {
        let dir = tempfile::tempdir().unwrap();
        let service = Arc::new(Service::open(&dir.path().join("home.db"), "home", KindRegistry::new()).unwrap());
        let rounds = Rounds::default();
        let CommandValue::ChangeStreamFrame { session, .. } =
            rounds.next("peer", Arc::clone(&service), None, None, Timestamp(1)).await.unwrap()
        else {
            panic!("frame");
        };
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(10)).await;
        let CommandValue::ChangeStreamFrame { frame, .. } =
            rounds.next("peer", Arc::clone(&service), None, Some(session.clone()), Timestamp(1)).await.unwrap()
        else {
            panic!("frame");
        };
        assert!(matches!(Frame::decode(&frame).unwrap(), Frame::End { .. }));
        tokio::time::advance(Duration::from_secs(10)).await;
        let CommandValue::ChangeStreamFrame { frame, .. } =
            rounds.next("peer", Arc::clone(&service), None, Some(session.clone()), Timestamp(1)).await.unwrap()
        else {
            panic!("frame");
        };
        assert!(matches!(Frame::decode(&frame).unwrap(), Frame::Keepalive { .. }));
        assert!(rounds.next("peer", Arc::clone(&service), None, Some(session), Timestamp(1)).await.is_err());
        // Resume with a fresh wall clock while the blocking source takes its read cut.
        tokio::time::resume();
        let CommandValue::ChangeStreamFrame { session, .. } =
            rounds.next("peer", Arc::clone(&service), None, None, Timestamp(2)).await.unwrap()
        else {
            panic!("frame");
        };
        tokio::time::pause();
        tokio::time::advance(DEADLINE + Duration::from_secs(1)).await;
        assert!(rounds.next("peer", service, None, Some(session), Timestamp(2)).await.is_err());
    }
    // A fresh round replaces the peer's abandoned cursor immediately. Other peers
    // progress independently, and an opaque session token grants no cross-peer access.
    #[tokio::test]
    async fn peer_limits_isolate_sessions_and_bound_global_admission() {
        let dir = tempfile::tempdir().unwrap();
        let service = Arc::new(Service::open(&dir.path().join("home.db"), "home", KindRegistry::new()).unwrap());
        let rounds = Rounds::default();
        let CommandValue::ChangeStreamFrame { session, .. } =
            rounds.next("first", Arc::clone(&service), None, None, Timestamp(1)).await.unwrap()
        else {
            panic!("frame");
        };
        let CommandValue::ChangeStreamFrame { session: active, .. } =
            rounds.next("first", Arc::clone(&service), None, None, Timestamp(1)).await.unwrap()
        else {
            panic!("replacement frame");
        };
        assert_eq!(
            rounds.next("first", Arc::clone(&service), None, Some(session), Timestamp(1)).await.unwrap_err(),
            "change-stream session expired"
        );
        assert!(rounds.next("other", Arc::clone(&service), None, Some(active), Timestamp(1)).await.unwrap_err().contains("another peer"));
        for index in 1..MAX_ROUNDS {
            assert!(rounds.next(&format!("peer-{index}"), Arc::clone(&service), None, None, Timestamp(1)).await.is_ok());
        }
        assert!(rounds.next("overflow", service, None, None, Timestamp(1)).await.unwrap_err().contains("too many"));
    }

    // A source failure releases its cursor as soon as it is delivered, so the same peer
    // can retry without waiting for the idle reaper or receiving a stale session error.
    #[tokio::test]
    #[cfg(unix)] // The injected failure unlinks a database while its writer is open.
    async fn delivered_source_error_releases_the_peer_slot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("home.db");
        let service = Arc::new(Service::open(&path, "home", KindRegistry::new()).unwrap());
        std::fs::remove_file(path).unwrap();
        let rounds = Rounds::default();
        for _ in 0..2 {
            let error = rounds.next("peer", Arc::clone(&service), None, None, Timestamp(1)).await.unwrap_err();
            assert!(error.contains("unable to open database file"), "{error}");
        }
    }
}
