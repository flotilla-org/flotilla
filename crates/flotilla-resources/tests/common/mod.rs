#![allow(dead_code)]

use flotilla_resources::{
    ApiPaths, Convoy as RealConvoy, ConvoySpec as RealConvoySpec, ConvoyStatus as RealConvoyStatus, InputDefinition, InputMeta,
    ObjectMeta, ProcessDefinition, ProcessSource, Resource, ResourceObject, Selector, StatusPatch, TaskDefinition, TaskPhase,
    TaskState, WorkflowTemplate, WorkflowTemplateSpec,
};
use serde::{Deserialize, Serialize};
use chrono::{TimeZone, Utc};

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

pub fn input_meta(name: &str) -> InputMeta {
    InputMeta {
        name: name.to_string(),
        labels: [("app".to_string(), "flotilla".to_string())].into_iter().collect(),
        annotations: [("note".to_string(), "test".to_string())].into_iter().collect(),
    }
}

pub fn spec(template: &str) -> ConvoySpec {
    ConvoySpec { template: template.to_string() }
}

pub fn status(phase: &str) -> ConvoyStatus {
    ConvoyStatus { phase: phase.to_string() }
}

pub fn workflow_template_meta(name: &str) -> InputMeta {
    InputMeta {
        name: name.to_string(),
        labels: [("app".to_string(), "flotilla".to_string())].into_iter().collect(),
        annotations: [("note".to_string(), "workflow-template-test".to_string())].into_iter().collect(),
    }
}

pub fn valid_workflow_template_spec() -> WorkflowTemplateSpec {
    WorkflowTemplateSpec {
        inputs: vec![
            InputDefinition { name: "feature".to_string(), description: Some("Brief description of the feature to implement".to_string()) },
            InputDefinition { name: "branch".to_string(), description: Some("Target git branch".to_string()) },
        ],
        tasks: vec![
            TaskDefinition {
                name: "implement".to_string(),
                depends_on: Vec::new(),
                processes: vec![
                    ProcessDefinition {
                        role: "coder".to_string(),
                        source: ProcessSource::Agent {
                            selector: Selector { capability: "code".to_string() },
                            prompt: Some(
                                "Convoy {{workflow.name}} - implement {{inputs.feature}} on branch {{inputs.branch}}.".to_string(),
                            ),
                        },
                    },
                    ProcessDefinition {
                        role: "build".to_string(),
                        source: ProcessSource::Tool { command: "cargo watch -x check".to_string() },
                    },
                ],
            },
            TaskDefinition {
                name: "review".to_string(),
                depends_on: vec!["implement".to_string()],
                processes: vec![
                    ProcessDefinition {
                        role: "reviewer".to_string(),
                        source: ProcessSource::Agent {
                            selector: Selector { capability: "code-review".to_string() },
                            prompt: Some("Review branch {{inputs.branch}} for correctness and style.".to_string()),
                        },
                    },
                    ProcessDefinition {
                        role: "tests".to_string(),
                        source: ProcessSource::Tool { command: "cargo test --watch".to_string() },
                    },
                ],
            },
        ],
    }
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
    }
}

pub fn pending_task_state() -> TaskState {
    TaskState { phase: TaskPhase::Pending, ready_at: None, started_at: None, finished_at: None, message: None, placement: None }
}

pub fn valid_workflow_template_object(name: &str) -> ResourceObject<WorkflowTemplate> {
    ResourceObject {
        metadata: object_meta(name, "flotilla", "42"),
        spec: valid_workflow_template_spec(),
        status: None,
    }
}

pub fn convoy_object(name: &str, spec: RealConvoySpec, status: Option<RealConvoyStatus>) -> ResourceObject<RealConvoy> {
    ResourceObject { metadata: object_meta(name, "flotilla", "7"), spec, status }
}

pub fn bootstrapped_convoy_status() -> RealConvoyStatus {
    let snapshot = flotilla_resources::WorkflowSnapshot {
        tasks: valid_workflow_template_spec()
            .tasks
            .into_iter()
            .map(|task| flotilla_resources::SnapshotTask {
                name: task.name,
                depends_on: task.depends_on,
                processes: task.processes,
            })
            .collect(),
    };
    let tasks = [
        ("implement".to_string(), pending_task_state()),
        ("review".to_string(), pending_task_state()),
    ]
    .into_iter()
    .collect();

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
