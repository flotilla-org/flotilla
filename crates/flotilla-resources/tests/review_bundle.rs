use std::fs;

use flotilla_resources::{
    validate_settlement_claim, ClaimAdmissibilityError, CrewWorkPhase, CrewWorkState, FindingResolution, ReviewBundleIndex, ReviewCheck,
    ReviewCheckOutcome, ReviewFinding, ReviewRefPair, ReviewRound, SettlementClaimEvidence, REVIEW_BUNDLE_INDEX_FILE,
};

fn refs() -> ReviewRefPair {
    ReviewRefPair::builder().base("refs/heads/main".to_string()).head("refs/heads/topic".to_string()).build()
}

fn claim(digest: &str) -> SettlementClaimEvidence {
    SettlementClaimEvidence::builder()
        .refs(refs())
        .bundle_url("https://objects.example/reviews/project/convoy/1/".to_string())
        .claimed_head_digest(digest.to_string())
        .build()
}

fn index(resolution: FindingResolution, digest: &str) -> ReviewBundleIndex {
    ReviewBundleIndex::builder()
        .refs(refs())
        .head_digest(digest.to_string())
        .rounds(vec![ReviewRound::builder()
            .number(1)
            .findings(vec![ReviewFinding::builder()
                .id("finding-1".to_string())
                .summary("Handle the edge case".to_string())
                .resolution(resolution)
                .build()])
            .build()])
        .checks(vec![ReviewCheck::builder()
            .name("cargo test --workspace --locked".to_string())
            .outcome(ReviewCheckOutcome::Passed)
            .build()])
        .artifacts(vec!["review.html".to_string(), "diff-summary.md".to_string()])
        .build()
}

fn write_bundle(index: &ReviewBundleIndex) -> tempfile::TempDir {
    let directory = tempfile::tempdir().expect("create bundle directory");
    fs::write(directory.path().join(REVIEW_BUNDLE_INDEX_FILE), serde_json::to_vec_pretty(index).expect("serialize bundle index"))
        .expect("write bundle index");
    directory
}

#[test]
fn accepts_terminal_findings_at_the_claimed_digest() {
    let bundle = write_bundle(&index(FindingResolution::Addressed { fix_reference: "commit:abc123".to_string() }, "sha256:reviewed-head"));

    let admitted = validate_settlement_claim(&claim("sha256:reviewed-head"), bundle.path()).expect("claim should be admissible");

    assert_eq!(admitted.artifacts, ["review.html", "diff-summary.md"]);
}

#[test]
fn rejects_an_unanswered_finding() {
    let bundle = write_bundle(&index(FindingResolution::Open, "sha256:reviewed-head"));

    let error = validate_settlement_claim(&claim("sha256:reviewed-head"), bundle.path()).expect_err("open finding must be refused");

    assert!(matches!(error, ClaimAdmissibilityError::UnansweredFinding { round: 1, ref finding_id } if finding_id == "finding-1"));
}

#[test]
fn rejects_a_claimed_head_digest_that_differs_from_the_bundle() {
    let bundle = write_bundle(&index(
        FindingResolution::RejectedWithRationale { rationale: "The reported behavior is required".to_string() },
        "sha256:reviewed-head",
    ));

    let error = validate_settlement_claim(&claim("sha256:moved-head"), bundle.path()).expect_err("stale claim must be refused");

    assert!(matches!(
        error,
        ClaimAdmissibilityError::HeadDigestMismatch { ref claimed, ref bundled }
            if claimed == "sha256:moved-head" && bundled == "sha256:reviewed-head"
    ));
}

#[test]
fn finding_resolution_has_only_the_protocol_states() {
    let states = [
        serde_json::to_value(FindingResolution::Open).expect("serialize open state"),
        serde_json::to_value(FindingResolution::Addressed { fix_reference: "commit:abc123".to_string() })
            .expect("serialize addressed state"),
        serde_json::to_value(FindingResolution::RejectedWithRationale { rationale: "not applicable".to_string() })
            .expect("serialize rejected state"),
    ];

    assert_eq!(states[0]["state"], "open");
    assert_eq!(states[1]["state"], "addressed");
    assert_eq!(states[2]["state"], "rejected-with-rationale");
    assert!(serde_json::from_value::<FindingResolution>(serde_json::json!({ "state": "dismissed" })).is_err());
}

#[test]
fn settlement_claim_carries_the_complete_evidence_reference() {
    let state = CrewWorkState::builder().phase(CrewWorkPhase::Done).claim_evidence(claim("sha256:reviewed-head")).build();

    let serialized = serde_json::to_value(&state).expect("serialize settlement claim");

    assert_eq!(serialized["claim_evidence"]["refs"]["base"], "refs/heads/main");
    assert_eq!(serialized["claim_evidence"]["refs"]["head"], "refs/heads/topic");
    assert_eq!(serialized["claim_evidence"]["bundle_url"], "https://objects.example/reviews/project/convoy/1/");
    assert_eq!(serialized["claim_evidence"]["claimed_head_digest"], "sha256:reviewed-head");
}
