use std::collections::BTreeMap;

use flotilla_protocol::{PrincipalRef, ResourceRef};
use flotilla_resources::{
    evaluate_landing_gate, settlement_human_gate, DemandState, DemandStatus, DemandVerdict, DemandVerdictDisposition,
    LandingCredentialScope, LandingGateDecision, RepositoryKey, ReviewRefPair, SettlementClaimEvidence, StatusPatch, VesselStatus,
    VesselStatusPatch, LANDING_APPROVE_OPTION,
};

fn claim() -> SettlementClaimEvidence {
    SettlementClaimEvidence::builder()
        .refs(ReviewRefPair::builder().base("refs/heads/main".to_string()).head("refs/heads/topic".to_string()).build())
        .bundle_url("https://objects.example/reviews/project/convoy/1/".to_string())
        .claimed_head_digest("sha256:reviewed-head".to_string())
        .build()
}

fn approved() -> DemandStatus {
    DemandStatus::builder()
        .state(DemandState::Satisfied)
        .verdict(
            DemandVerdict::builder()
                .responding_principal_ref(PrincipalRef::implicit_for_namespace("flotilla"))
                .disposition(DemandVerdictDisposition::Selected { option: LANDING_APPROVE_OPTION.to_string() })
                .build(),
        )
        .build()
}

#[test]
fn claim_demand_carries_bundle_ref_pair_and_digest() {
    let spec = settlement_human_gate(
        ResourceRef::new("flotilla.dev/v1alpha1", "Convoy", "flotilla", "convoy-a"),
        PrincipalRef::implicit_for_namespace("flotilla"),
        claim(),
    );
    let carried = &spec.human_gate.expect("human gate payload").claim;
    assert_eq!(carried.bundle_url, "https://objects.example/reviews/project/convoy/1/");
    assert_eq!(carried.refs.head, "refs/heads/topic");
    assert_eq!(carried.claimed_head_digest, "sha256:reviewed-head");
}

#[test]
fn landing_credentials_are_absent_before_approval_and_both_scopings_stage_after() {
    let spec = settlement_human_gate(
        ResourceRef::new("flotilla.dev/v1alpha1", "Convoy", "flotilla", "convoy-a"),
        PrincipalRef::implicit_for_namespace("flotilla"),
        claim(),
    );
    let credentials = BTreeMap::from([
        ("branch-push".to_string(), LandingCredentialScope::Branch {
            repository: RepositoryKey("github.com-org-repo".to_string()),
            branch: "topic".to_string(),
        }),
        ("temporal-push".to_string(), LandingCredentialScope::TemporalOnly),
    ]);
    let mut vessel = VesselStatus::default();
    assert!(vessel.held_credentials.is_empty());
    assert_eq!(evaluate_landing_gate(&spec, None, "sha256:reviewed-head", &credentials), LandingGateDecision::Pending);
    let LandingGateDecision::Stage { credentials } = evaluate_landing_gate(&spec, Some(&approved()), "sha256:reviewed-head", &credentials)
    else {
        panic!("approved current claim should stage");
    };
    VesselStatusPatch::StageLandingCredentials { credentials: credentials.clone() }.apply(&mut vessel);
    assert_eq!(vessel.held_credentials, credentials);
}

#[test]
fn stale_approval_never_stages_and_refusal_preserves_the_reason() {
    let spec = settlement_human_gate(
        ResourceRef::new("flotilla.dev/v1alpha1", "Convoy", "flotilla", "convoy-a"),
        PrincipalRef::implicit_for_namespace("flotilla"),
        claim(),
    );
    assert!(matches!(
        evaluate_landing_gate(&spec, Some(&approved()), "sha256:moved-head", &BTreeMap::new()),
        LandingGateDecision::Stale { .. }
    ));
    let refused = DemandStatus::builder()
        .state(DemandState::Satisfied)
        .verdict(
            DemandVerdict::builder()
                .responding_principal_ref(PrincipalRef::implicit_for_namespace("flotilla"))
                .disposition(DemandVerdictDisposition::Selected { option: "refuse".to_string() })
                .comment("review evidence is incomplete".to_string())
                .build(),
        )
        .build();
    assert_eq!(evaluate_landing_gate(&spec, Some(&refused), "sha256:reviewed-head", &BTreeMap::new()), LandingGateDecision::Refused {
        reason: "review evidence is incomplete".to_string()
    });
}

#[test]
fn acknowledged_gate_is_terminal_and_does_not_wait_forever() {
    let spec = settlement_human_gate(
        ResourceRef::new("flotilla.dev/v1alpha1", "Convoy", "flotilla", "convoy-a"),
        PrincipalRef::implicit_for_namespace("flotilla"),
        claim(),
    );
    let acknowledged = DemandStatus::builder().state(DemandState::Acknowledged).build();
    assert_eq!(evaluate_landing_gate(&spec, Some(&acknowledged), "sha256:reviewed-head", &BTreeMap::new()), LandingGateDecision::Refused {
        reason: "landing approval was dismissed".to_string()
    });
}
