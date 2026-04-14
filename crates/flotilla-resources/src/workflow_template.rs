use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::{
    resource::{ApiPaths, Resource},
    status_patch::NoStatusPatch,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkflowTemplate;

impl Resource for WorkflowTemplate {
    type Spec = WorkflowTemplateSpec;
    type Status = ();
    type StatusPatch = NoStatusPatch;

    const API_PATHS: ApiPaths = ApiPaths { group: "flotilla.work", version: "v1", plural: "workflowtemplates", kind: "WorkflowTemplate" };
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowTemplateSpec {
    #[serde(default)]
    pub inputs: Vec<InputDefinition>,
    pub tasks: Vec<TaskDefinition>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputDefinition {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskDefinition {
    pub name: String,
    #[serde(default)]
    pub depends_on: Vec<String>,
    pub processes: Vec<ProcessDefinition>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessDefinition {
    pub role: String,
    #[serde(flatten)]
    pub source: ProcessSource,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum ProcessSource {
    Agent {
        selector: Selector,
        #[serde(default)]
        prompt: Option<String>,
    },
    Tool {
        command: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Selector {
    pub capability: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidationError {
    DuplicateTaskName { name: String },
    DuplicateRoleInTask { task: String, role: String },
    UnknownDependency { task: String, missing: String },
    DependencyCycle { cycle: Vec<String> },
    DuplicateInputName { name: String },
    MalformedInterpolation { location: InterpolationLocation, text: String },
    UnknownInputReference { location: InterpolationLocation, name: String },
    UnknownWorkflowField { location: InterpolationLocation, name: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterpolationLocation {
    pub task: String,
    pub role: String,
    pub field: InterpolationField,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterpolationField {
    Prompt,
    Command,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum VisitState {
    Visiting,
    Visited,
}

pub fn validate(spec: &WorkflowTemplateSpec) -> Result<(), Vec<ValidationError>> {
    let mut errors = Vec::new();
    let declared_inputs = collect_inputs(spec, &mut errors);
    let tasks_by_name = collect_tasks(spec, &mut errors);

    for task in &spec.tasks {
        validate_task(task, &declared_inputs, &tasks_by_name, &mut errors);
    }
    validate_cycles(&tasks_by_name, &mut errors);

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

fn collect_inputs(spec: &WorkflowTemplateSpec, errors: &mut Vec<ValidationError>) -> BTreeSet<String> {
    let mut declared_inputs = BTreeSet::new();
    for input in &spec.inputs {
        if !declared_inputs.insert(input.name.clone()) {
            push_error(errors, ValidationError::DuplicateInputName { name: input.name.clone() });
        }
    }
    declared_inputs
}

fn collect_tasks<'a>(spec: &'a WorkflowTemplateSpec, errors: &mut Vec<ValidationError>) -> BTreeMap<String, &'a TaskDefinition> {
    let mut tasks_by_name = BTreeMap::new();
    for task in &spec.tasks {
        if tasks_by_name.insert(task.name.clone(), task).is_some() {
            push_error(errors, ValidationError::DuplicateTaskName { name: task.name.clone() });
        }
    }
    tasks_by_name
}

fn validate_task(
    task: &TaskDefinition,
    declared_inputs: &BTreeSet<String>,
    tasks_by_name: &BTreeMap<String, &TaskDefinition>,
    errors: &mut Vec<ValidationError>,
) {
    let mut roles = BTreeSet::new();
    for dependency in &task.depends_on {
        if !tasks_by_name.contains_key(dependency) {
            push_error(errors, ValidationError::UnknownDependency { task: task.name.clone(), missing: dependency.clone() });
        }
    }

    for process in &task.processes {
        if !roles.insert(process.role.clone()) {
            push_error(errors, ValidationError::DuplicateRoleInTask { task: task.name.clone(), role: process.role.clone() });
        }

        match &process.source {
            ProcessSource::Agent { prompt, .. } => {
                if let Some(prompt) = prompt {
                    validate_template_text(
                        prompt,
                        &InterpolationLocation { task: task.name.clone(), role: process.role.clone(), field: InterpolationField::Prompt },
                        declared_inputs,
                        errors,
                    );
                }
            }
            ProcessSource::Tool { command } => validate_template_text(
                command,
                &InterpolationLocation { task: task.name.clone(), role: process.role.clone(), field: InterpolationField::Command },
                declared_inputs,
                errors,
            ),
        }
    }
}

fn validate_cycles(tasks_by_name: &BTreeMap<String, &TaskDefinition>, errors: &mut Vec<ValidationError>) {
    let mut states = BTreeMap::new();
    let mut stack = Vec::new();

    for task_name in tasks_by_name.keys() {
        visit_task(task_name, tasks_by_name, &mut states, &mut stack, errors);
    }
}

fn visit_task(
    task_name: &str,
    tasks_by_name: &BTreeMap<String, &TaskDefinition>,
    states: &mut BTreeMap<String, VisitState>,
    stack: &mut Vec<String>,
    errors: &mut Vec<ValidationError>,
) {
    match states.get(task_name) {
        Some(VisitState::Visited) => return,
        None => {}
        Some(VisitState::Visiting) => unreachable!("cycle detection handles visiting dependencies before recursion"),
    }

    states.insert(task_name.to_string(), VisitState::Visiting);
    stack.push(task_name.to_string());

    if let Some(task) = tasks_by_name.get(task_name) {
        let mut dependencies = task.depends_on.iter().map(String::as_str).collect::<Vec<_>>();
        dependencies.sort_unstable();
        for dependency in dependencies {
            if !tasks_by_name.contains_key(dependency) {
                continue;
            }

            if states.get(dependency) == Some(&VisitState::Visiting) {
                if let Some(index) = stack.iter().position(|name| name == dependency) {
                    let mut cycle = stack[index..].to_vec();
                    cycle.push(dependency.to_string());
                    push_error(errors, ValidationError::DependencyCycle { cycle });
                }
                continue;
            }

            visit_task(dependency, tasks_by_name, states, stack, errors);
        }
    }

    stack.pop();
    states.insert(task_name.to_string(), VisitState::Visited);
}

fn validate_template_text(
    text: &str,
    location: &InterpolationLocation,
    declared_inputs: &BTreeSet<String>,
    errors: &mut Vec<ValidationError>,
) {
    let mut search_from = 0;
    while let Some(open_offset) = text[search_from..].find("{{") {
        let open = search_from + open_offset;
        let token_start = open + 2;
        match text[token_start..].find("}}") {
            Some(close_offset) => {
                let token_end = token_start + close_offset;
                validate_token(&text[token_start..token_end], location, declared_inputs, errors);
                search_from = token_end + 2;
            }
            None => {
                let token = &text[token_start..];
                if is_owned_token(token) {
                    push_error(errors, ValidationError::MalformedInterpolation { location: location.clone(), text: token.to_string() });
                }
                break;
            }
        }
    }
}

fn validate_token(token: &str, location: &InterpolationLocation, declared_inputs: &BTreeSet<String>, errors: &mut Vec<ValidationError>) {
    if !is_owned_token(token) {
        return;
    }

    if token.chars().any(char::is_whitespace) {
        push_error(errors, ValidationError::MalformedInterpolation { location: location.clone(), text: token.to_string() });
        return;
    }

    let segments = token.split('.').collect::<Vec<_>>();
    if segments.iter().any(|segment| segment.is_empty() || !segment.chars().all(is_valid_segment_char)) {
        push_error(errors, ValidationError::MalformedInterpolation { location: location.clone(), text: token.to_string() });
        return;
    }

    match segments.as_slice() {
        ["inputs", input_name] => {
            if !declared_inputs.contains(*input_name) {
                push_error(errors, ValidationError::UnknownInputReference { location: location.clone(), name: (*input_name).to_string() });
            }
        }
        ["workflow", "name"] | ["workflow", "namespace"] => {}
        ["workflow", field] => {
            push_error(errors, ValidationError::UnknownWorkflowField { location: location.clone(), name: (*field).to_string() })
        }
        [prefix, ..] if *prefix == "inputs" || *prefix == "workflow" => {
            push_error(errors, ValidationError::MalformedInterpolation { location: location.clone(), text: token.to_string() })
        }
        _ => {}
    }
}

fn is_owned_token(token: &str) -> bool {
    matches!(token.split('.').next(), Some("inputs" | "workflow"))
}

fn is_valid_segment_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_' || ch == '-'
}

fn push_error(errors: &mut Vec<ValidationError>, error: ValidationError) {
    if !errors.contains(&error) {
        errors.push(error);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        validate, InputDefinition, InterpolationField, InterpolationLocation, ProcessDefinition, ProcessSource, Selector, TaskDefinition,
        ValidationError, WorkflowTemplateSpec,
    };

    fn valid_spec() -> WorkflowTemplateSpec {
        WorkflowTemplateSpec {
            inputs: vec![InputDefinition { name: "feature".to_string(), description: None }],
            tasks: vec![
                TaskDefinition {
                    name: "implement".to_string(),
                    depends_on: Vec::new(),
                    processes: vec![
                        ProcessDefinition {
                            role: "coder".to_string(),
                            source: ProcessSource::Agent {
                                selector: Selector { capability: "code".to_string() },
                                prompt: Some("Implement {{inputs.feature}} for {{workflow.name}}".to_string()),
                            },
                        },
                        ProcessDefinition { role: "build".to_string(), source: ProcessSource::Tool { command: "cargo check".to_string() } },
                    ],
                },
                TaskDefinition {
                    name: "review".to_string(),
                    depends_on: vec!["implement".to_string()],
                    processes: vec![ProcessDefinition {
                        role: "reviewer".to_string(),
                        source: ProcessSource::Agent {
                            selector: Selector { capability: "code-review".to_string() },
                            prompt: Some("Review {{workflow.namespace}}".to_string()),
                        },
                    }],
                },
            ],
        }
    }

    #[test]
    fn validate_rejects_duplicate_task_names() {
        let mut spec = valid_spec();
        spec.tasks.push(spec.tasks[0].clone());

        let errors = validate(&spec).expect_err("duplicate task names should fail");
        assert!(errors.contains(&ValidationError::DuplicateTaskName { name: "implement".to_string() }));
    }

    #[test]
    fn validate_rejects_duplicate_role_names_within_task() {
        let mut spec = valid_spec();
        spec.tasks[0]
            .processes
            .push(ProcessDefinition { role: "coder".to_string(), source: ProcessSource::Tool { command: "cargo test".to_string() } });

        let errors = validate(&spec).expect_err("duplicate role names should fail");
        assert!(errors.contains(&ValidationError::DuplicateRoleInTask { task: "implement".to_string(), role: "coder".to_string() }));
    }

    #[test]
    fn validate_rejects_unknown_dependencies() {
        let mut spec = valid_spec();
        spec.tasks[1].depends_on = vec!["missing".to_string()];

        let errors = validate(&spec).expect_err("unknown dependencies should fail");
        assert!(errors.contains(&ValidationError::UnknownDependency { task: "review".to_string(), missing: "missing".to_string() }));
    }

    #[test]
    fn validate_rejects_cycles() {
        let mut spec = valid_spec();
        spec.tasks[0].depends_on = vec!["review".to_string()];

        let errors = validate(&spec).expect_err("cycles should fail");
        assert!(errors.contains(&ValidationError::DependencyCycle {
            cycle: vec!["implement".to_string(), "review".to_string(), "implement".to_string()],
        }));
    }

    #[test]
    fn validate_rejects_duplicate_input_names() {
        let mut spec = valid_spec();
        spec.inputs.push(InputDefinition { name: "feature".to_string(), description: Some("duplicate".to_string()) });

        let errors = validate(&spec).expect_err("duplicate inputs should fail");
        assert!(errors.contains(&ValidationError::DuplicateInputName { name: "feature".to_string() }));
    }

    #[test]
    fn validate_rejects_unknown_input_references() {
        let mut spec = valid_spec();
        spec.tasks[0].processes[0].source = ProcessSource::Agent {
            selector: Selector { capability: "code".to_string() },
            prompt: Some("Implement {{inputs.branch}}".to_string()),
        };

        let errors = validate(&spec).expect_err("unknown input references should fail");
        assert!(errors.contains(&ValidationError::UnknownInputReference {
            location: InterpolationLocation { task: "implement".to_string(), role: "coder".to_string(), field: InterpolationField::Prompt },
            name: "branch".to_string(),
        }));
    }

    #[test]
    fn validate_rejects_unknown_workflow_fields() {
        let mut spec = valid_spec();
        spec.tasks[0].processes[0].source = ProcessSource::Agent {
            selector: Selector { capability: "code".to_string() },
            prompt: Some("Implement {{workflow.uid}}".to_string()),
        };

        let errors = validate(&spec).expect_err("unknown workflow fields should fail");
        assert!(errors.contains(&ValidationError::UnknownWorkflowField {
            location: InterpolationLocation { task: "implement".to_string(), role: "coder".to_string(), field: InterpolationField::Prompt },
            name: "uid".to_string(),
        }));
    }

    #[test]
    fn validate_rejects_malformed_owned_interpolations() {
        let mut spec = valid_spec();
        spec.tasks[0].processes[0].source = ProcessSource::Agent {
            selector: Selector { capability: "code".to_string() },
            prompt: Some("Implement {{inputs.feature }} and {{workflow.name.extra}}".to_string()),
        };

        let errors = validate(&spec).expect_err("malformed owned interpolation should fail");
        assert!(errors.contains(&ValidationError::MalformedInterpolation {
            location: InterpolationLocation { task: "implement".to_string(), role: "coder".to_string(), field: InterpolationField::Prompt },
            text: "inputs.feature ".to_string(),
        }));
        assert!(errors.contains(&ValidationError::MalformedInterpolation {
            location: InterpolationLocation { task: "implement".to_string(), role: "coder".to_string(), field: InterpolationField::Prompt },
            text: "workflow.name.extra".to_string(),
        }));
    }

    #[test]
    fn validate_allows_foreign_interpolations() {
        let mut spec = valid_spec();
        spec.tasks[0].processes[1].source =
            ProcessSource::Tool { command: "kubectl get pod -o go-template='{{.metadata.name}}'".to_string() };

        assert!(validate(&spec).is_ok(), "foreign interpolations should pass through");
    }
}
