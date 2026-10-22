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
use tokio::sync::{mpsc, Mutex};

struct Session {
    receiver: Mutex<mpsc::Receiver<Result<Vec<u8>, String>>>,
    touched: std::sync::Mutex<tokio::time::Instant>,
}
type Receiver = Arc<Session>;
#[derive(Default)]
pub(super) struct Rounds(Arc<Mutex<HashMap<String, Receiver>>>);
const DEADLINE: Duration = Duration::from_secs(15);
const MAX_ROUNDS: usize = 512;

impl Rounds {
    pub(super) async fn next(
        &self,
        service: Arc<Service>,
        after: Option<u64>,
        session: Option<String>,
        now: Timestamp,
    ) -> Result<CommandValue, String> {
        let (session, receiver) = if let Some(session) = session {
            let receiver = self.0.lock().await.get(&session).cloned().ok_or("change-stream session expired")?;
            (session, receiver)
        } else {
            let session = uuid::Uuid::new_v4().to_string();
            let (tx, rx) = mpsc::channel(2);
            let receiver = Arc::new(Session { receiver: Mutex::new(rx), touched: std::sync::Mutex::new(tokio::time::Instant::now()) });
            let mut rounds = self.0.lock().await;
            if rounds.len() >= MAX_ROUNDS {
                return Err("too many change-stream sessions".into());
            }
            rounds.insert(session.clone(), Arc::clone(&receiver));
            drop(rounds);
            let idle = Arc::downgrade(&receiver);
            let sessions = Arc::clone(&self.0);
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
            let sessions = Arc::clone(&self.0);
            let key = session.clone();
            tokio::spawn(async move {
                let (frames, mut incoming) = mpsc::channel(2);
                let worker = tokio::task::spawn_blocking(move || {
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
            self.0.lock().await.remove(&session);
            return Err("change-stream session expired".into());
        }
        *receiver.touched.lock().expect("stream progress") = now;
        let frame = tokio::time::timeout(DEADLINE, async { receiver.receiver.lock().await.recv().await })
            .await
            .map_err(|_| "routed frame timed out")?
            .ok_or("routed stream closed")??;
        if matches!(Frame::decode(&frame).map_err(|e| e.to_string())?, Frame::Keepalive { .. }) {
            self.0.lock().await.remove(&session);
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
        let CommandValue::ChangeStreamFrame { session, .. } = rounds.next(Arc::clone(&service), None, None, Timestamp(1)).await.unwrap()
        else {
            panic!("frame");
        };
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(10)).await;
        let CommandValue::ChangeStreamFrame { frame, .. } =
            rounds.next(Arc::clone(&service), None, Some(session.clone()), Timestamp(1)).await.unwrap()
        else {
            panic!("frame");
        };
        assert!(matches!(Frame::decode(&frame).unwrap(), Frame::End { .. }));
        tokio::time::advance(Duration::from_secs(10)).await;
        let CommandValue::ChangeStreamFrame { frame, .. } =
            rounds.next(Arc::clone(&service), None, Some(session.clone()), Timestamp(1)).await.unwrap()
        else {
            panic!("frame");
        };
        assert!(matches!(Frame::decode(&frame).unwrap(), Frame::Keepalive { .. }));
        assert!(rounds.next(Arc::clone(&service), None, Some(session), Timestamp(1)).await.is_err());
        // Resume with a fresh wall clock while the blocking source takes its read cut.
        tokio::time::resume();
        let CommandValue::ChangeStreamFrame { session, .. } = rounds.next(Arc::clone(&service), None, None, Timestamp(2)).await.unwrap()
        else {
            panic!("frame");
        };
        tokio::time::pause();
        tokio::time::advance(DEADLINE + Duration::from_secs(1)).await;
        assert!(rounds.next(service, None, Some(session), Timestamp(2)).await.is_err());
    }
}
