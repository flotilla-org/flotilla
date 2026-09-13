use std::{fs, path::Path, process::Command};

use flotilla_resources::{validate_settlement_claim, FindingResolution, SettlementClaimEvidence, REVIEW_BUNDLE_INDEX_FILE};

fn run(command: &mut Command) -> String {
    let output = command.output().expect("run command");
    assert!(output.status.success(), "command failed: {}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8(output.stdout).expect("command output is UTF-8").trim().to_string()
}

fn write_json(path: &Path, value: serde_json::Value) {
    fs::create_dir_all(path.parent().expect("fixture file has a parent")).expect("create fixture directory");
    fs::write(path, serde_json::to_vec_pretty(&value).expect("serialize fixture")).expect("write fixture");
}

#[test]
fn assembled_rounds_pass_settlement_claim_admissibility() {
    let project = tempfile::tempdir().expect("create project");
    let project_path = project.path().to_str().expect("UTF-8 project path");
    run(Command::new("git").args(["-C", project_path, "init", "-q"]));
    run(Command::new("git").args(["-C", project_path, "config", "user.email", "test@example.test"]));
    run(Command::new("git").args(["-C", project_path, "config", "user.name", "Test"]));
    fs::write(project.path().join("message.txt"), "base\n").expect("write base");
    run(Command::new("git").args(["-C", project_path, "add", "message.txt"]));
    run(Command::new("git").args(["-C", project_path, "commit", "-qm", "base"]));
    run(Command::new("git").args(["-C", project_path, "branch", "base"]));
    fs::write(project.path().join("message.txt"), "base\nreviewed change\n").expect("write change");
    run(Command::new("git").args(["-C", project_path, "commit", "-qam", "change"]));

    let review = project.path().join(".flotilla/review");
    let round = review.join("rounds/0001");
    write_json(&review.join("review.json"), serde_json::json!({"base": "base", "head": "HEAD"}));
    write_json(&round.join("findings.json"), serde_json::json!([{"id": "R1-F1", "summary": "Preserve the reviewed change"}]));
    write_json(
        &round.join("responses.json"),
        serde_json::json!([{"finding_id": "R1-F1", "state": "addressed", "fix_reference": "commit:HEAD"}]),
    );
    write_json(&round.join("checks.json"), serde_json::json!([{"name": "cargo test --workspace --locked", "outcome": "passed"}]));

    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../skills/crew-review/scripts/assemble_review_bundle.py");
    let mut aggregator = if Command::new("uv").arg("--version").output().is_ok() {
        let mut command = Command::new("uv");
        command.args(["run", "--no-project"]);
        command
    } else {
        Command::new("python3")
    };
    let output = aggregator
        .arg(script)
        .args(["--rounds", ".flotilla/review", "--output", ".flotilla/review-bundle", "--project-root", "."])
        .current_dir(project.path())
        .output()
        .expect("run bundle aggregator");
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
}
