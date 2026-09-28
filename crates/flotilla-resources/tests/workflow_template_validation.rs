mod common;

use common::{valid_workflow_template_spec, valid_workflow_template_yaml};
use flotilla_protocol::{IssueRef, IssueSource};
use flotilla_resources::{
    admit_leaf, implement_review_workflow_spec, interactive_single_workflow_spec, issue_address, issue_address_with_forges,
    issue_record_name, single_agent_shepherd_workflow_spec, single_agent_workflow_spec, validate, ExitDeclaration, InterpolationField,
    InterpolationLocation, RepositoryKey, ValidationError, WorkflowTemplateSpec,
};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct WorkflowTemplateDocument {
    spec: WorkflowTemplateSpec,
}

fn parse_spec(yaml: &str) -> WorkflowTemplateSpec {
    serde_yml::from_str(yaml).expect("parse workflow template spec")
}

#[test]
fn exit_table_roundtrips_declared_entries_in_order() {
    let yaml = r#"
inputs: []
exit:
  merged: $cr.state == merged
  closed-unmerged: $cr.state == closed
vessels:
  - name: implement
    crew:
      - role: coder
        selector:
          capability: code
"#;

    let declared = parse_spec(yaml);
    let undeclared = parse_spec(
        r#"
inputs: []
vessels:
  - name: implement
    crew:
      - role: coder
        selector:
          capability: code
"#,
    );
    assert!(declared.exit.is_some(), "transcribed table must remain declared");
    assert!(undeclared.exit.is_none(), "an undeclared template must have no exit");

    let serialized = serde_yml::to_string(&declared).expect("serialize workflow template spec");
    let merged = serialized.find("merged: $cr.state == merged").expect("merged exit entry");
    let closed = serialized.find("closed-unmerged: $cr.state == closed").expect("closed exit entry");
    assert!(merged < closed, "exit entry declaration order should be preserved: {serialized}");
}

#[test]
fn claim_exit_roundtrips_as_claim() {
    let yaml = r#"
inputs: []
exit: claim
vessels:
  - name: implement
    crew:
      - role: coder
        selector:
          capability: code
"#;

    let serialized = serde_yml::to_string(&parse_spec(yaml)).expect("serialize workflow template spec");
    assert!(serialized.contains("exit: claim"), "claim exit should remain declarable: {serialized}");
}

#[test]
fn turn_delivery_rules_roundtrip_as_named_independent_rules() {
    let yaml = r#"
inputs: []
turn_delivery:
  actionable-review:
    on: $cr.review.actionable-at-head == true
    to:
      vessel: implement
      role: coder
    brief: Address the actionable review, push the fix, and claim again.
    hold:
      kind: change-request-comment
      body: Automatic delivery paused; human attention is required.
vessels:
  - name: implement
    crew:
      - role: coder
        selector:
          capability: code
"#;

    let spec = parse_spec(yaml);
    validate(&spec).expect("valid turn-delivery rule");
    let serialized = serde_yml::to_string(&spec).expect("serialize rule");
    assert!(serialized.contains("actionable-review:"));
    assert!(serialized.contains("$cr.review.actionable-at-head == true"));
    assert!(!serialized.contains("round"));
}

#[test]
fn stall_nudge_policy_roundtrips_and_rejects_unknown_roles() {
    let yaml = r#"
inputs: []
stall_nudges:
  work/coder:
    max_per_episode: 0
vessels:
  - name: work
    crew:
      - role: coder
        selector:
          capability: code
"#;
    let spec = parse_spec(yaml);
    validate(&spec).expect("declared agent role");
    assert_eq!(spec.stall_nudges["work/coder"].max_per_episode, 0);
    assert!(serde_yml::to_string(&spec).expect("serialize").contains("max_per_episode: 0"));
    let mut invalid = spec;
    invalid.stall_nudges.insert("work/reviewer".to_string(), flotilla_resources::StallNudgePolicy { max_per_episode: 1 });
    assert!(validate(&invalid)
        .expect_err("unknown role")
        .iter()
        .any(|error| matches!(error, ValidationError::UnknownStallNudgeRole { target } if target == "work/reviewer")));
}

#[test]
fn turn_delivery_schema_rejects_control_flow_extensions() {
    let yaml = r#"
inputs: []
turn_delivery:
  actionable-review:
    sequence:
      - on: $cr.review.actionable-at-head == true
vessels:
  - name: implement
    crew:
      - role: coder
        selector:
          capability: code
"#;
    assert!(serde_yml::from_str::<WorkflowTemplateSpec>(yaml).is_err());
}

#[test]
fn validate_rejects_unsupported_turn_delivery_operator() {
    let spec = parse_spec(
        r#"
inputs: []
turn_delivery:
  actionable-review:
    on: $cr.review.actionable-at-head < true
    to:
      vessel: implement
      role: coder
    brief: Address the actionable review.
    hold:
      kind: change-request-comment
      body: Automatic delivery paused.
vessels:
  - name: implement
    crew:
      - role: coder
        selector:
          capability: code
"#,
    );

    let errors = validate(&spec).expect_err("ordering operators are not admitted for turn delivery");
    assert!(errors
        .iter()
        .any(|error| matches!(error, ValidationError::InvalidTurnDeliveryLeaf { source, .. } if source == "actionable-review")));
}

#[test]
fn issue_turn_delivery_admits_state_labels_and_updated_at() {
    for condition in ["$issue.state == closed", "$issue.labels.ready == true", "$issue.updated-at > 2026-09-27T00:00:00Z"] {
        let yaml = format!(
            r#"
inputs: []
turn_delivery:
  issue-change:
    on: {condition}
    to:
      vessel: implement
      role: coder
    brief: Respond to the issue.
    hold:
      kind: change-request-comment
      body: Automatic delivery paused.
vessels:
  - name: implement
    crew:
      - role: coder
        selector:
          capability: code
"#
        );
        validate(&parse_spec(&yaml)).expect("issue turn-delivery condition is admitted");
    }
    let bad: flotilla_protocol::Leaf = "issue/github.com/owner/repo/1 .labels.ready == yes".parse().expect("leaf syntax");
    assert!(admit_leaf(&bad).is_err(), "label membership must compare to true or false");
}

#[test]
fn turn_delivery_rejects_invalid_typed_literals_before_leaf_admission() {
    for (condition, expected_reason) in [
        ("$issue.updated-at > not-a-date", "invalid timestamp literal `not-a-date`"),
        ("$issue.labels.ready == yes", "issue label leaf literal must be `true` or `false`"),
    ] {
        let yaml = format!(
            r#"
turn_delivery:
  issue-change:
    on: {condition}
    to:
      vessel: implement
      role: coder
    brief: Respond to the issue.
    hold:
      kind: change-request-comment
      body: Automatic delivery paused.
vessels:
  - name: implement
    crew:
      - role: coder
        selector:
          capability: code
"#
        );
        let errors = validate(&parse_spec(&yaml)).expect_err("invalid typed literal should reject the template");
        assert!(
            errors.iter().any(|error| matches!(error, ValidationError::InvalidTurnDeliveryLiteral { source, template, reason }
            if source == "issue-change" && template == condition && reason.contains(expected_reason))),
            "unexpected errors: {errors:?}"
        );
    }
}

#[test]
fn issue_exit_is_rejected_and_issue_subjects_follow_relay_casing() {
    let spec = parse_spec(
        r#"
inputs: []
exit:
  closed: $issue.state == closed
vessels:
  - name: implement
    crew:
      - role: coder
        selector:
          capability: code
"#,
    );
    assert!(validate(&spec)
        .expect_err("issue exit is not admitted")
        .iter()
        .any(|error| matches!(error, ValidationError::InvalidExitLeaf { .. })));

    let github = IssueRef { source: IssueSource { service: "https://GitHub.com".into(), scope: "Owner/Repo".into() }, id: "12".into() };
    assert_eq!(issue_address(&github).expect("github address").to_string(), "issue/github.com/owner/repo/12");
    assert_eq!(issue_record_name("GitHub.com", "Owner/Repo", 12), issue_record_name("github.com", "owner/repo", 12));
    let forgejo =
        IssueRef { source: IssueSource { service: "https://forgejo.example/lab".into(), scope: "Team/Repo".into() }, id: "12".into() };
    assert_eq!(issue_address(&forgejo).expect("forgejo address").to_string(), "issue/forgejo.example%2flab/Team/Repo/12");
    assert_ne!(issue_record_name("forgejo.example", "Team/Repo", 12), issue_record_name("forgejo.example", "team/repo", 12));
}

#[test]
fn issue_subjects_distinguish_installations_on_one_host() {
    use std::collections::BTreeSet;

    use flotilla_resources::{ForgeKind, ForgeSpec};

    let source = |path: &str| IssueRef {
        source: IssueSource { service: format!("https://Forgejo.Example/{path}/"), scope: "Team/Repo".into() },
        id: "12".into(),
    };
    let lab = source("lab");
    let stage = source("stage");
    let fallback_lab = issue_address(&lab).expect("lab");
    let fallback_stage = issue_address(&stage).expect("stage");
    assert_eq!(fallback_lab.to_string(), "issue/forgejo.example%2flab/Team/Repo/12");
    assert_eq!(fallback_stage.to_string(), "issue/forgejo.example%2fstage/Team/Repo/12");
    assert_ne!(fallback_lab, fallback_stage);
    assert_ne!(issue_record_name("forgejo.example%2flab", "Team/Repo", 12), issue_record_name("forgejo.example%2fstage", "Team/Repo", 12));

    let forge = |id: &str, path: &str| {
        ForgeSpec::builder()
            .forge_id(id.into())
            .kind(ForgeKind::Forgejo)
            .hosts(BTreeSet::from(["forgejo.example".into(), format!("{id}.example")]))
            .https_url(format!("https://forgejo.example/{path}"))
            .git_ssh_host("forgejo.example".into())
            .build()
    };
    let forges = [forge("lab", "lab"), forge("stage", "stage")];
    let lab_address = issue_address_with_forges(&lab, &forges).expect("declared lab");
    let stage_address = issue_address_with_forges(&stage, &forges).expect("declared stage");
    assert_eq!(lab_address.to_string(), "issue/lab/Team/Repo/12");
    assert_eq!(stage_address.to_string(), "issue/stage/Team/Repo/12");
    let lab_alias =
        IssueRef { source: IssueSource { service: "https://lab.example/lab".into(), scope: "Team/Repo".into() }, id: "12".into() };
    assert_eq!(issue_address_with_forges(&lab_alias, &forges).expect("declared lab alias"), lab_address);
    let distinct_case = source("Lab");
    assert_eq!(
        issue_address_with_forges(&distinct_case, &forges).expect("distinct case").to_string(),
        "issue/forgejo.example%2flab/Team/Repo/12"
    );
    assert_ne!(lab_address, stage_address);
    assert_ne!(issue_record_name("lab", "Team/Repo", 12), issue_record_name("stage", "Team/Repo", 12));
}

#[test]
fn undeclared_issue_subjects_keep_non_https_schemes_distinct() {
    let reference = |scheme: &str| IssueRef {
        source: IssueSource { service: format!("{scheme}://tracker.example/root"), scope: "Team/Repo".into() },
        id: "12".into(),
    };
    assert_eq!(issue_address(&reference("https")).expect("https").to_string(), "issue/tracker.example%2froot/Team/Repo/12");
    assert_eq!(issue_address(&reference("http")).expect("http").to_string(), "issue/http%3a%2f%2ftracker.example%2froot/Team/Repo/12");
}

#[test]
fn undeclared_single_label_host_has_a_distinct_service_spelling() {
    let reference = IssueRef { source: IssueSource { service: "https://forgejo".into(), scope: "Team/Repo".into() }, id: "12".into() };
    assert_eq!(issue_address(&reference).expect("single-label host").to_string(), "issue/host%3aforgejo/Team/Repo/12");
}

#[test]
fn stock_workflows_transcribe_the_standard_exit_table() {
    let expected = Some(ExitDeclaration::standard_table());
    for spec in [
        single_agent_workflow_spec(),
        single_agent_shepherd_workflow_spec(),
        single_agent_workflow_spec(),
        interactive_single_workflow_spec(),
        implement_review_workflow_spec(),
    ] {
        assert_eq!(spec.exit, expected);
    }
}

#[test]
fn stock_landing_workflows_validate_with_review_and_conflicting_turn_delivery() {
    for (name, spec, role) in [
        ("single-agent-shepherd", single_agent_shepherd_workflow_spec(), "shepherd"),
        ("single-agent", single_agent_workflow_spec(), "coder"),
        ("implement-review", implement_review_workflow_spec(), "coder"),
    ] {
        validate(&spec).unwrap_or_else(|errors| panic!("stock workflow {name} must validate: {errors:?}"));
        assert_eq!(spec.turn_delivery.keys().map(String::as_str).collect::<Vec<_>>(), ["actionable-review", "conflicting"]);
        for rule in spec.turn_delivery.values() {
            assert_eq!(rule.to.vessel, "work", "wrong vessel in {name}");
            assert_eq!(rule.to.role, role, "wrong role in {name}");
        }
        assert_eq!(spec.turn_delivery["conflicting"].on.to_string(), "$cr.mergeable == conflicting");
    }
}

#[test]
fn fresh_convoy_workflow_snapshot_renders_both_standard_turn_delivery_rules() {
    let workflow = single_agent_workflow_spec();
    let snapshot = flotilla_resources::WorkflowSnapshot {
        stall_nudges: Default::default(),
        supervision: None,
        exit: workflow.exit,
        turn_delivery: workflow.turn_delivery,
        vessels: workflow.vessels,
    };

    let rendered = serde_json::to_value(snapshot).expect("render workflow snapshot");
    let rules = rendered["turn_delivery"].as_object().expect("turn-delivery object");
    assert!(rules.contains_key("actionable-review"));
    assert_eq!(rules["conflicting"]["on"], "$cr.mergeable == conflicting");
}

#[test]
fn exit_schema_rejects_branching_and_sequencing_constructs() {
    for exit in ["exit:\n  sequence:\n    - $cr.state == merged", "exit:\n  merged:\n    then: $cr.state == closed"] {
        let yaml =
            format!("inputs: []\n{exit}\nvessels:\n  - name: implement\n    crew:\n      - role: coder\n        command: cargo test\n");
        assert!(serde_yml::from_str::<WorkflowTemplateSpec>(&yaml).is_err(), "schema extension should be rejected: {yaml}");
    }
}

#[test]
fn validate_rejects_non_terminal_exit_leaf() {
    let spec = parse_spec(
        r#"
inputs: []
exit:
  still-working: $cr.state == open
vessels:
  - name: implement
    crew:
      - role: coder
        command: cargo test
"#,
    );

    let errors = validate(&spec).expect_err("open is not a world terminal");
    assert!(errors
        .iter()
        .any(|error| matches!(error, ValidationError::InvalidExitLeaf { disposition, .. } if disposition == "still-working")));
}

fn assert_has_error(errors: &[ValidationError], expected: &ValidationError) {
    assert!(errors.contains(expected), "missing expected error {expected:?} in {errors:?}");
}

#[test]
fn parse_rejects_process_without_selector_or_command() {
    let yaml = r#"
inputs: []
vessels:
  - name: implement
    crew:
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
vessels:
  - name: implement
    crew:
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
vessels:
  - name: implement
    crew:
      - role: coder
        command: cargo test
        prompt: should-not-be-here
"#;

    let error = serde_yml::from_str::<WorkflowTemplateSpec>(yaml).expect_err("parse should fail");
    let message = error.to_string();
    assert!(message.contains("data did not match any variant"), "unexpected error: {message}");
}

#[test]
fn validate_rejects_duplicate_vessel_names() {
    let mut spec = valid_workflow_template_spec();
    spec.vessels.push(spec.vessels[0].clone());

    let errors = validate(&spec).expect_err("validation should fail");
    assert_has_error(&errors, &ValidationError::DuplicateVesselName { name: "implement".to_string() });
}

#[test]
fn validate_rejects_address_marker_prefix_on_vessel_names() {
    let mut spec = valid_workflow_template_spec();
    spec.vessels[0].name = "@implement".to_string();

    let errors = validate(&spec).expect_err("validation should fail");
    assert!(errors.iter().any(|error| error.to_string().contains("vessel name `@implement`")), "unexpected errors: {errors:?}");
}

#[test]
fn validate_rejects_address_marker_prefix_on_crew_roles() {
    let mut spec = valid_workflow_template_spec();
    spec.vessels[0].crew[0].role = "@coder".to_string();

    let errors = validate(&spec).expect_err("validation should fail");
    assert!(errors.iter().any(|error| error.to_string().contains("crew role `@coder`")), "unexpected errors: {errors:?}");
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
    let duplicate_process = spec.vessels[0].crew[0].clone();
    spec.vessels[0].crew.push(duplicate_process);

    let errors = validate(&spec).expect_err("validation should fail");
    assert_has_error(&errors, &ValidationError::DuplicateRoleInVessel { vessel: "implement".to_string(), role: "coder".to_string() });
}

#[test]
fn validate_rejects_unknown_dependencies() {
    let mut spec = valid_workflow_template_spec();
    spec.vessels[1].depends_on = vec!["missing".to_string()];

    let errors = validate(&spec).expect_err("validation should fail");
    assert_has_error(&errors, &ValidationError::UnknownDependency { vessel: "review".to_string(), missing: "missing".to_string() });
}

#[test]
fn validate_rejects_cycles() {
    let mut spec = valid_workflow_template_spec();
    spec.vessels[0].depends_on = vec!["review".to_string()];

    let errors = validate(&spec).expect_err("validation should fail");
    assert_has_error(&errors, &ValidationError::DependencyCycle {
        cycle: vec!["implement".to_string(), "review".to_string(), "implement".to_string()],
    });
}

#[test]
fn validate_rejects_unknown_input_references() {
    let mut spec = valid_workflow_template_spec();
    if let Some(prompt) = match &mut spec.vessels[0].crew[0].source {
        flotilla_resources::CrewSource::Agent { prompt, .. } => prompt,
        _ => unreachable!("first process should be an agent"),
    } {
        *prompt = "Implement {{inputs.missing}}".to_string();
    }

    let errors = validate(&spec).expect_err("validation should fail");
    assert_has_error(&errors, &ValidationError::UnknownInputReference {
        location: InterpolationLocation { vessel: "implement".to_string(), role: "coder".to_string(), field: InterpolationField::Prompt },
        name: "missing".to_string(),
    });
}

#[test]
fn validate_rejects_unknown_workflow_fields() {
    let mut spec = valid_workflow_template_spec();
    if let Some(prompt) = match &mut spec.vessels[0].crew[0].source {
        flotilla_resources::CrewSource::Agent { prompt, .. } => prompt,
        _ => unreachable!("first process should be an agent"),
    } {
        *prompt = "Implement {{workflow.uid}}".to_string();
    }

    let errors = validate(&spec).expect_err("validation should fail");
    assert_has_error(&errors, &ValidationError::UnknownWorkflowField {
        location: InterpolationLocation { vessel: "implement".to_string(), role: "coder".to_string(), field: InterpolationField::Prompt },
        name: "uid".to_string(),
    });
}

#[test]
fn validate_rejects_malformed_owned_interpolations() {
    let mut spec = valid_workflow_template_spec();
    if let Some(prompt) = match &mut spec.vessels[0].crew[0].source {
        flotilla_resources::CrewSource::Agent { prompt, .. } => prompt,
        _ => unreachable!("first process should be an agent"),
    } {
        *prompt = "Implement {{inputs.branch }} then {{workflow.name.extra}}".to_string();
    }

    let errors = validate(&spec).expect_err("validation should fail");
    assert_has_error(&errors, &ValidationError::MalformedInterpolation {
        location: InterpolationLocation { vessel: "implement".to_string(), role: "coder".to_string(), field: InterpolationField::Prompt },
        text: "inputs.branch ".to_string(),
    });
    assert_has_error(&errors, &ValidationError::MalformedInterpolation {
        location: InterpolationLocation { vessel: "implement".to_string(), role: "coder".to_string(), field: InterpolationField::Prompt },
        text: "workflow.name.extra".to_string(),
    });
}

#[test]
fn validate_allows_foreign_interpolations() {
    let spec = parse_spec(
        r#"
inputs: []
vessels:
  - name: implement
    crew:
      - role: build
        command: "kubectl get pod -o go-template='{{.metadata.name}}'"
"#,
    );

    assert!(validate(&spec).is_ok(), "foreign interpolation should pass through");
}

#[test]
fn validate_rejects_reserved_process_label_keys() {
    let mut spec = valid_workflow_template_spec();
    spec.vessels[0].crew[0].labels.insert("flotilla.work/convoy".to_string(), "manual".to_string());

    let errors = validate(&spec).expect_err("reserved labels should fail validation");
    assert_has_error(&errors, &ValidationError::ReservedLabelKey {
        vessel: "implement".to_string(),
        role: "coder".to_string(),
        key: "flotilla.work/convoy".to_string(),
    });
}

#[test]
fn validate_allows_non_reserved_process_label_keys() {
    let spec = parse_spec(
        r#"
inputs: []
vessels:
  - name: implement
    crew:
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
fn parse_preserves_agent_brief_template_selection() {
    let spec = parse_spec(
        r#"
inputs: []
vessels:
  - name: interact
    crew:
      - role: driver
        selector:
          capability: code
        brief_template: interactive-session.md
"#,
    );

    let flotilla_resources::CrewSource::Agent { brief_template, .. } = &spec.vessels[0].crew[0].source else {
        panic!("driver should be agent-backed");
    };
    assert_eq!(brief_template.as_deref(), Some("interactive-session.md"));
    assert!(validate(&spec).is_ok(), "brief template selection should validate");
}

#[test]
fn parser_round_trip_preserves_sample_workflow() {
    let first: WorkflowTemplateDocument = serde_yml::from_str(valid_workflow_template_yaml()).expect("parse workflow template document");
    let encoded = serde_yml::to_string(&first.spec).expect("serialize workflow template spec");
    let second: WorkflowTemplateSpec = serde_yml::from_str(&encoded).expect("re-parse workflow template spec");

    assert_eq!(second, first.spec);
}

#[test]
fn parser_accepts_and_drops_pre_adr_0046_vessel_stance() {
    // Stored templates, frozen workflow snapshots, and the snapshot inside every
    // convoy status were written with a vessel `stance`; they must keep decoding
    // (ADR 0046 removed the field). It is dropped and never written back.
    let yaml = r#"
vessels:
  - name: trusted
    stance: trusted
    crew: []
  - name: workspace
    stance: workspace-write
    crew: []
  - name: contained
    stance: contained
    crew: []
"#;
    let spec = serde_yml::from_str::<WorkflowTemplateSpec>(yaml).expect("pre-ADR-0046 stance still decodes");
    assert_eq!(spec.vessels.iter().map(|vessel| vessel.name.as_str()).collect::<Vec<_>>(), ["trusted", "workspace", "contained"]);
    assert!(!serde_yml::to_string(&spec).expect("serialize spec").contains("stance"), "stance must not be written back");
}

#[test]
fn repository_scope_must_be_non_empty_and_unique() {
    let spec = parse_spec(
        r#"
vessels:
  - name: empty
    repository_refs: []
    crew: []
  - name: duplicate
    repository_refs: [repo-a, repo-a]
    crew: []
"#,
    );

    let errors = validate(&spec).expect_err("invalid repository scopes should fail validation");
    assert_has_error(&errors, &ValidationError::EmptyRepositoryScope { vessel: "empty".to_string() });
    assert_has_error(&errors, &ValidationError::DuplicateRepositoryRef {
        vessel: "duplicate".to_string(),
        repo_ref: RepositoryKey("repo-a".to_string()),
    });
}
