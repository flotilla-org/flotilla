//! Registering a crew session that is already registered hands nothing to the blocking pool: the
//! Message loop registers every live holder on every pass, so a repeat must not queue behind the
//! session record's writer. A session not yet registered still writes its record there.

use flotilla_engine_messages_testkit::agent_session_request;
use std::time::Duration;

use flotilla_engine_messages_testkit::{designate_fleet, EngineMessagesHome};
use flotilla_schema::Timestamp;
use flotilla_store::ResourceBackend;
use flotilla_store_memory::InMemoryBackend;

const NAMESPACE: &str = "flotilla";

#[test]
fn a_registered_session_registers_again_without_the_blocking_pool() {
    // One blocking thread, so a test task holding it leaves none for a register.
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().max_blocking_threads(1).build().expect("test runtime");
    runtime.block_on(async {
        let backend = ResourceBackend::new(InMemoryBackend::default());
        designate_fleet(&backend, NAMESPACE).await;
        let home = EngineMessagesHome::open("home-a", NAMESPACE, &backend);
        let primitives = home.primitives();
        let now = || Timestamp::from_datetime(&chrono::Utc::now());
        let first = primitives.register("session-a", agent_session_request(), now()).await.expect("register");

        let (release, held) = std::sync::mpsc::channel::<()>();
        let occupant = tokio::task::spawn_blocking(move || held.recv());

        let again = tokio::time::timeout(Duration::from_secs(10), primitives.register("session-a", agent_session_request(), now()))
            .await
            .expect("a registered session registers again while the blocking pool is busy")
            .expect("register again");
        assert_eq!(again, first, "the same consumer");

        let new = tokio::time::timeout(Duration::from_millis(200), primitives.register("session-b", agent_session_request(), now())).await;
        assert!(new.is_err(), "a new session waits for the blocking pool to write its record");

        release.send(()).expect("release the blocking thread");
        occupant.await.expect("occupant").expect("released");
        let registered = primitives.register("session-b", agent_session_request(), now()).await.expect("register a new session");
        assert_eq!(registered, primitives.consumer("session-b"));
    });
}
