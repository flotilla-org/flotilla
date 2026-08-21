mod common;

use common::timestamp;
use flotilla_protocol::ResourceRef;
use flotilla_resources::{
    resolve_demand, DemandAddressee, DemandKind, DemandPoolRef, DemandResponseOption, DemandSpec, DemandState, DemandStatus,
    DemandStatusPatch, DemandVerdict, DemandVerdictDisposition, PrincipalRef, RegardStatus, RegardStatusPatch, ResourceBackend,
    StatusPatch,
};

#[test]
fn regard_refresh_records_created_and_latest_refreshed_time() {
    let mut status = RegardStatus::default();

    RegardStatusPatch::Refresh { as_of: timestamp(10) }.apply(&mut status);
    RegardStatusPatch::Refresh { as_of: timestamp(8) }.apply(&mut status);
    RegardStatusPatch::Refresh { as_of: timestamp(20) }.apply(&mut status);

    assert_eq!(status.created_at, Some(timestamp(10)));
    assert_eq!(status.refreshed_at, Some(timestamp(20)));
}

#[test]
fn demand_raise_stamps_lifecycle_authority_once() {
    let mut status = DemandStatus::default();

    DemandStatusPatch::Raise { as_of: timestamp(10), authority: "dispatch/principal-default".to_string() }.apply(&mut status);
    DemandStatusPatch::Raise { as_of: timestamp(20), authority: "other".to_string() }.apply(&mut status);

    assert_eq!(status.state, DemandState::Raised);
    let raised = status.raised.expect("raised transition");
    assert_eq!(raised.as_of, timestamp(10));
    assert_eq!(raised.authority, "dispatch/principal-default");
    assert!(status.satisfied.is_none());
    assert!(status.acknowledged.is_none());
}

#[test]
fn demand_acknowledge_preserves_transition_timestamps_and_authorities() {
    let mut status = DemandStatus::default();

    DemandStatusPatch::Raise { as_of: timestamp(10), authority: "dispatch/principal-default".to_string() }.apply(&mut status);
    DemandStatusPatch::Acknowledge { as_of: timestamp(30), authority: "principal/default".to_string() }.apply(&mut status);
    DemandStatusPatch::Acknowledge { as_of: timestamp(40), authority: "late-controller".to_string() }.apply(&mut status);

    assert_eq!(status.state, DemandState::Acknowledged);
    let raised = status.raised.expect("raised transition");
    let acknowledged = status.acknowledged.expect("acknowledged transition");
    assert_eq!(raised.as_of, timestamp(10));
    assert_eq!(raised.authority, "dispatch/principal-default");
    assert_eq!(acknowledged.as_of, timestamp(30));
    assert_eq!(acknowledged.authority, "principal/default");
}

#[test]
fn demand_spec_defaults_to_dispatching_principal_via_constructor() {
    let work_ref = ResourceRef::new("flotilla.work/v1", "Vessel", "flotilla", "demo-implement");
    let principal_ref = PrincipalRef::implicit_for_namespace("flotilla");
    let spec = DemandSpec::for_dispatching_principal(work_ref.clone(), DemandKind::Permission, principal_ref.clone());

    assert_eq!(spec.originating_work_ref, work_ref);
    assert_eq!(spec.addressee, DemandAddressee::Principal { principal_ref });
}

#[test]
fn demand_spec_can_route_unroutable_work_to_a_pool() {
    let work_ref = ResourceRef::new("flotilla.work/v1", "Vessel", "flotilla", "demo-review");
    let spec = DemandSpec::for_pool(work_ref, DemandKind::Review, DemandPoolRef("project/default".to_string()));

    assert_eq!(spec.addressee, DemandAddressee::Pool { pool_ref: DemandPoolRef("project/default".to_string()) });
}

#[tokio::test]
async fn demand_resolution_records_a_typed_offered_verdict() {
    let backend = ResourceBackend::InMemory(Default::default());
    let demands = backend.using::<flotilla_resources::Demand>("flotilla");
    demands
        .create(
            &flotilla_resources::InputMeta::builder().name("review-demo".to_string()).build(),
            &DemandSpec::builder()
                .originating_work_ref(ResourceRef::new("flotilla.work/v1", "Vessel", "flotilla", "demo-review"))
                .kind(DemandKind::Review)
                .addressee(DemandAddressee::Principal { principal_ref: PrincipalRef::implicit_for_namespace("flotilla") })
                .response_options(vec![
                    DemandResponseOption::builder().name("approve".to_string()).title("Approve".to_string()).build(),
                    DemandResponseOption::builder().name("revise".to_string()).title("Request revisions".to_string()).build(),
                ])
                .build(),
        )
        .await
        .expect("create demand");
    let verdict = DemandVerdict::builder()
        .responding_principal_ref(PrincipalRef::implicit_for_namespace("flotilla"))
        .disposition(DemandVerdictDisposition::Selected { option: "revise".to_string() })
        .comment("Please cover the retry path".to_string())
        .build();

    let resolved = resolve_demand(&demands, "review-demo", verdict.clone(), timestamp(20), "principal/default".to_string())
        .await
        .expect("resolve with offered option");

    let status = resolved.status.expect("demand status");
    assert_eq!(status.state, DemandState::Satisfied);
    assert_eq!(status.verdict, Some(verdict));
}

#[tokio::test]
async fn demand_resolution_rejects_an_unoffered_verdict_but_accepts_explicit_other() {
    let backend = ResourceBackend::InMemory(Default::default());
    let demands = backend.using::<flotilla_resources::Demand>("flotilla");
    demands
        .create(
            &flotilla_resources::InputMeta::builder().name("review-demo".to_string()).build(),
            &DemandSpec::builder()
                .originating_work_ref(ResourceRef::new("flotilla.work/v1", "Vessel", "flotilla", "demo-review"))
                .kind(DemandKind::Review)
                .addressee(DemandAddressee::Principal { principal_ref: PrincipalRef::implicit_for_namespace("flotilla") })
                .response_options(vec![DemandResponseOption::builder().name("approve".to_string()).title("Approve".to_string()).build()])
                .build(),
        )
        .await
        .expect("create demand");
    let unoffered = DemandVerdict::builder()
        .responding_principal_ref(PrincipalRef::implicit_for_namespace("flotilla"))
        .disposition(DemandVerdictDisposition::Selected { option: "abandon".to_string() })
        .build();

    let error = resolve_demand(&demands, "review-demo", unoffered, timestamp(20), "principal/default".to_string())
        .await
        .expect_err("unoffered option must be rejected");
    assert!(error.to_string().contains("not offered"));

    let other = DemandVerdict::builder()
        .responding_principal_ref(PrincipalRef::implicit_for_namespace("flotilla"))
        .disposition(DemandVerdictDisposition::Other)
        .comment("Use the manual recovery path".to_string())
        .build();
    let resolved = resolve_demand(&demands, "review-demo", other.clone(), timestamp(30), "principal/default".to_string())
        .await
        .expect("explicit other is valid");
    assert_eq!(resolved.status.expect("status").verdict, Some(other));
}
