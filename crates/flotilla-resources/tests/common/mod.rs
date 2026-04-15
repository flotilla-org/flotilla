#![allow(dead_code)]

pub mod contract;

use std::{collections::BTreeMap, future::Future, time::Duration};

use chrono::{DateTime, TimeZone, Utc};
use flotilla_resources::{
    ApiPaths, Convoy as RealConvoy, ConvoySpec as RealConvoySpec, ConvoyStatus as RealConvoyStatus, InputDefinition, InputMeta, ObjectMeta,
    OwnerReference, ProcessDefinition, ProcessSource, Resource, ResourceObject, Selector, StatusPatch, TaskDefinition, TaskPhase,
    TaskState, WorkflowTemplate, WorkflowTemplateSpec,
};
use serde::{Deserialize, Serialize};
use tokio::{
    task::JoinHandle,
    time::{sleep, Instant},
};

pub struct ConvoyResource;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConvoySpec {
    pub template: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConvoyStatus {
    pub phase: String,
}

pub enum ConvoyStatusPatch {}

impl StatusPatch<ConvoyStatus> for ConvoyStatusPatch {
    fn apply(&self, _: &mut ConvoyStatus) {
        match *self {}
    }
}

impl Resource for ConvoyResource {
    type Spec = ConvoySpec;
    type Status = ConvoyStatus;
    type StatusPatch = ConvoyStatusPatch;

    const API_PATHS: ApiPaths = ApiPaths { group: "flotilla.work", version: "v1", plural: "convoys", kind: "Convoy" };
}

#[bon::builder]
pub fn resource_meta(
    name: &str,
    #[builder(default)] labels: BTreeMap<String, String>,
    #[builder(default)] annotations: BTreeMap<String, String>,
    #[builder(default)] owner_references: Vec<OwnerReference>,
    #[builder(default)] finalizers: Vec<String>,
    deletion_timestamp: Option<DateTime<Utc>>,
) -> InputMeta {
    InputMeta::builder()
        .name(name.to_string())
        .labels(labels)
        .annotations(annotations)
        .owner_references(owner_references)
        .finalizers(finalizers)
        .maybe_deletion_timestamp(deletion_timestamp)
        .build()
}

pub fn owner_reference(name: &str, kind: &str) -> OwnerReference {
    OwnerReference { api_version: "flotilla.work/v1".to_string(), kind: kind.to_string(), name: name.to_string(), controller: true }
}

pub fn input_meta(name: &str) -> InputMeta {
    resource_meta()
        .name(name)
        .labels([("app".to_string(), "flotilla".to_string())].into_iter().collect())
        .annotations([("note".to_string(), "test".to_string())].into_iter().collect())
        .call()
}

pub fn spec(template: &str) -> ConvoySpec {
    ConvoySpec { template: template.to_string() }
}

pub fn status(phase: &str) -> ConvoyStatus {
    ConvoyStatus { phase: phase.to_string() }
}

pub fn convoy_meta(name: &str) -> InputMeta {
    input_meta(name)
}

pub fn convoy_spec(workflow_ref: &str) -> RealConvoySpec {
    let mut spec = valid_convoy_spec();
    spec.workflow_ref = workflow_ref.to_string();
    spec
}

pub fn convoy_status(phase: flotilla_resources::ConvoyPhase) -> RealConvoyStatus {
    RealConvoyStatus {
        phase,
        workflow_snapshot: None,
        tasks: Default::default(),
        message: None,
        started_at: None,
        finished_at: None,
        observed_workflow_ref: None,
        observed_workflows: None,
    }
}

pub fn workflow_template_meta(name: &str) -> InputMeta {
    resource_meta()
        .name(name)
        .labels([("app".to_string(), "flotilla".to_string())].into_iter().collect())
        .annotations([("note".to_string(), "workflow-template-test".to_string())].into_iter().collect())
        .call()
}

pub fn valid_workflow_template_spec() -> WorkflowTemplateSpec {
    WorkflowTemplateSpec::builder()
        .inputs(vec![
            InputDefinition { name: "feature".to_string(), description: Some("Brief description of the feature to implement".to_string()) },
            InputDefinition { name: "branch".to_string(), description: Some("Target git branch".to_string()) },
        ])
        .tasks(vec![
            TaskDefinition::builder()
                .name("implement".to_string())
                .processes(vec![
                    ProcessDefinition::builder()
                        .role("coder".to_string())
                        .source(ProcessSource::Agent {
                            selector: Selector { capability: "code".to_string() },
                            prompt: Some(
                                "Convoy {{workflow.name}} - implement {{inputs.feature}} on branch {{inputs.branch}}.".to_string(),
                            ),
                        })
                        .build(),
                    ProcessDefinition::builder()
                        .role("build".to_string())
                        .source(ProcessSource::Tool { command: "cargo watch -x check".to_string() })
                        .build(),
                ])
                .build(),
            TaskDefinition::builder()
                .name("review".to_string())
                .depends_on(vec!["implement".to_string()])
                .processes(vec![
                    ProcessDefinition::builder()
                        .role("reviewer".to_string())
                        .source(ProcessSource::Agent {
                            selector: Selector { capability: "code-review".to_string() },
                            prompt: Some("Review branch {{inputs.branch}} for correctness and style.".to_string()),
                        })
                        .build(),
                    ProcessDefinition::builder()
                        .role("tests".to_string())
                        .source(ProcessSource::Tool { command: "cargo test --watch".to_string() })
                        .build(),
                ])
                .build(),
        ])
        .build()
}

pub fn updated_workflow_template_spec() -> WorkflowTemplateSpec {
    let mut spec = valid_workflow_template_spec();
    if let ProcessSource::Tool { command } = &mut spec.tasks[0].processes[1].source {
        *command = "cargo check --all-targets".to_string();
    }
    spec
}

pub fn valid_workflow_template_yaml() -> &'static str {
    include_str!("../../examples/review-and-fix.yaml")
}

pub fn timestamp(seconds: i64) -> chrono::DateTime<Utc> {
    Utc.timestamp_opt(seconds, 0).single().expect("valid timestamp")
}

pub fn object_meta(name: &str, namespace: &str, resource_version: &str) -> ObjectMeta {
    ObjectMeta {
        name: name.to_string(),
        namespace: namespace.to_string(),
        resource_version: resource_version.to_string(),
        labels: Default::default(),
        annotations: Default::default(),
        owner_references: Vec::new(),
        finalizers: Vec::new(),
        deletion_timestamp: None,
        creation_timestamp: timestamp(1),
    }
}

pub fn valid_convoy_spec() -> RealConvoySpec {
    RealConvoySpec {
        workflow_ref: "review-and-fix".to_string(),
        inputs: [
            ("feature".to_string(), flotilla_resources::InputValue::String("Retry logic".to_string())),
            ("branch".to_string(), flotilla_resources::InputValue::String("fix-retry-logic".to_string())),
        ]
        .into_iter()
        .collect(),
        placement_policy: Some("laptop-docker".to_string()),
        repository: None,
        r#ref: None,
    }
}

pub fn task_provisioning_convoy_spec() -> RealConvoySpec {
    let mut spec = valid_convoy_spec();
    spec.repository = Some(flotilla_resources::ConvoyRepositorySpec { url: "git@github.com:flotilla-org/flotilla.git".to_string() });
    spec.r#ref = Some("feat/task-provisioning".to_string());
    spec
}

pub fn pending_task_state() -> TaskState {
    TaskState { phase: TaskPhase::Pending, ready_at: None, started_at: None, finished_at: None, message: None, placement: None }
}

pub fn valid_workflow_template_object(name: &str) -> ResourceObject<WorkflowTemplate> {
    ResourceObject { metadata: object_meta(name, "flotilla", "42"), spec: valid_workflow_template_spec(), status: None }
}

pub fn convoy_object(name: &str, spec: RealConvoySpec, status: Option<RealConvoyStatus>) -> ResourceObject<RealConvoy> {
    ResourceObject { metadata: object_meta(name, "flotilla", "7"), spec, status }
}

pub fn tool_only_workflow_template_spec() -> WorkflowTemplateSpec {
    WorkflowTemplateSpec::builder()
        .inputs(vec![
            InputDefinition { name: "feature".to_string(), description: Some("Brief description of the feature to implement".to_string()) },
            InputDefinition { name: "branch".to_string(), description: Some("Target git branch".to_string()) },
        ])
        .tasks(vec![
            TaskDefinition::builder()
                .name("implement".to_string())
                .processes(vec![
                    ProcessDefinition::builder()
                        .role("coder".to_string())
                        .source(ProcessSource::Tool { command: "cargo check".to_string() })
                        .build(),
                    ProcessDefinition::builder()
                        .role("build".to_string())
                        .source(ProcessSource::Tool { command: "cargo test --no-run".to_string() })
                        .build(),
                ])
                .build(),
            TaskDefinition::builder()
                .name("review".to_string())
                .depends_on(vec!["implement".to_string()])
                .processes(vec![
                    ProcessDefinition::builder()
                        .role("review".to_string())
                        .source(ProcessSource::Tool { command: "cargo test".to_string() })
                        .build(),
                    ProcessDefinition::builder()
                        .role("lint".to_string())
                        .source(ProcessSource::Tool { command: "cargo clippy --no-deps".to_string() })
                        .build(),
                ])
                .build(),
        ])
        .build()
}

pub fn tool_only_workflow_template_object(name: &str) -> ResourceObject<WorkflowTemplate> {
    ResourceObject { metadata: object_meta(name, "flotilla", "42"), spec: tool_only_workflow_template_spec(), status: None }
}

pub fn bootstrapped_convoy_status() -> RealConvoyStatus {
    let snapshot = flotilla_resources::WorkflowSnapshot {
        tasks: valid_workflow_template_spec()
            .tasks
            .into_iter()
            .map(|task| flotilla_resources::SnapshotTask { name: task.name, depends_on: task.depends_on, processes: task.processes })
            .collect(),
    };
    let tasks = [("implement".to_string(), pending_task_state()), ("review".to_string(), pending_task_state())].into_iter().collect();

    RealConvoyStatus {
        phase: flotilla_resources::ConvoyPhase::Pending,
        workflow_snapshot: Some(snapshot),
        tasks,
        message: None,
        started_at: None,
        finished_at: None,
        observed_workflow_ref: Some("review-and-fix".to_string()),
        observed_workflows: Some([("review-and-fix".to_string(), "42".to_string())].into_iter().collect()),
    }
}

pub struct TestLoopHarness {
    handles: Vec<JoinHandle<()>>,
}

impl TestLoopHarness {
    pub fn new() -> Self {
        Self { handles: Vec::new() }
    }

    pub fn spawn<F>(&mut self, future: F)
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.handles.push(tokio::spawn(async move {
            let _ = future.await;
        }));
    }

    pub async fn wait_until<F, Fut>(&self, timeout: Duration, condition: F)
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = bool>,
    {
        wait_until(timeout, condition).await;
    }

    pub async fn shutdown(mut self) {
        for handle in self.handles.drain(..) {
            handle.abort();
            let _ = handle.await;
        }
    }
}

impl Drop for TestLoopHarness {
    fn drop(&mut self) {
        for handle in self.handles.drain(..) {
            handle.abort();
        }
    }
}

#[allow(dead_code)]
pub async fn wait_until<F, Fut>(timeout: Duration, mut condition: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if condition().await {
            return;
        }
        sleep(Duration::from_millis(20)).await;
    }
    panic!("condition was not satisfied within {:?}", timeout);
}

pub fn bootstrapped_tool_only_convoy_status() -> RealConvoyStatus {
    let snapshot = flotilla_resources::WorkflowSnapshot {
        tasks: tool_only_workflow_template_spec()
            .tasks
            .into_iter()
            .map(|task| flotilla_resources::SnapshotTask { name: task.name, depends_on: task.depends_on, processes: task.processes })
            .collect(),
    };
    let tasks = [("implement".to_string(), pending_task_state()), ("review".to_string(), pending_task_state())].into_iter().collect();

    RealConvoyStatus {
        phase: flotilla_resources::ConvoyPhase::Pending,
        workflow_snapshot: Some(snapshot),
        tasks,
        message: None,
        started_at: None,
        finished_at: None,
        observed_workflow_ref: Some("review-and-fix".to_string()),
        observed_workflows: Some([("review-and-fix".to_string(), "42".to_string())].into_iter().collect()),
    }
}
