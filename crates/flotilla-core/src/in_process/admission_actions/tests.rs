use super::super::action_events::ActionEvents;
use super::super::convoy_admission::HandlerContractFixture;
use super::*;
use crate::event_sink::EventSink;
use crate::event_sink::RecordingEventSink;
use flotilla_protocol::{ConvoyStartIntent, NodeId};
use flotilla_resources::{Convoy, CrewSource, CrewSpec, VesselRequirement, WorkflowTemplateSpec};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

// Boundary stand-in: the asynchronous admission worker can be unavailable or
// retain a queued task without running a process. Other integrations are unused.
#[derive(Default)]
struct Worker {
    available: AtomicBool,
    inspections: AtomicUsize,
    tasks: std::sync::Mutex<Vec<ConvoyStartTask>>,
}
#[async_trait]
impl AdmissionActionPort for Worker {
    // Stand-in for Git inspection I/O; checkout persistence uses real stores.
    async fn inspect_adopted_checkout(
        &self,
        path: &Path,
        repository_url: Option<&str>,
        git_ref: Option<&str>,
    ) -> Result<RepositoryInspection, String> {
        self.inspections.fetch_add(1, Ordering::SeqCst);
        let url = repository_url.expect("explicit repository");
        Ok(RepositoryInspection {
            spec: RepositorySpec::remote(url)?,
            checkout: crate::repository_inspection::LocalCheckoutInspection {
                path: path.into(),
                host_ref: "host".into(),
                git_ref: git_ref.unwrap().into(),
                is_main: false,
            },
            transport_url: Some(url.into()),
            replaces_prior_repository: false,
        })
    }
    async fn project_add(&self, _: &str, _: Option<&str>, _: Option<&str>, _: Option<&str>) -> Result<String, String> {
        panic!("no project edits")
    }
    async fn project_refresh(&self, _: &str) -> Result<(usize, bool, Vec<String>, Vec<String>), String> {
        panic!("no project edits")
    }
    async fn project_register(&self, _: &str) -> Result<(String, usize), String> {
        panic!("no project edits")
    }
    async fn resolve_repository_remote(&self, remote: &str) -> Result<RepositorySpec, String> {
        RepositorySpec::remote(remote)
    }
    async fn roll_convoy_ensure(&self, _: &str, _: &str) -> Result<String, String> {
        panic!("no ensure roll")
    }
    fn spawn_convoy_start(&self, task: ConvoyStartTask) -> bool {
        if !self.available.load(Ordering::SeqCst) {
            return false;
        }
        self.tasks.lock().unwrap().push(task);
        true
    }
}

fn handlers<'a>(
    worker: &'a Worker,
    owner: &'a ConvoyAdmission,
    backend: &'a ResourceBackend,
    observed: &'a ResourceBackend,
    events: ActionEvents<'a>,
    reconciliation: &'a Arc<Mutex<()>>,
    namespace: &'a std::sync::RwLock<String>,
) -> AdmissionActions<'a> {
    AdmissionActions {
        port: worker,
        convoy_admission: owner,
        resource_backend: backend,
        observed_resource_backend: observed,
        observed_checkout_reconciliation: reconciliation,
        namespace,
        events,
    }
}

// #2975: admission emits Started before Finished, freezes inputs and principal,
// and serializes competing admissions so the duplicate creates no resources.
#[tokio::test]
async fn direct_admission_contract_success_and_duplicate() {
    let HandlerContractFixture { owner, backend, observed, reconciliation, namespace, temp } =
        super::super::convoy_admission::handler_contract_fixture().await;
    let worker = Worker::default();
    let recording = Arc::new(RecordingEventSink::default());
    let sink: Arc<dyn EventSink> = recording.clone();
    let node = NodeId::new("handler");
    let actions = handlers(&worker, &owner, &backend, &observed, ActionEvents { sink: &sink, node_id: &node }, &reconciliation, &namespace);
    let workflow = WorkflowTemplateSpec::builder()
        .vessels(vec![VesselRequirement::builder()
            .name("work".into())
            .crew(vec![CrewSpec::builder().role("coder".into()).source(CrewSource::Tool { command: "true".into() }).build()])
            .build()])
        .build();
    backend.using::<WorkflowTemplate>("flotilla").create(&InputMeta::builder().name("work".into()).build(), &workflow).await.unwrap();
    let command = Command::builder()
        .action(CommandAction::ConvoyCreate {
            name: "contract".into(),
            workflow_ref: "work".into(),
            inputs: vec![("task".into(), "preserve me".into())],
            repository_url: Some("https://github.com/example/project".into()),
            r#ref: Some("feature/contract".into()),
            project_ref: None,
            placement_policy: None,
            adopted_checkout: Some(Box::new(temp.path().to_path_buf())),
        })
        .build();
    let principal = Some(PrincipalRef { namespace: "flotilla".into(), name: "operator".into() });
    let (first, duplicate) = tokio::join!(
        actions.execute_action_convoy_create(1, &command, &principal),
        actions.execute_action_convoy_create(2, &command, &principal)
    );
    assert_eq!(first.unwrap(), 1);
    assert_eq!(duplicate.unwrap(), 2);
    let events = recording.events();
    for id in [1, 2] {
        let own: Vec<_> = events.iter().filter(|e| matches!(e, DaemonEvent::CommandStarted { command_id, .. } | DaemonEvent::CommandFinished { command_id, .. } if *command_id == id)).collect();
        assert!(
            matches!(own.as_slice(), [DaemonEvent::CommandStarted { node_id, .. }, DaemonEvent::CommandFinished { .. }] if node_id == &node)
        );
    }
    assert_eq!(
        events.iter().filter(|e| matches!(e, DaemonEvent::CommandFinished { result: CommandValue::ConvoyCreated { .. }, .. })).count(),
        1
    );
    assert_eq!(events.iter().filter(|e| matches!(e, DaemonEvent::CommandFinished { result: CommandValue::Error { message }, .. } if message.contains("already exists"))).count(), 1);
    let convoys = backend.using::<Convoy>("flotilla").list().await.unwrap().items;
    assert_eq!(convoys.len(), 1);
    assert_eq!(Some(convoys[0].spec.dispatching_principal_ref.clone()), principal);
    assert_eq!(convoys[0].spec.inputs["task"], flotilla_resources::InputValue::String("preserve me".into()));
    assert_eq!(convoys[0].spec.role, "contract");
    assert_eq!(worker.inspections.load(Ordering::SeqCst), 1, "duplicate must refuse before checkout inspection");
    for store in [&backend, &observed] {
        let checkouts = store.using::<flotilla_resources::Checkout>("flotilla").list().await.unwrap().items;
        assert_eq!(checkouts.len(), 1, "duplicate must not leave an orphan checkout");
        assert!(convoys[0].spec.adopted_checkout_refs.values().any(|name| name == &checkouts[0].metadata.name));
    }
}

// #2975: failed worker submission clears the pending key; a queued duplicate
// refuses without submission, and owner cleanup allows the next attempt.
#[tokio::test]
async fn direct_admission_contract_worker_cleanup_and_duplicate() {
    let HandlerContractFixture { owner, backend, observed, reconciliation, namespace, temp: _temp } =
        super::super::convoy_admission::handler_contract_fixture().await;
    let worker = Worker::default();
    let recording = Arc::new(RecordingEventSink::default());
    let sink: Arc<dyn EventSink> = recording.clone();
    let node = NodeId::new("handler");
    let actions = handlers(&worker, &owner, &backend, &observed, ActionEvents { sink: &sink, node_id: &node }, &reconciliation, &namespace);
    let intent = ConvoyStartIntent::builder().project_ref("project".into()).name("unit".into()).build();
    let command = Command::builder().action(CommandAction::ConvoyStart { intent: Box::new(intent.clone()) }).build();
    actions.execute_action_convoy_start(1, &command, &None).await.unwrap();
    assert!(
        matches!(&recording.events()[1], DaemonEvent::CommandFinished { result: CommandValue::Error { message }, .. } if message == "convoy start worker is unavailable")
    );
    worker.available.store(true, Ordering::SeqCst);
    let principal = Some(PrincipalRef { namespace: "flotilla".into(), name: "operator".into() });
    actions.execute_action_convoy_start(2, &command, &principal).await.unwrap();
    actions.execute_action_convoy_start(3, &command, &None).await.unwrap();
    {
        let tasks = worker.tasks.lock().unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].command_id, 2);
        assert_eq!(Some(tasks[0].dispatching_principal_ref.clone()), principal);
    }
    assert!(
        matches!(recording.events().last().unwrap(), DaemonEvent::CommandFinished { command_id: 3, result: CommandValue::Error { message }, .. } if message.contains("already in progress"))
    );
    owner.clear_pending(&ConvoyStartKey::new("flotilla".into(), &intent)).await;
    actions.execute_action_convoy_start(4, &command, &None).await.unwrap();
    assert_eq!(worker.tasks.lock().unwrap().len(), 2);
    assert_eq!(worker.tasks.lock().unwrap()[1].dispatching_principal_ref, PrincipalRef::implicit_for_namespace("flotilla"));
}
