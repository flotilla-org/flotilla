use super::*;
use flotilla_provisioning::{
    engine::{launch, reads::StoreReads, LifecycleHome, Lifecycles, Step},
    stored::{
        self,
        terminal_session::{Launch, TerminalSessionKind, TerminalSessionRequest},
        Holder,
    },
};
use flotilla_resources::vessel_seats::VesselSeats;
use flotilla_schema::{Ref as StoredRef, Timestamp};
use flotilla_store_memory::InMemoryBackend;

const HOME: &str = "local";

async fn labelled_session(backend: &ResourceBackend, name: &str, role: &str, vessel_ref: &str) {
    backend
        .clone()
        .using::<TerminalSession>(NAMESPACE)
        .create(
            &empty_meta_with_labels(name, BTreeMap::from([(flotilla_resources::VESSEL_REF_LABEL.to_string(), vessel_ref.to_string())])),
            &TerminalSessionSpec {
                env_ref: "env-a".into(),
                role: role.into(),
                source: TerminalSessionSource::Tool { command: "bash".into() },
                cwd: "/workspace".into(),
                env: Default::default(),
                pool: "test".into(),
            },
        )
        .await
        .expect("session");
}

// Behaviour (#3748 review, #3629 layer 8): during the switch a vessel can hold launches the engine
// decided beside sessions only the old store knows. Its seats are the labelled sessions, with each
// role the engine holds a launch for served by that launch's record; an engine launch whose record
// is gone leaves the labels' answer.
#[tokio::test]
async fn a_vessels_seats_overlay_the_engines_launches_on_its_labelled_sessions() {
    let backend = ResourceBackend::new(InMemoryBackend::default());
    labelled_session(&backend, "terminal-vessel-a-reviewer", "reviewer", "vessel-a").await;
    labelled_session(&backend, "terminal-vessel-a-coder-old", "coder", "vessel-a").await;
    // The engine's coder launch, under an unlabelled record of its own.
    backend
        .clone()
        .using::<TerminalSession>(NAMESPACE)
        .create(
            &empty_meta("terminal-vessel-a-coder"),
            &TerminalSessionSpec {
                env_ref: "env-a".into(),
                role: "coder".into(),
                source: TerminalSessionSource::Tool { command: "bash".into() },
                cwd: "/workspace".into(),
                env: Default::default(),
                pool: "test".into(),
            },
        )
        .await
        .expect("session");

    let mut registry = flotilla_schema::KindRegistry::new();
    stored::register(&mut registry).expect("registry");
    let store = Arc::new(StdMutex::new(
        flotilla_schema_sqlite::Store::open(rusqlite::Connection::open_in_memory().expect("memory"), HOME, registry).expect("store"),
    ));
    let sessions: LifecycleHome<TerminalSessionKind, _> = LifecycleHome::new(
        HOME,
        flotilla_state_machines::Home::with_log(HOME, flotilla_engine_store::StoreLog::new(Arc::clone(&store))).expect("home"),
    );
    let request = |holder: &str| TerminalSessionRequest {
        environment: StoredRef::new("env-a", HOME),
        holder: Some(Holder::vessel(holder)),
        role: "coder".into(),
        pool: "test".into(),
        cwd: "/workspace".into(),
        env: Default::default(),
        source: Launch::Tool { command: "bash".into() },
    };
    sessions.request(&launch("terminal-vessel-a-coder", 1), request("vessel-a"), Timestamp(1)).expect("launch");
    sessions.step(&launch("terminal-vessel-a-coder", 1), &Step::Realised { identity: "s1".into() }, Timestamp(2)).expect("ran");
    // A launch whose record is gone.
    sessions.request(&launch("terminal-vessel-a-gone", 1), request("vessel-a"), Timestamp(3)).expect("launch");

    let seats = EngineVesselSeats { reads: StoreReads::new(store), backend };
    let served: BTreeMap<String, String> = seats
        .seats(NAMESPACE, "vessel-a")
        .await
        .expect("seats")
        .into_iter()
        .map(|(role, session)| (role, session.object.metadata.name))
        .collect();
    assert_eq!(
        served,
        BTreeMap::from([
            ("coder".to_string(), "terminal-vessel-a-coder".to_string()),
            ("reviewer".to_string(), "terminal-vessel-a-reviewer".to_string()),
        ])
    );
}
