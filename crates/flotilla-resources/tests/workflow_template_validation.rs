mod common;

use common::{valid_workflow_template_spec, valid_workflow_template_yaml};
use flotilla_resources::{validate, InterpolationField, InterpolationLocation, ValidationError, WorkflowTemplateSpec};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct WorkflowTemplateDocument {
    spec: WorkflowTemplateSpec,
}

fn parse_spec(yaml: &str) -> WorkflowTemplateSpec {
    serde_yml::from_str(yaml).expect("parse workflow template spec")
}

fn assert_has_error(errors: &[ValidationError], expected: &ValidationError) {
    assert!(errors.contains(expected), "missing expected error {expected:?} in {errors:?}");
}

#[test]
fn parse_rejects_process_without_selector_or_command() {
    let yaml = r#"
inputs: []
tasks:
  - name: implement
    processes:
      - role: coder
"#;

    let error = serde_yml::from_str::<WorkflowTemplateSpec>(yaml).expect_err("parse should fail");
    let message = error.to_string();
    assert!(message.contains("data did not match any variant"), "unexpected error: {message}");
}

#[test]
fn parse_rejects_process_with_selector_and_command() {
    let yaml = r#"
inputs: []
tasks:
  - name: implement
    processes:
      - role: coder
        selector:
          capability: code
        command: cargo test
"#;

    let error = serde_yml::from_str::<WorkflowTemplateSpec>(yaml).expect_err("parse should fail");
    let message = error.to_string();
    assert!(message.contains("data did not match any variant"), "unexpected error: {message}");
}

#[test]
fn parse_rejects_prompt_on_tool_process() {
    let yaml = r#"
inputs: []
tasks:
  - name: implement
    processes:
      - role: coder
        command: cargo test
        prompt: should-not-be-here
"#;

    let error = serde_yml::from_str::<WorkflowTemplateSpec>(yaml).expect_err("parse should fail");
    let message = error.to_string();
    assert!(message.contains("data did not match any variant"), "unexpected error: {message}");
}

#[test]
fn validate_rejects_duplicate_task_names() {
    let mut spec = valid_workflow_template_spec();
    spec.tasks.push(spec.tasks[0].clone());

    let errors = validate(&spec).expect_err("validation should fail");
    assert_has_error(&errors, &ValidationError::DuplicateTaskName { name: "implement".to_string() });
}

#[test]
fn validate_rejects_duplicate_input_names() {
    let mut spec = valid_workflow_template_spec();
    spec.inputs.push(spec.inputs[0].clone());

    let errors = validate(&spec).expect_err("validation should fail");
    assert_has_error(&errors, &ValidationError::DuplicateInputName { name: "feature".to_string() });
}

#[test]
fn validate_rejects_duplicate_role_names_within_task() {
    let mut spec = valid_workflow_template_spec();
    let duplicate_process = spec.tasks[0].processes[0].clone();
    spec.tasks[0].processes.push(duplicate_process);

    let errors = validate(&spec).expect_err("validation should fail");
    assert_has_error(&errors, &ValidationError::DuplicateRoleInTask { task: "implement".to_string(), role: "coder".to_string() });
}

#[test]
fn validate_rejects_unknown_dependencies() {
    let mut spec = valid_workflow_template_spec();
    spec.tasks[1].depends_on = vec!["missing".to_string()];

    let errors = validate(&spec).expect_err("validation should fail");
    assert_has_error(&errors, &ValidationError::UnknownDependency { task: "review".to_string(), missing: "missing".to_string() });
}

#[test]
fn validate_rejects_cycles() {
    let mut spec = valid_workflow_template_spec();
    spec.tasks[0].depends_on = vec!["review".to_string()];

    let errors = validate(&spec).expect_err("validation should fail");
    assert_has_error(&errors, &ValidationError::DependencyCycle {
        cycle: vec!["implement".to_string(), "review".to_string(), "implement".to_string()],
    });
}

#[test]
fn validate_rejects_unknown_input_references() {
    let mut spec = valid_workflow_template_spec();
    if let Some(prompt) = match &mut spec.tasks[0].processes[0].source {
        flotilla_resources::ProcessSource::Agent { prompt, .. } => prompt,
        _ => unreachable!("first process should be an agent"),
    } {
        *prompt = "Implement {{inputs.missing}}".to_string();
    }

    let errors = validate(&spec).expect_err("validation should fail");
    assert_has_error(&errors, &ValidationError::UnknownInputReference {
        location: InterpolationLocation { task: "implement".to_string(), role: "coder".to_string(), field: InterpolationField::Prompt },
        name: "missing".to_string(),
    });
}

#[test]
fn validate_rejects_unknown_workflow_fields() {
    let mut spec = valid_workflow_template_spec();
    if let Some(prompt) = match &mut spec.tasks[0].processes[0].source {
        flotilla_resources::ProcessSource::Agent { prompt, .. } => prompt,
        _ => unreachable!("first process should be an agent"),
    } {
        *prompt = "Implement {{workflow.uid}}".to_string();
    }

    let errors = validate(&spec).expect_err("validation should fail");
    assert_has_error(&errors, &ValidationError::UnknownWorkflowField {
        location: InterpolationLocation { task: "implement".to_string(), role: "coder".to_string(), field: InterpolationField::Prompt },
        name: "uid".to_string(),
    });
}

#[test]
fn validate_rejects_malformed_owned_interpolations() {
    let mut spec = valid_workflow_template_spec();
    if let Some(prompt) = match &mut spec.tasks[0].processes[0].source {
        flotilla_resources::ProcessSource::Agent { prompt, .. } => prompt,
        _ => unreachable!("first process should be an agent"),
    } {
        *prompt = "Implement {{inputs.branch }} then {{workflow.name.extra}}".to_string();
    }

    let errors = validate(&spec).expect_err("validation should fail");
    assert_has_error(&errors, &ValidationError::MalformedInterpolation {
        location: InterpolationLocation { task: "implement".to_string(), role: "coder".to_string(), field: InterpolationField::Prompt },
        text: "inputs.branch ".to_string(),
    });
    assert_has_error(&errors, &ValidationError::MalformedInterpolation {
        location: InterpolationLocation { task: "implement".to_string(), role: "coder".to_string(), field: InterpolationField::Prompt },
        text: "workflow.name.extra".to_string(),
    });
}

#[test]
fn validate_allows_foreign_interpolations() {
    let spec = parse_spec(
        r#"
inputs: []
tasks:
  - name: implement
    processes:
      - role: build
        command: "kubectl get pod -o go-template='{{.metadata.name}}'"
"#,
    );

    assert!(validate(&spec).is_ok(), "foreign interpolation should pass through");
}

#[test]
fn validate_rejects_reserved_process_label_keys() {
    let mut spec = valid_workflow_template_spec();
    spec.tasks[0].processes[0].labels.insert("flotilla.work/convoy".to_string(), "manual".to_string());

    let errors = validate(&spec).expect_err("reserved labels should fail validation");
    assert_has_error(&errors, &ValidationError::ReservedLabelKey {
        task: "implement".to_string(),
        role: "coder".to_string(),
        key: "flotilla.work/convoy".to_string(),
    });
}

#[test]
fn validate_allows_non_reserved_process_label_keys() {
    let spec = parse_spec(
        r#"
inputs: []
tasks:
  - name: implement
    processes:
      - role: build
        command: cargo test
        labels:
          service: api
          queue: fast-lane
"#,
    );

    assert!(validate(&spec).is_ok(), "non-reserved labels should validate");
}

#[test]
fn parser_round_trip_preserves_sample_workflow() {
    let first: WorkflowTemplateDocument = serde_yml::from_str(valid_workflow_template_yaml()).expect("parse workflow template document");
    let encoded = serde_yml::to_string(&first.spec).expect("serialize workflow template spec");
    let second: WorkflowTemplateSpec = serde_yml::from_str(&encoded).expect("re-parse workflow template spec");

    assert_eq!(second, first.spec);
}
