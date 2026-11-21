//! The engine at a crew test's home (#3108): one decided store whose launches, vessels, terminal
//! sessions and environments a test drives, served on its resource backend as Convoy, Vessel and
//! TerminalSession the way the composition root serves them, the readings a crew service and its
//! leaf subscriptions take over the same store, and the home's Message path.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use flotilla_engine_convoys_testkit::EngineConvoysHome;
use flotilla_engine_messages_testkit::EngineMessagesHome;
use flotilla_provisioning::engine::reads::{LifecycleReadPort, StoreReads};
use flotilla_provisioning_testkit::{AgentSessionsFixture, EngineFixture, TerminalsFixture};
use flotilla_resources::{
    agent_activity::AgentSessionReads,
    convoy::engine::{set_notes, EngineConvoyPath, EngineConvoys, VerdictScope},
    lifecycle_reads::LifecycleReads,
    terminal_records::{AgentLaunch, TerminalLaunchFacts, TerminalLaunchRequest, TerminalLaunchSource},
    Clock, Convoy, ConvoyPhase, ConvoySpec, ConvoyStatus, CrewWorkPhase, ResourceObject, SystemClock,
};
use flotilla_schema_sqlite::stream::Service;
use flotilla_store::ResourceBackend;

/// One home's engine, as a crew test's daemon would hold it.
pub(crate) struct EngineHome {
    pub engine: EngineFixture,
    pub convoys: EngineConvoysHome,
    pub agents: Arc<AgentSessionsFixture>,
    /// The home's Message path, installed on its backend.
    pub messages: EngineMessagesHome,
    /// The Message path's clock.
    message_clock: Arc<MessageClock>,
    namespace: String,
    host: String,
    service: Arc<Service>,
    _dir: tempfile::TempDir,
}

impl EngineHome {
    /// Opens a decided store at `home` and serves its records on `backend` in `namespace`.
    pub fn install(backend: &ResourceBackend, namespace: &str, home: &str) -> Self {
        let dir = tempfile::tempdir().expect("decided store directory");
        let registry = flotilla_provisioning::stored::decided_registry().expect("decided store kinds");
        let service = Arc::new(Service::open(&dir.path().join("decided.sqlite"), home, registry).expect("decided store"));
        let engine = EngineFixture::over_service(&service);
        backend.install_engine_records(Arc::new(flotilla_store::VesselViews::new(backend.clone(), namespace, engine.reads())));
        backend.install_engine_records(Arc::new(flotilla_store::TerminalViews::new(namespace, engine.reads())));
        let convoys = EngineConvoysHome::over(&service, namespace, backend, Arc::new(SystemClock), engine.reads());
        let agents = Arc::new(AgentSessionsFixture::new(chrono::DateTime::<chrono::Utc>::UNIX_EPOCH));
        crate::serve_agents(Arc::clone(&agents));
        let message_clock = Arc::new(MessageClock::default());
        let messages = EngineMessagesHome::open_with_clock(home, namespace, backend, Arc::clone(&message_clock) as Arc<dyn Clock>);
        // As the composition root installs them: a seat is read through its vessel's launch.
        messages.primitives().install_lifecycle_reads(Arc::clone(convoys.reads())).expect("lifecycle reads");
        Self {
            engine,
            convoys,
            agents,
            messages,
            message_clock,
            namespace: namespace.to_owned(),
            host: home.to_owned(),
            service,
            _dir: dir,
        }
    }

    /// The Message path, as the crew and its leaf subscriptions take it.
    pub fn message_path(&self) -> crate::ports::MessagePath {
        self.messages.messages()
    }

    /// Records the message `record` sent to `receiver` as a turn `session` confirmed at `at`, as
    /// its delivery does.
    pub async fn deliver_turn_at(&self, receiver: &str, record: &str, session: &str, at: DateTime<Utc>) {
        *self.message_clock.pinned.lock().expect("message clock") = Some(at);
        self.messages.deliver_turn(receiver, record, session).await;
        *self.message_clock.pinned.lock().expect("message clock") = None;
    }

    /// The home's decided store: another home's log is replicated into it as a peer's arrives.
    pub fn service(&self) -> &Arc<Service> {
        &self.service
    }

    /// The Convoy path, as the crew and its leaf subscriptions take it.
    pub fn path(&self) -> EngineConvoyPath {
        self.convoys.path()
    }

    /// The readings a crew service takes: this home's records, with its AgentSessions.
    pub fn reads(&self) -> Arc<dyn LifecycleReads> {
        self.reads_with(self.agents.clone())
    }

    /// The readings a daemon restarted at `watched_from` serves: this home's records and
    /// AgentSessions, with none of their progress seen. [`crate::agents`] observes on the
    /// restarted daemon's from then on.
    pub fn restarted_reads(&self, watched_from: chrono::DateTime<chrono::Utc>) -> Arc<dyn LifecycleReads> {
        let agents = Arc::new(crate::agents().restarted(watched_from));
        crate::serve_agents(Arc::clone(&agents));
        self.reads_with(agents)
    }

    fn reads_with(&self, agents: Arc<AgentSessionsFixture>) -> Arc<dyn LifecycleReads> {
        let agents: Arc<dyn AgentSessionReads> = agents;
        let engine = &self.engine;
        Arc::new(
            LifecycleReadPort::new(StoreReads::new(self.service.store()), self.host.clone())
                .with_environments(Arc::clone(&engine.environments.environments) as _)
                .with_clones(Arc::clone(&engine.clones.clones) as _)
                .with_checkouts(Arc::clone(&engine.checkouts.checkouts) as _)
                .with_terminals(Arc::clone(&engine.terminals.terminals) as _)
                .with_vessels(Arc::clone(&engine.vessels.vessels) as _)
                .with_agent_sessions(agents),
        )
    }

    /// Admits convoy `name` with `spec` and `status` as the next launch of `name`; returns the launch.
    pub async fn admit(&self, name: &str, spec: ConvoySpec, status: ConvoyStatus) -> String {
        self.convoys.launch(flotilla_engine_convoys_testkit::convoy(&self.namespace, name, spec, status)).await
    }

    /// Admits convoy `name` as [`Self::admit`] does, as a generation of the standing role ensured
    /// from `ensure`; returns the launch.
    pub async fn admit_ensured(&self, name: &str, ensure: &str, spec: ConvoySpec, status: ConvoyStatus) -> String {
        let mut convoy = flotilla_engine_convoys_testkit::convoy(&self.namespace, name, spec, status);
        convoy.metadata.annotations.insert(flotilla_resources::ENSURED_FROM_ANNOTATION.to_owned(), ensure.to_owned());
        self.convoys.launch(convoy).await
    }

    /// Reserves the next launch of `name`, as admission does before it writes anything keyed by
    /// the launch; returns the launch, which [`Self::admit_reserved`] admits.
    pub async fn reserve(&self, name: &str) -> String {
        self.engine().reserve(name).await.expect("reserve a launch")
    }

    /// Admits launch `launch`, which [`Self::reserve`] reserved, with `spec` and `status`.
    pub async fn admit_reserved(&self, launch: &str, spec: ConvoySpec, status: ConvoyStatus) {
        let convoy = flotilla_engine_convoys_testkit::convoy(&self.namespace, launch, spec, status.clone());
        self.engine().admit_reserved(&convoy).await.expect("admit the launch");
        set_notes(&self.path(), launch, &status);
    }

    /// Launch `launch` as its view reads, through `backend`.
    pub async fn view(&self, backend: &ResourceBackend, launch: &str) -> ResourceObject<Convoy> {
        backend.using::<Convoy>(&self.namespace).get(launch).await.expect("convoy view")
    }

    /// Work `work` of launch `launch`'s vessel, requested at this home and realised in
    /// `environment`; returns the vessel's launch.
    pub fn vessel(&self, launch: &str, work: &str, environment: &str) -> String {
        let vessel = self.engine.vessels.request(launch, work, &self.host);
        self.engine.vessels.ready(&vessel, environment);
        vessel
    }

    /// `role`'s agent session `session` in `vessel`, realised in `environment`; returns its launch.
    pub fn session(&self, vessel: &str, role: &str, session: &str, environment: &str) -> String {
        self.launch_session(&TerminalLaunchRequest { session: session.to_owned(), ..TerminalsFixture::agent(vessel, role, environment) })
    }

    /// The session `request` asks for, realised; returns its launch, which is its crew's id. An
    /// agent's launch resolves to its adapter's harness (`claude-code` when it names none).
    pub fn launch_session(&self, request: &TerminalLaunchRequest) -> String {
        let launch = self.engine.terminals.request(request);
        let agent = match &request.source {
            TerminalLaunchSource::Agent { adapter, model, .. } => Some(AgentLaunch {
                adapter: adapter.clone().unwrap_or_else(|| "claude-code".to_owned()),
                model: model.clone(),
                stance: "trusted-implicit".to_owned(),
            }),
            TerminalLaunchSource::Tool { .. } => None,
        };
        let facts = TerminalLaunchFacts { agent, ..Default::default() };
        self.engine.terminals.ready(&launch, &format!("{}-runtime", request.session), &facts);
        launch
    }

    /// Brings `role`'s seat in work `work` of launch `launch`, whose Vessel is `vessel`, to the
    /// phase its view reads as `phase`, by the facts that make it so: Pending starts nothing;
    /// Working starts its agent session `session`; Interrupted loses that session; Done keeps the
    /// seat's promises; Failed is a supervisor's ruling; HandedBack hands its work to a seat
    /// `successor`. Returns the session's launch, once one started.
    pub async fn seat(
        &self,
        backend: &ResourceBackend,
        launch: &str,
        (vessel, work): (&str, &str),
        (role, session): (&str, &str),
        phase: CrewWorkPhase,
    ) -> Option<String> {
        if phase == CrewWorkPhase::Pending {
            return None;
        }
        let convoy = self.view(backend, launch).await;
        let snapshot = convoy.status.as_ref().and_then(|status| status.workflow_snapshot.as_ref()).expect("a launch's workflow");
        let eager =
            snapshot.vessels.iter().any(|requirement| requirement.name == work && requirement.eagerly_started_roles().contains(role));
        if !eager {
            // A latent role works once its task is delivered.
            self.note(backend, launch, |status| {
                status.crew_work.get_mut(work).and_then(|roles| roles.get_mut(role)).expect("the seat").task_assigned = true;
            })
            .await;
        }
        let started = self.session(vessel, role, session, "env");
        let convoy = self.view(backend, launch).await;
        let engine = self.engine();
        match phase {
            CrewWorkPhase::Working => {}
            CrewWorkPhase::Interrupted => self.engine.terminals.lost(&started, "pool lost the session"),
            CrewWorkPhase::Done => self.keep(backend, launch, work, role).await,
            CrewWorkPhase::Failed => engine.fail_seat(&convoy, work, role, "supervisor", "failed").await.expect("fail the seat"),
            CrewWorkPhase::HandedBack => {
                // A seat is handed back once it has handed every open promise on.
                let status = convoy.status.as_ref().expect("status");
                let open: Vec<String> = flotilla_resources::promises::owned(status, work, role)
                    .iter()
                    .filter(|promise| !promise.state.terminal())
                    .map(|promise| promise.id.clone())
                    .collect();
                engine.reassign(&convoy, (work, role), (work, "successor"), &open).await.expect("hand the work back")
            }
            other => panic!("no facts make a {other:?} seat"),
        }
        let read = self.view(backend, launch).await.status.expect("status").crew_work[work][role].phase;
        assert_eq!(read, phase, "the seat's facts read as its phase");
        Some(started)
    }

    /// An operator keeps every open promise of `role` in `work` of launch `launch` by verdict.
    pub async fn keep(&self, backend: &ResourceBackend, launch: &str, work: &str, role: &str) {
        let convoy = self.view(backend, launch).await;
        let scope = VerdictScope { vessel: work.to_owned(), role: Some(role.to_owned()) };
        self.engine().keep_by_verdict(&convoy, &scope, "operator", "done").await.expect("keep by verdict");
    }

    /// Ends launch `launch` as `phase` says: Landed is its delivery, Failed and Abandoned an
    /// operator's ending.
    pub async fn end(&self, backend: &ResourceBackend, launch: &str, phase: ConvoyPhase) {
        let convoy = self.view(backend, launch).await;
        let engine = self.engine();
        match phase {
            ConvoyPhase::Landed => engine.deliver(&convoy, "claim").await,
            ConvoyPhase::Failed => engine.fail(&convoy, "operator", "failed").await,
            ConvoyPhase::Abandoned => engine.abandon(&convoy, "operator", "abandoned").await,
            other => panic!("{other:?} is no ending"),
        }
        .expect("end the launch");
    }

    /// Changes this home's notes on launch `launch` as `change` changes its view's status; returns
    /// the view with them. Only what the home notes is kept: the rest of a view is derived.
    pub async fn note(&self, backend: &ResourceBackend, launch: &str, change: impl FnOnce(&mut ConvoyStatus)) -> ResourceObject<Convoy> {
        let convoy = self.view(backend, launch).await;
        let mut status = convoy.status.unwrap_or_default();
        change(&mut status);
        set_notes(&self.path(), launch, &status);
        self.view(backend, launch).await
    }

    /// The Convoy path's engine.
    pub fn engine(&self) -> Arc<dyn EngineConvoys> {
        self.path()
    }
}

/// The system clock, unless a test pins it to the instant a step happened at.
#[derive(Debug, Default)]
struct MessageClock {
    pinned: std::sync::Mutex<Option<DateTime<Utc>>>,
}

impl Clock for MessageClock {
    fn now(&self) -> DateTime<Utc> {
        self.pinned.lock().expect("message clock").unwrap_or_else(flotilla_resources::clock::now)
    }
}

#[test]
fn dropping_a_home_and_its_backend_closes_its_stores() {
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    let stores = runtime.block_on(async {
        let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
        let home = EngineHome::install(&backend, "flotilla", "host");
        [Arc::downgrade(&home.service), Arc::downgrade(home.messages.service())]
    });
    // The runtime's tasks go with it; a port the backend holds must not hold the backend back.
    drop(runtime);
    assert!(stores[0].upgrade().is_none(), "the home's decided store outlived it and its backend");
    assert!(stores[1].upgrade().is_none(), "the Message path's decided store outlived it and its backend");
}
