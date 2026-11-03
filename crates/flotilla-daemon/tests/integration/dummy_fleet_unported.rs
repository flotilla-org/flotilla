//! Dummy-fleet scenarios not yet written on `TestDaemon` (#4321). They keep
//! their harness from `main` until it is decided what each needs:
//!
//! - production resync: a controller resync cadence the harness does not offer;
//! - two-daemon replication: peering two daemons, and reading engine rows;
//! - input-needed and declaration refusals: an awareness query subscription.

use std::{sync::Arc, time::Duration};

use flotilla_config::{ConfigStore, DaemonConfig};
use flotilla_core::in_process::InProcessDaemon;
use flotilla_daemon::{
    dummy_fleet::{attach_dummy_fleet, install_dummy_fleet},
    runtime::{DaemonRuntime, RuntimeOptions},
};
use flotilla_daemon_api::daemon::DaemonHandle;
use flotilla_discovery_testkit::fake_discovery_with_host_directories;
use flotilla_dummy_fleet::{
    catalog::{self, CrewMix, DUMMY_FULFILMENT_KIND},
    DummyFleet, DummyFleetSnapshot, DUMMY_AGENT_ADAPTER,
};
use flotilla_protocol::{commands::CommandValue, Command, CommandAction, DaemonEvent, HostName, PrincipalRef};
use flotilla_resources::{Convoy, ConvoyPhase, CrewWorkPhase, Forge, Host, InputMeta, Repository, ResourceObject, WorkflowTemplate};
use flotilla_store::ResourceBackend;
use flotilla_store_memory::InMemoryBackend;

const NAMESPACE: &str = "flotilla";
// Two tasks cover both a middle turn and the last one.
const LONG_RUNNER_TURNS: u32 = 2;
const FULFILMENT_PROBE_INTERVAL: Duration = Duration::from_millis(200);

struct DummyDaemon {
    daemon: Arc<InProcessDaemon>,
    fleet: Arc<DummyFleet>,
    _runtime: DaemonRuntime,
    _dir: tempfile::TempDir,
}

#[bon::bon]
impl DummyDaemon {
    async fn start_with_resync(agent: &str, environment: &str, resync: Duration) -> Self {
        Self::start_switched(agent, environment, resync, false).await
    }

    async fn start_switched(agent: &str, environment: &str, resync: Duration, primitives: bool) -> Self {
        Self::start_with_relay().agent(agent).environment(environment).resync(resync).primitives(primitives).host("dummy-load").call().await
    }

    #[builder]
    async fn start_with_relay(
        #[builder(default)] agent: &str,
        #[builder(default)] environment: &str,
        #[builder(default = Duration::from_millis(200))] resync: Duration,
        #[builder(default = Duration::from_secs(5))] heartbeat: Duration,
        #[builder(default = true)] primitives: bool,
        host: &str,
        #[builder(default = true)] designated: bool,
        #[builder(default)] supervision: flotilla_daemon::supervisor::ControllerSupervision,
        #[builder(default = Arc::new(flotilla_resources::SystemClock))] clock: Arc<dyn flotilla_resources::Clock>,
    ) -> Self {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let config_dir = dir.path().join("config");
        std::fs::create_dir_all(&config_dir).expect("config dir");
        let switch = if primitives { "[primitives]\n" } else { "" };
        let daemon_toml = format!(
            "machine_id = \"{host}\"\n\n{switch}[dummy_fleet]\n[dummy_fleet.agent]\nlong_runner_turns = {LONG_RUNNER_TURNS}\n{agent}\n[dummy_fleet.environment]\n{environment}"
        );
        std::fs::write(config_dir.join("daemon.toml"), daemon_toml).expect("daemon.toml");
        let config = Arc::new(ConfigStore::with_base(config_dir));
        let daemon_config: DaemonConfig = config.load_daemon_config().expect("daemon config");
        let mut discovery = fake_discovery_with_host_directories(dir.path());
        let fleet = install_dummy_fleet(&daemon_config, &mut discovery).expect("dummy fleet configured");
        let backend = ResourceBackend::new(InMemoryBackend::default());
        let daemon = InProcessDaemon::new_with_resource_backend_and_clock(
            vec![],
            Arc::clone(&config),
            discovery,
            HostName::new(host),
            backend,
            clock,
            std::sync::Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
        )
        .await;
        attach_dummy_fleet(&fleet, &daemon).expect("attach fleet");
        let options = RuntimeOptions {
            namespace: NAMESPACE.into(),
            heartbeat_interval: heartbeat,
            controller_resync_interval: resync,
            // The dummy kind is seeded after startup; observe it on the next pass.
            fulfilment_probe_interval: FULFILMENT_PROBE_INTERVAL,
            controller_supervision: supervision,
            ..RuntimeOptions::default()
        };
        let runtime = DaemonRuntime::start_with_options(Arc::clone(&daemon), config, None, options).await.expect("runtime");
        let started = Self { daemon, fleet, _runtime: runtime, _dir: dir };
        started.seed(designated).await;
        started
    }

    async fn designate(&self) {
        self.backend()
            .definitions::<flotilla_resources::FleetDesignation>(NAMESPACE)
            .create(
                &InputMeta::builder().name(flotilla_resources::FLEET_DESIGNATION_NAME.to_string()).build(),
                &flotilla_resources::FleetDesignationSpec { fleet: Some("lab".into()), project: "fleet".into(), image_cache: None },
            )
            .await
            .expect("fleet designation");
    }

    fn backend(&self) -> ResourceBackend {
        self.daemon.resource_backend()
    }

    async fn seed(&self, designated: bool) {
        let backend = self.backend();
        if self.daemon.message_primitives().is_some() {
            // Fleet identity and charter Project are declared independently.
            backend
                .clone()
                .using::<flotilla_resources::Project>(NAMESPACE)
                .create(
                    &InputMeta::builder().name("fleet".to_string()).build(),
                    &flotilla_resources::ProjectSpec::builder()
                        .display_name("Fleet".to_string())
                        .default_workflow_ref(CrewMix::Coder.workflow_name().to_string())
                        .repositories(Vec::new())
                        .build(),
                )
                .await
                .expect("fleet project");
            if designated {
                self.designate().await;
            }
        }
        let host = backend.clone().using::<Host>(NAMESPACE).list().await.expect("hosts").items.into_iter().next().expect("local host");
        let forge = catalog::forge(&self.fleet.config().forge_service_url);
        backend
            .clone()
            .using::<Forge>(NAMESPACE)
            .create(&InputMeta::builder().name(forge.forge_id.clone()).build(), &forge)
            .await
            .expect("dummy forge");
        // A dispatched repository is tracked first; admission reads its resolved default branch.
        let repository = catalog::repository(&forge).expect("dummy repository");
        let repositories = backend.clone().using::<Repository>(NAMESPACE);
        repositories.create(&InputMeta::builder().name(repository.key().to_string()).build(), &repository).await.expect("repository");
        backend
            .clone()
            .using::<flotilla_resources::FulfilmentKind>(NAMESPACE)
            .create(&InputMeta::builder().name(DUMMY_FULFILMENT_KIND.to_string()).build(), &catalog::fulfilment_kind(&host.metadata.name))
            .await
            .expect("dummy fulfilment kind");
        for mix in CrewMix::ALL {
            backend
                .clone()
                .using::<WorkflowTemplate>(NAMESPACE)
                .create(&InputMeta::builder().name(mix.workflow_name().to_string()).build(), &mix.workflow(LONG_RUNNER_TURNS))
                .await
                .expect("dummy workflow");
        }
        tokio::time::timeout(Duration::from_secs(10), async {
            while repositories
                .get(&repository.key().to_string())
                .await
                .ok()
                .and_then(|repository| repository.status?.default_branch)
                .is_none()
            {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("the dummy forge resolves the default branch");
        // Admission places a literal image only once its host has probed the image's harnesses.
        let hosts = backend.clone().using::<Host>(NAMESPACE);
        tokio::time::timeout(Duration::from_secs(20), async {
            while !hosts.get(&host.metadata.name).await.ok().and_then(|host| host.status).is_some_and(|status| {
                status.fulfilment_facts.get(DUMMY_FULFILMENT_KIND).is_some_and(|facts| facts.harnesses.contains_key(DUMMY_AGENT_ADAPTER))
            }) {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("the dummy kind's harness is observed");
    }

    async fn execute(&self, action: CommandAction) -> CommandValue {
        self.execute_as(action, None).await
    }

    async fn execute_as(&self, action: CommandAction, principal: Option<PrincipalRef>) -> CommandValue {
        let mut events = self.daemon.subscribe();
        let id = self.daemon.execute_for_principal(Command::builder().action(action).build(), principal).await.expect("execute");
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Ok(DaemonEvent::CommandFinished { command_id, result, .. }) = events.recv().await {
                    if command_id == id {
                        return result;
                    }
                }
            }
        })
        .await
        .expect("command result")
    }

    async fn dispatch(&self, name: &str, mix: CrewMix) {
        let forge = catalog::forge(&self.fleet.config().forge_service_url);
        let created = self
            .execute(CommandAction::ConvoyCreate {
                name: name.into(),
                workflow_ref: mix.workflow_name().into(),
                inputs: Vec::new(),
                repository_url: Some(catalog::repository_url(&forge)),
                r#ref: Some("main".into()),
                project_ref: None,
                placement_policy: Some(DUMMY_FULFILMENT_KIND.into()),
                adopted_checkout: None,
            })
            .await;
        assert_eq!(created, CommandValue::ConvoyCreated { name: name.into() });
    }

    async fn convoy(&self, name: &str) -> Option<ResourceObject<Convoy>> {
        let convoys = self.backend().using::<Convoy>(NAMESPACE).list().await.expect("convoys");
        convoys.items.into_iter().find(|convoy| convoy.metadata.labels.values().any(|value| value == name))
    }

    /// Wait until `done` holds for the convoy, reporting where it stopped.
    async fn wait_for(&self, name: &str, what: &str, done: impl Fn(&ResourceObject<Convoy>) -> bool) -> ResourceObject<Convoy> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            let convoy = self.convoy(name).await;
            match convoy {
                Some(convoy) if done(&convoy) => return convoy,
                convoy if tokio::time::Instant::now() > deadline => {
                    let status = convoy.and_then(|convoy| convoy.status);
                    panic!(
                        "convoy {name} never reached {what}: phase {:?}, message {:?}, crew {:?}, stall {:?}, supervision {:?}, fleet {:?}",
                        status.as_ref().map(|status| status.phase),
                        status.as_ref().and_then(|status| status.message.clone()),
                        status.as_ref().map(|status| &status.crew_work),
                        status.as_ref().and_then(|status| status.stalled.as_ref()),
                        status.as_ref().and_then(|status| status.workflow_snapshot.as_ref()).and_then(|w| w.supervision.as_ref()),
                        self.fleet.snapshot()
                    );
                }
                _ => tokio::time::sleep(Duration::from_millis(100)).await,
            }
        }
    }

    async fn wait_for_phase(&self, name: &str, phase: ConvoyPhase) -> ResourceObject<Convoy> {
        self.wait_for(name, &format!("{phase:?}"), |convoy| convoy.status.as_ref().is_some_and(|status| status.phase == phase)).await
    }

    /// A crew stall a supervisor can act on: the crew's work and the convoy's
    /// stall condition, both published by the crew and maintained by the leaf engine.
    async fn wait_for_stall(&self, name: &str) -> ResourceObject<Convoy> {
        self.wait_for(name, "a supervisable stall", |convoy| {
            convoy.status.as_ref().is_some_and(|status| {
                status.stalled.is_some()
                    && status
                        .crew_work
                        .get("implement")
                        .and_then(|roles| roles.get("coder"))
                        .is_some_and(|work| work.phase == CrewWorkPhase::Stalled)
            })
        })
        .await
    }

    async fn deliver(&self, name: &str, mix: CrewMix) -> DummyFleetSnapshot {
        self.dispatch(name, mix).await;
        let landed = self.wait_for_phase(name, ConvoyPhase::Landed).await;
        if self.daemon.message_primitives().is_some() {
            self.assert_engine_delivered(&landed.metadata.name);
        }
        self.fleet.snapshot()
    }
}

// Scenarios use the default agent behaviour; failure and stall rates are added per test.
const SCENARIO_AGENT: &str = "";
// A missed event must not be hidden by the scenario harness's 200ms resync.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_coder_lands_at_the_production_resync_interval() {
    let fleet = DummyDaemon::start_with_resync(SCENARIO_AGENT, "", RuntimeOptions::default().controller_resync_interval).await;
    fleet.dispatch("production-resync", CrewMix::Coder).await;
    fleet.wait_for_phase("production-resync", ConvoyPhase::Landed).await;
    assert_eq!(fleet.fleet.snapshot().command_errors, 0, "{:?}", fleet.fleet.command_error_samples());
}

impl DummyDaemon {
    fn assert_engine_delivered(&self, name: &str) {
        use flotilla_state_machines::machines::convoy::ConvoyState;
        let engine = self.daemon.message_primitives().expect("engine").convoys().get(name).expect("engine convoy");
        assert!(matches!(engine.state(), ConvoyState::Delivered { .. }), "engine delivery: {engine:?}");
    }
}

impl DummyDaemon {
    // Transport fixtures for kinds the dummy workflow does not request. These
    // enter through real engine homes; they prove replication, not provisioning
    // or standing-convoy admission.
    fn seed_transport_kinds(&self) {
        use flotilla_engine_store::StoreLog;
        use flotilla_provisioning::stored::{
            clone::{Clone, CloneRequest},
            image_build::{Architecture, Delivery, ImageBuild, ImageBuildRequest, Reservation},
        };
        use flotilla_schema::{Address, Frozen, Timestamp};
        use flotilla_state_machines::{
            machines::{
                ensure::{Ensure, EnsureSpec, MemoryEnsureWorld},
                lifecycle::MemoryLifecycleWorld,
            },
            Home,
        };
        let service = self.daemon.decided_store().expect("store");
        let home = self.daemon.node_id().as_str();
        let at = Timestamp::from_datetime(&chrono::Utc::now());
        let mut builds = Home::<ImageBuild, _>::with_log(home, StoreLog::new(service.store())).expect("image home");
        builds
            .create(
                "transport-build",
                ImageBuildRequest {
                    composition: "sha256:transport".into(),
                    architecture: Architecture::Amd64,
                    layers: Frozen::new(Vec::new()).expect("layers"),
                    delivery: Delivery::Local,
                    reservation: Reservation { cpu: 1, disk_bytes: 1 },
                },
                &MemoryLifecycleWorld::default(),
                at,
            )
            .expect("image record");
        let mut clones = Home::<Clone, _>::with_log(home, StoreLog::new(service.store())).expect("clone home");
        clones
            .create(
                "transport-clone",
                CloneRequest {
                    repository: "transport-repository".into(),
                    url: "https://forge.dummy.invalid/transport".into(),
                    environment: flotilla_schema::Ref::new("transport-environment", home),
                    path: "/transport".into(),
                },
                &MemoryLifecycleWorld::default(),
                at,
            )
            .expect("clone record");
        let mut awaiting =
            Home::<flotilla_state_machines::machines::awaiting_reply::AwaitingReply, _>::with_log(home, StoreLog::new(service.store()))
                .expect("awaiting home");
        awaiting
            .create(
                "transport-awaiting",
                flotilla_state_machines::machines::awaiting_reply::AwaitingReplySpec {
                    message: flotilla_schema::Ref::new("transport-message", home),
                    sender: Address::project("fleet").expect("sender"),
                    recipient: Address::project("fleet").expect("recipient"),
                    subject: None,
                    kind: "transport".into(),
                    awaited: flotilla_state_machines::machines::awaiting_reply::Awaited::Reply,
                    deadline: None,
                },
                &flotilla_state_machines::machines::awaiting_reply::MemoryAwaitingReplyWorld::default(),
                at,
            )
            .expect("awaiting record");
        let mut ensures = Home::<Ensure, _>::with_log(home, StoreLog::new(service.store())).expect("ensure home");
        ensures
            .create(
                "transport-ensure",
                EnsureSpec {
                    inbox: Address::project("fleet").expect("project"),
                    policy: flotilla_state_machines::AttemptPolicy::immediate(3),
                },
                &MemoryEnsureWorld::default(),
                at,
            )
            .expect("ensure record");
        // #3824 layer 1: the account kinds, a binding machine and its provision, a plain kind.
        let binding = self.transport_binding(at);
        let mut bindings =
            Home::<flotilla_provisioning::stored::account::AccountBinding, _>::with_log(home, StoreLog::new(service.store()))
                .expect("binding home");
        bindings.create("transport-binding", binding, &(), at).expect("binding record");
        flotilla_engine_store::plain::PlainLog::new(service.store())
            .create(
                "transport-provision",
                &flotilla_provisioning::stored::account::ProvisionSpec {
                    binding: flotilla_schema::Ref::new("transport-binding", home),
                    // The daemon's own host-direct place, which its provision controller keeps.
                    place: flotilla_resources::host_direct_environment_name(
                        &self.daemon.local_host_id().expect("local host id").to_string(),
                    ),
                    environment_kind: "host-direct".into(),
                    plan: flotilla_resources::account_access::ProvisionPlan {
                        implementation: "claude-direct".into(),
                        effective_access: "account".into(),
                        mechanism: "oauth token".into(),
                        update_mode: flotilla_resources::account_access::UpdateMode::Push,
                    },
                    home: "/transport".into(),
                    delivered: None,
                },
                &flotilla_engine_store::plain::PendingChange::new(flotilla_schema::NewChange::new("transport", at)),
            )
            .expect("provision record");
        // #3824 layer 2c: a ManifestRoot, declared with no records yet.
        let mut roots =
            Home::<flotilla_provisioning::stored::manifest_root::ManifestRoot, _>::with_log(home, StoreLog::new(service.store()))
                .expect("root home");
        let request = flotilla_provisioning::stored::manifest_root::RootRequest {
            host: "transport".into(),
            path: "/transport/manifests".into(),
            source: "transport".into(),
            binding: None,
        };
        roots.create("manifest-transport", request, &(), at).expect("root record");
        // #3824 layer 2b-ii: one record of each definition kind.
        Self::seed_definitions(&flotilla_engine_store::plain::PlainLog::new(service.store()), at);
    }

    fn seed_definitions(log: &flotilla_engine_store::plain::PlainLog, at: flotilla_schema::Timestamp) {
        use flotilla_provisioning::stored::definition::{
            CredentialGrantDefinition, CredentialSpecDefinition, CrewDefaultsDefinition, FleetDesignationDefinition, ForgeDefinition,
            ImageLayerDefinition, WorkflowTemplateDefinition,
        };
        fn decode<T: serde::de::DeserializeOwned>(value: serde_json::Value) -> T {
            serde_json::from_value(value).expect("fixture spec")
        }
        let builtin = |kind: &str| flotilla_resources::builtins::builtin_definitions(kind).expect("built-ins")[0].spec.clone();
        // One seeding pass, one change.
        let change = flotilla_engine_store::plain::PendingChange::new(flotilla_schema::NewChange::new("transport", at));
        let forge: flotilla_resources::ForgeSpec = decode(serde_json::json!({
            "forge_id": "transport", "kind": "forgejo", "hosts": ["transport.example"],
            "https_url": "https://transport.example", "git_ssh_host": "transport.example"
        }));
        log.create("transport", &ForgeDefinition::new(&forge), &change).expect("forge record");
        let fleet: flotilla_resources::FleetDesignationSpec = decode(serde_json::json!({"project": "transport"}));
        log.create("transport-fleet", &FleetDesignationDefinition::new(&fleet), &change).expect("fleet record");
        let credential: flotilla_resources::CredentialSpecSpec = decode(serde_json::json!({
            "consumer": {"adapter": "claude-oauth", "account_email": "transport@example.invalid"},
            "lifecycle": "static", "source": {"kind": "file", "path": "/transport/token"}
        }));
        log.create("transport-credential", &CredentialSpecDefinition::new(&credential), &change).expect("spec record");
        let grant: flotilla_resources::CredentialGrantSpec =
            decode(serde_json::json!({"credentials": ["transport-credential"], "selector": {"projects": ["transport"]}}));
        log.create("transport-grant", &CredentialGrantDefinition::new(&grant), &change).expect("grant record");
        log.create("transport-defaults", &CrewDefaultsDefinition::new(&Default::default()), &change).expect("defaults record");
        let layer: flotilla_resources::ImageLayerResourceSpec = decode(builtin("ImageLayer"));
        let flotilla_resources::ImageLayerResourceSpec::Current(layer) = layer else { panic!("a built-in layer is current") };
        log.create("transport-layer", &ImageLayerDefinition::new(&layer).expect("layer"), &change).expect("layer record");
        let template: flotilla_resources::WorkflowTemplateSpec = decode(builtin("WorkflowTemplate"));
        log.create("transport-template", &WorkflowTemplateDefinition::new(None, &template).expect("template"), &change)
            .expect("template record");
        // #3824 layer 3a: a Repository, named by its key.
        let repository = flotilla_resources::RepositorySpec::remote("https://transport.example/transport/app").expect("repository");
        let record = flotilla_provisioning::stored::repository::RepositoryRecord::new(repository.clone()).expect("record");
        log.create(&repository.key().to_string(), &record, &change).expect("repository record");
        // #3824 layer 3b: a Project, and its project operations' record.
        let project = flotilla_resources::ProjectSpec::builder().display_name("transport".into()).build();
        let record = flotilla_provisioning::stored::project::ProjectDefinition::new(&project).expect("project");
        log.create("transport", &record, &change).expect("project record");
        let ops = flotilla_provisioning::stored::project::ProjectOpsRecord { refused: None, entries: None };
        log.create("transport", &ops, &change).expect("project ops record");
        let artifact = flotilla_resources::ArtifactSpec::builder()
            .convoy("transport-convoy".into())
            .producer("coder".into())
            .kind("brief".into())
            .subject("transport-convoy".into())
            .digest("ef37".into())
            .size(1)
            .media_type("text/markdown".into())
            .expires_at(chrono::DateTime::UNIX_EPOCH)
            .build();
        let record = flotilla_provisioning::stored::artifact::ArtifactRecord::from_spec(&artifact).expect("artifact");
        log.create("transport-artifact", &record, &change).expect("artifact record");
        // One record of each forge observation.
        use flotilla_provisioning::stored::forge_observation::{ChangeRequestRecord, ForgeReadRecord, IssueRecord, IssueState, ReadValue};
        let change_request = ChangeRequestRecord {
            service: "github.com".into(),
            scope: "transport/app".into(),
            number: 1,
            title: Some("transport".into()),
            author: None,
            state: Some(flotilla_provisioning::stored::forge_observation::ChangeRequestState::Open),
            review_decision: None,
            review_requested_from_owner: None,
            head_sha: Some("abc".into()),
            checks: None,
            actionable_at_head: None,
            mergeable: None,
            failing_since: None,
        };
        log.create("transport-cr", &change_request, &change).expect("change request record");
        let issue = IssueRecord {
            service: "github.com".into(),
            scope: "transport/app".into(),
            number: 2,
            title: Some("transport".into()),
            assignees: Some(Vec::new()),
            state: Some(IssueState::Open),
            labels: None,
            updated_at: Some(at),
            failing_since: None,
        };
        log.create("transport-issue", &issue, &change).expect("issue record");
        let read = ForgeReadRecord {
            service: "https://github.com".into(),
            scope: "transport/app".into(),
            request: flotilla_resources::ForgeReadRequest::Board,
            value: Some(ReadValue { value: serde_json::Value::Null }),
            error: None,
            retry_at: None,
        };
        log.create("transport-forge-read", &read, &change).expect("forge read record");
    }

    fn transport_binding(&self, at: flotilla_schema::Timestamp) -> flotilla_provisioning::stored::account::BindingSpec {
        use flotilla_resources::account_access::{requester_address, AccessName, CredentialName, Selection};
        flotilla_provisioning::stored::account::BindingSpec {
            requester: requester_address(Some("fleet"), "transport-convoy", "coder").expect("requester"),
            item: "claude-code".into(),
            requirement: AccessName("claude-code:account".into()),
            credential: CredentialName("transport-credential".into()),
            account: None,
            grants: Default::default(),
            candidates: Default::default(),
            exclusions: Default::default(),
            selection: Selection::Draw { index: 0 },
            epochs: flotilla_schema::Records::new(),
            opened_at: at,
        }
    }

    // A replica may consume a later edge before the driver checks its earlier
    // snapshot. Exercise that ordering without depending on teardown timing.
    fn advance_transport_ensure(&self) {
        use flotilla_engine_store::{open_home, StoreLog};
        use flotilla_schema::Timestamp;
        use flotilla_state_machines::{
            machines::ensure::{Ensure, MemoryEnsureWorld, Record},
            Ref,
        };
        let service = self.daemon.decided_store().expect("store");
        let mut home = open_home::<Ensure>(StoreLog::new(service.store())).expect("ensure home");
        home.apply(
            "transport-ensure",
            Record { fulfiller: Ref::new("later-transport-generation") },
            &MemoryEnsureWorld::default(),
            Timestamp::from_datetime(&chrono::Utc::now()),
        )
        .expect("advance transport ensure");
    }

    fn engine_snapshots(&self) -> Vec<flotilla_schema_sqlite::Snapshot> {
        use flotilla_provisioning::stored::{
            checkout::Checkout, clone::Clone, environment::Environment, image_build::ImageBuild, terminal_session::TerminalSession,
        };
        use flotilla_state_machines::{
            machines::{
                agent_session::AgentSession, awaiting_reply::AwaitingReply, convoy::Convoy, ensure::Ensure, message::Message,
                person::Person, vessel::Vessel,
            },
            Resource,
        };
        self.daemon
            .decided_store()
            .expect("store")
            .with_store(|store| {
                let home = store.home().to_string();
                let registered: std::collections::BTreeSet<_> = store.kinds().iter().map(|kind| kind.code).collect();
                let mut covered = std::collections::BTreeSet::new();
                let mut snapshots = Vec::new();
                let mut read = store.read()?;
                macro_rules! collect {
                    ($kind:ty) => {
                        collect!(stored Resource<$kind>)
                    };
                    (stored $stored:ty) => {{
                        let code = <$stored as flotilla_schema::Kind>::CODE;
                        covered.insert(code);
                        let mut after = None;
                        let before = snapshots.len();
                        loop {
                            let page = read.scan::<$stored>(&home, after, 64)?;
                            for id in page.ids {
                                snapshots.push(read.load_snapshot(id)?.expect("snapshot"));
                            }
                            after = page.next;
                            if after.is_none() {
                                break;
                            }
                        }
                        assert!(snapshots.len() > before, "transport proof has no {} record", stringify!($stored));
                    }};
                }
                collect!(Message);
                collect!(AgentSession);
                // The delivery adapter keeps the person consumer's record.
                collect!(Person);
                collect!(Convoy);
                collect!(AwaitingReply);
                collect!(Environment);
                collect!(ImageBuild);
                collect!(Clone);
                collect!(Checkout);
                collect!(TerminalSession);
                collect!(Vessel);
                collect!(Ensure);
                collect!(flotilla_provisioning::stored::account::AccountBinding);
                collect!(stored flotilla_provisioning::stored::account::ProvisionSpec);
                collect!(flotilla_provisioning::stored::manifest_root::ManifestRoot);
                collect!(stored flotilla_provisioning::stored::definition::ForgeDefinition);
                collect!(stored flotilla_provisioning::stored::definition::FleetDesignationDefinition);
                collect!(stored flotilla_provisioning::stored::definition::CredentialSpecDefinition);
                collect!(stored flotilla_provisioning::stored::definition::CredentialGrantDefinition);
                collect!(stored flotilla_provisioning::stored::definition::CrewDefaultsDefinition);
                collect!(stored flotilla_provisioning::stored::definition::ImageLayerDefinition);
                collect!(stored flotilla_provisioning::stored::definition::WorkflowTemplateDefinition);
                collect!(stored flotilla_provisioning::stored::repository::RepositoryRecord);
                collect!(stored flotilla_provisioning::stored::project::ProjectDefinition);
                collect!(stored flotilla_provisioning::stored::project::ProjectOpsRecord);
                collect!(stored flotilla_provisioning::stored::artifact::ArtifactRecord);
                collect!(stored flotilla_provisioning::stored::forge_observation::ChangeRequestRecord);
                collect!(stored flotilla_provisioning::stored::forge_observation::IssueRecord);
                collect!(stored flotilla_provisioning::stored::forge_observation::ForgeReadRecord);
                collect!(stored flotilla_provisioning::stored::placement::FulfilmentKindDefinition);
                collect!(stored flotilla_provisioning::stored::host::HostRecord);
                collect!(stored flotilla_provisioning::stored::host::HostObservation);
                assert_eq!(covered, registered, "every registered engine kind needs a transport fixture");
                Ok(snapshots)
            })
            .expect("engine snapshots")
    }
}

// Both live peer runtimes consume the real change stream. Every registered
// engine kind is read back at the other host with the same home, version and
// stored rows; the old replicator runs beside it and still copies old Convoys.
#[tokio::test(start_paused = true)]
async fn on_the_engine_two_daemons_replicate_every_kind_alongside_the_old_replicator() {
    use OldInventory::{HomeLocal, Replicated};
    // Replication is under test, not missed events: the production resync
    // keeps the paused clock's controller passes, which cost real time, few.
    let start = |host| DummyDaemon::start_with_relay().host(host).resync(RuntimeOptions::default().controller_resync_interval).call();
    let (a, b) = tokio::join!(start("engine-a"), start("engine-b"));
    tokio::join!(a.deliver("replicated-a", CrewMix::CoderReviewer), b.deliver("replicated-b", CrewMix::CoderReviewer));
    let _peers = flotilla_daemon_testkit::server::spawn_in_memory_request_topology_stateful(Arc::clone(&a.daemon), Arc::clone(&b.daemon))
        .await
        .expect("peer runtimes");
    for (source, destination, role) in [(&a, &b, "replicated-a"), (&b, &a, "replicated-b")] {
        let convoy = source.convoy(role).await.expect("source convoy");
        tokio::time::timeout(Duration::from_secs(60), async {
            while destination
                .daemon
                .message_primitives()
                .expect("engine")
                .convoys()
                .replica(&convoy.metadata.name, source.daemon.node_id().as_str())
                .expect("replica")
                .is_none()
            {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .expect("initial engine snapshot replicates");
    }
    // These admissions follow the initial snapshot, so the proof also needs
    // committed cursor continuation; an initial snapshot alone is insufficient.
    a.seed_transport_kinds();
    b.seed_transport_kinds();
    let expected_a = a.engine_snapshots();
    let expected_b = b.engine_snapshots();
    a.advance_transport_ensure();
    b.advance_transport_ensure();
    for (source, destination, expected) in [(&a, &b, &expected_a), (&b, &a, &expected_b)] {
        let mut mismatch = None;
        tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                // The source and replica reads are not atomic. A replica newer
                // than this cut fails equality and is checked again next poll.
                let current = source.engine_snapshots();
                let complete = destination
                    .daemon
                    .decided_store()
                    .expect("store")
                    .with_store(|store| {
                        let mut read = store.read()?;
                        for minimum in expected {
                            let snapshot = current
                                .iter()
                                .find(|snapshot| {
                                    snapshot.kind == minimum.kind && snapshot.name == minimum.name && snapshot.home == minimum.home
                                })
                                .unwrap_or_else(|| panic!("source retains transport record {} {}", minimum.kind, minimum.name));
                            assert!(
                                snapshot.version >= minimum.version,
                                "source version regressed for {} {}: {} < {}",
                                minimum.kind,
                                minimum.name,
                                snapshot.version,
                                minimum.version
                            );
                            let Some(id) = read.resolve(snapshot.kind, &snapshot.name, &snapshot.home)? else {
                                mismatch = Some((snapshot.kind, snapshot.name.clone(), snapshot.version, None));
                                return Ok(false);
                            };
                            let actual = read.load_snapshot(id)?;
                            if actual.as_ref() != Some(snapshot) {
                                mismatch =
                                    Some((snapshot.kind, snapshot.name.clone(), snapshot.version, actual.map(|value| value.version)));
                                return Ok(false);
                            }
                        }
                        Ok(true)
                    })
                    .expect("replicated engine rows");
                if complete {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .unwrap_or_else(|error| panic!("every engine kind replicates through the peer runtime: {error}; mismatch={mismatch:?}"));
        assert_old_inventory::<flotilla_resources::Convoy>(source, destination, Replicated).await;
        assert_old_inventory::<flotilla_resources::Vessel>(source, destination, Replicated).await;
        assert_old_inventory::<flotilla_resources::Environment>(source, destination, HomeLocal).await;
        assert_old_inventory::<flotilla_resources::Checkout>(source, destination, Replicated).await;
        assert_old_inventory::<flotilla_resources::TerminalSession>(source, destination, Replicated).await;
        assert_old_inventory::<flotilla_resources::Clone>(source, destination, HomeLocal).await;
        assert_old_inventory::<flotilla_resources::ImageBuild>(source, destination, Replicated).await;
        assert_old_inventory::<flotilla_resources::ConvoyEnsure>(source, destination, Replicated).await;
        assert_old_inventory::<flotilla_resources::Message>(source, destination, Replicated).await;
        assert!(
            destination
                .backend()
                .including_replicas::<flotilla_resources::Message>(NAMESPACE)
                .list()
                .await
                .expect("old messages")
                .items
                .is_empty(),
            "Messages live only on the engine"
        );
    }
}

// The old replicator copies old-store representations, not decided-store rows.
// Compare every old counterpart's actual inventory, including empty kinds.
#[derive(Clone, Copy, Debug)]
enum OldInventory {
    Replicated,
    HomeLocal,
}

async fn assert_old_inventory<T: flotilla_resources::Resource>(source: &DummyDaemon, destination: &DummyDaemon, inventory: OldInventory) {
    match inventory {
        OldInventory::HomeLocal => assert_eq!(T::REPLICATION_CLASS, flotilla_resources::ReplicationClass::None, "home-local old kind"),
        OldInventory::Replicated => assert_ne!(T::REPLICATION_CLASS, flotilla_resources::ReplicationClass::None, "replicated old kind"),
    }
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let expected: std::collections::BTreeMap<_, _> = source.backend().using::<T>(NAMESPACE).list().await.expect("old source inventory").items.into_iter()
                .map(|row| (row.metadata.name, serde_json::to_value(row.spec).expect("old spec"))).collect();
            let replicated: std::collections::BTreeMap<_, _> = destination.backend().including_replicas::<T>(NAMESPACE).list().await.expect("old replica inventory").items.into_iter()
                .filter(|row| matches!(&row.provenance, flotilla_resources::ResourceProvenance::Replica { origin_root, .. } if origin_root == source.daemon.node_id()))
                .map(|row| (row.object.metadata.name, serde_json::to_value(row.object.spec).expect("replica spec"))).collect();
            let expected = match inventory { OldInventory::HomeLocal => std::collections::BTreeMap::new(), OldInventory::Replicated => expected };
            if expected == replicated { break; }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }).await.unwrap_or_else(|error| panic!("old {} inventory does not replicate: {error}", T::API_PATHS.kind));
}

// Behaviour (#3824 layer 8b): the input-needed and declaration-refused Demands are gone; their
// facts are read from the records that hold them, with the switch off and on. A crew session that
// needs input raises its vessel's salience, and a refused declaration is listed by `fleet list`.
async fn judgements_scenario(primitives: bool) {
    use flotilla_protocol::{AwarenessGrouping, AwarenessKind, DeclarationAttentionKind, QueryCursor, QueryId, Salience};
    use flotilla_resources::{
        DeclarationRefusedCondition, Project, ProjectStatusPatch, TerminalAttention, TerminalAttentionSource, TerminalAttentionState,
        TerminalSession, TerminalSessionStatusPatch, CONVOY_LABEL,
    };

    let fleet =
        DummyDaemon::start_switched(&format!("{SCENARIO_AGENT}stall_rate = 1.0\n"), "", Duration::from_millis(200), primitives).await;
    fleet.dispatch("needs-input", CrewMix::Coder).await;
    let convoy = fleet.wait_for_stall("needs-input").await;
    let backend = fleet.backend();
    let sessions = backend.using::<TerminalSession>(NAMESPACE);
    let session = sessions
        .list()
        .await
        .expect("sessions")
        .items
        .into_iter()
        .find(|session| session.metadata.labels.get(CONVOY_LABEL) == Some(&convoy.metadata.name))
        .expect("the crew's session");
    let query = QueryId::Awareness { scope: None, grouping: AwarenessGrouping::Convoy, limit: Default::default() };
    let subscriber = uuid::Uuid::new_v4();
    let salience = || async {
        let events =
            fleet.daemon.subscribe_queries(subscriber, &[QueryCursor { query: query.clone(), since: None }]).await.expect("subscribe");
        events.into_iter().find_map(|event| match event {
            DaemonEvent::ResultSet(result) => result.rows.as_awareness().and_then(|nodes| {
                nodes.iter().flat_map(|node| &node.entries).find(|entry| entry.kind == AwarenessKind::Vessel).map(|entry| entry.salience)
            }),
            _ => None,
        })
    };
    // The stalled crew's hook reports it idle; the person's input is asked after that.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while sessions
        .get(&session.metadata.name)
        .await
        .expect("session")
        .status
        .and_then(|status| status.attention)
        .map(|attention| attention.state)
        != Some(TerminalAttentionState::Idle)
    {
        assert!(tokio::time::Instant::now() < deadline, "the stalled crew never reported idle");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let before = salience().await;
    assert!(before < Some(Salience::Attention), "the stalled vessel is not yet raised: {before:?}");
    let needs_input = TerminalSessionStatusPatch::ObserveAttention {
        attention: TerminalAttention {
            state: TerminalAttentionState::NeedsInput,
            as_of: chrono::Utc::now(),
            source: TerminalAttentionSource::Hook,
        },
    };
    flotilla_store::apply_status_patch(&sessions, &session.metadata.name, &needs_input).await.expect("needs input");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while salience().await != Some(Salience::Attention) {
        assert!(tokio::time::Instant::now() < deadline, "a session needing input never raised its vessel: {:?}", salience().await);
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    let projects = backend.definitions::<Project>(NAMESPACE);
    projects
        .create(
            &InputMeta::builder().name("refusing".into()).build(),
            &flotilla_resources::ProjectSpec::builder()
                .display_name("Refusing".to_string())
                .default_workflow_ref(CrewMix::Coder.workflow_name().to_string())
                .repositories(Vec::new())
                .build(),
        )
        .await
        .expect("project");
    let now = chrono::Utc::now();
    let refused = ProjectStatusPatch::DeclarationRefused {
        condition: Some(DeclarationRefusedCondition {
            entry_path: "ops/broken.md".into(),
            message: "unknown_option".into(),
            since: now,
            observed_at: now,
        }),
    };
    flotilla_store::apply_status_patch(&backend.using::<Project>(NAMESPACE), "refusing", &refused).await.expect("refusal");
    let listed = fleet.daemon.fleet_list_internal().await.expect("fleet list");
    assert!(
        listed.declaration_attention.iter().any(|row| row.resource.name == "refusing"
            && row.condition == DeclarationAttentionKind::DeclarationRefused
            && row.message.contains("unknown_option")),
        "{:?}",
        listed.declaration_attention
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn input_needed_and_declaration_refusals_are_read_from_their_records() {
    judgements_scenario(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn on_the_engine_input_needed_and_declaration_refusals_are_read_from_their_records() {
    judgements_scenario(true).await;
}
