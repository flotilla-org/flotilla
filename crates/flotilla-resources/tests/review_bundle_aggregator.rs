use std::{fs, path::Path, process::Command};

use flotilla_resources::{validate_settlement_claim, FindingResolution, SettlementClaimEvidence, REVIEW_BUNDLE_INDEX_FILE};

fn run(command: &mut Command) -> String {
    let output = command.output().expect("run command");
    assert!(output.status.success(), "command failed: {}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8(output.stdout).expect("command output is UTF-8").trim().to_string()
}

fn write_json(path: &Path, value: serde_json::Value) {
    fs::write(path, serde_json::to_vec_pretty(&value).expect("serialize fixture")).expect("write fixture");
}

fn project_with_review(response: Option<serde_json::Value>, instructed: bool) -> tempfile::TempDir {
    let project = tempfile::tempdir().expect("create project");
    run(Command::new("git").args(["-C", project.path().to_str().expect("UTF-8 path"), "init", "-q"]));
    run(Command::new("git").args(["-C", project.path().to_str().expect("UTF-8 path"), "config", "user.email", "test@example.test"]));
    run(Command::new("git").args(["-C", project.path().to_str().expect("UTF-8 path"), "config", "user.name", "Test"]));
    fs::write(project.path().join("message.txt"), "base\n").expect("write base");
    run(Command::new("git").args(["-C", project.path().to_str().expect("UTF-8 path"), "add", "message.txt"]));
    run(Command::new("git").args(["-C", project.path().to_str().expect("UTF-8 path"), "commit", "-qm", "base"]));
    run(Command::new("git").args(["-C", project.path().to_str().expect("UTF-8 path"), "branch", "base"]));
    fs::write(project.path().join("message.txt"), "base\nreviewed change\n").expect("write change");
    run(Command::new("git").args(["-C", project.path().to_str().expect("UTF-8 path"), "commit", "-qam", "change"]));

    if instructed {
        fs::create_dir(project.path().join("review-assets")).expect("create asset directory");
        fs::write(project.path().join("review-assets/explainer.html"), "<p>Architecture explainer</p>").expect("write artifact");
        fs::write(
            project.path().join("CLAUDE.md"),
            "```flotilla-review-prep\n{\"required_artifacts\":[\"review-assets/explainer.html\"]}\n```\n",
        )
        .expect("write instructions");
    }

    let review = project.path().join(".flotilla/review");
    let round = review.join("rounds/0001");
    fs::create_dir_all(&round).expect("create round");
    write_json(&review.join("review.json"), serde_json::json!({"base": "base", "head": "HEAD"}));
    write_json(&round.join("findings.json"), serde_json::json!([{"id": "R1-F1", "summary": "Preserve the reviewed change"}]));
    write_json(&round.join("responses.json"), serde_json::Value::Array(response.into_iter().collect()));
    write_json(&round.join("checks.json"), serde_json::json!([{"name": "cargo test --workspace --locked", "outcome": "passed"}]));
    project
}

fn aggregator(project: &Path) -> std::process::Output {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../skills/crew-review/scripts/assemble_review_bundle.py");
    let mut command = if Command::new("uv").arg("--version").output().is_ok() {
        let mut command = Command::new("uv");
        command.args(["run", "--no-project"]);
        command
    } else {
        Command::new("python3")
    };
    command
        .arg(script)
        .args(["--rounds", ".flotilla/review", "--output", ".flotilla/review-bundle", "--project-root", "."])
        .current_dir(project)
        .output()
        .expect("run bundle aggregator")
}

#[test]
fn assembled_real_rounds_pass_claim_admissibility_and_render_human_record() {
    let project =
        project_with_review(Some(serde_json::json!({"finding_id": "R1-F1", "state": "addressed", "fix_reference": "commit:HEAD"})), false);
    let output = aggregator(project.path());
    assert!(output.status.success(), "aggregation failed: {}", String::from_utf8_lossy(&output.stderr));

    let bundle = project.path().join(".flotilla/review-bundle");
    let index: flotilla_resources::ReviewBundleIndex =
        serde_json::from_slice(&fs::read(bundle.join(REVIEW_BUNDLE_INDEX_FILE)).expect("read index")).expect("index matches schema");
    let claim = SettlementClaimEvidence::builder()
        .refs(index.refs.clone())
        .bundle_url("file://local-review-bundle".to_string())
        .claimed_head_digest(index.head_digest.clone())
        .build();
    let admitted = validate_settlement_claim(&claim, &bundle).expect("aggregated bundle is admissible");

    assert!(matches!(admitted.rounds[0].findings[0].resolution, FindingResolution::Addressed { .. }));
    let page = fs::read_to_string(bundle.join("review.html")).expect("read human page");
    assert!(page.contains("message.txt"), "page contains diff summary");
    assert!(page.contains("Preserve the reviewed change"), "page contains finding");
    assert!(page.contains("commit:HEAD"), "page contains response");
}

#[test]
fn refuses_to_emit_a_bundle_when_a_finding_is_unanswered() {
    let project = project_with_review(None, false);
    let output = aggregator(project.path());

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("finding R1-F1 is unanswered"));
    assert!(!project.path().join(".flotilla/review-bundle").exists());
}

#[test]
fn project_review_prep_instructions_change_bundle_contents() {
    let response =
        || serde_json::json!({"finding_id": "R1-F1", "state": "rejected-with-rationale", "rationale": "The behavior is intentional"});
    let uninstructed = project_with_review(Some(response()), false);
    let instructed = project_with_review(Some(response()), true);
    assert!(aggregator(uninstructed.path()).status.success());
    assert!(aggregator(instructed.path()).status.success());

    let read_artifacts = |project: &Path| {
        let value: serde_json::Value =
            serde_json::from_slice(&fs::read(project.join(".flotilla/review-bundle/index.json")).expect("read bundle index"))
                .expect("decode bundle index");
        value["artifacts"].as_array().expect("artifact array").clone()
    };
    assert_eq!(read_artifacts(uninstructed.path()), ["review.html"]);
    assert_eq!(read_artifacts(instructed.path()), ["review.html", "project-artifacts/review-assets/explainer.html"]);
    assert!(instructed.path().join(".flotilla/review-bundle/project-artifacts/review-assets/explainer.html").is_file());
}

#[test]
fn project_review_prep_refuses_artifacts_outside_the_project() {
    let outside = tempfile::NamedTempFile::new().expect("create outside artifact");
    let declarations = [
        outside.path().display().to_string(),
        format!("../{}", outside.path().file_name().expect("outside artifact has a name").to_string_lossy()),
    ];
    for declaration in declarations {
        let project = project_with_review(
            Some(serde_json::json!({
                "finding_id": "R1-F1",
                "state": "addressed",
                "fix_reference": "commit:HEAD"
            })),
            false,
        );
        fs::write(
            project.path().join("CLAUDE.md"),
            format!("```flotilla-review-prep\n{{\"required_artifacts\":[{declaration:?}]}}\n```\n"),
        )
        .expect("write escaping instructions");

        let output = aggregator(project.path());
        assert!(!output.status.success(), "outside artifact was accepted: {declaration}");
        assert!(String::from_utf8_lossy(&output.stderr).contains("required artifact escapes project root"));
        assert!(!project.path().join(".flotilla/review-bundle").exists());
    }
}
