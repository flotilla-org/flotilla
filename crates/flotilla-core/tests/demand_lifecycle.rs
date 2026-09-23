use std::sync::Arc;

use chrono::{Duration, TimeZone, Utc};
use flotilla_core::demand_lifecycle::DemandLifecycle;
use flotilla_protocol::{PrincipalRef, ResourceRef};
use flotilla_resources::{
    Demand, DemandAddressee, DemandExpiry, DemandExpiryDisposition, DemandKind, DemandSpec, DemandState, InputMeta, ResourceBackend,
    VirtualClock,
};

fn timestamp(second: u32) -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 8, 21, 12, 0, second).single().expect("valid timestamp")
}

#[tokio::test]
async fn expired_demand_escalates_without_settling() {
    let backend = ResourceBackend::InMemory(Default::default());
    let clock = Arc::new(VirtualClock::new(timestamp(10)));
    let lifecycle = DemandLifecycle::new(backend.clone(), clock.clone());
    let demands = backend.using::<Demand>("flotilla");
    demands
        .create(
            &InputMeta::builder().name("never-claimed".to_string()).build(),
            &DemandSpec::builder()
                .originating_work_ref(ResourceRef::new("flotilla.work/v1", "Convoy", "flotilla", "never-claimed"))
                .kind(DemandKind::HumanGate)
                .addressee(DemandAddressee::Principal { principal_ref: PrincipalRef::implicit_for_namespace("flotilla") })
                .expiry(DemandExpiry::builder().deadline(timestamp(20)).disposition(DemandExpiryDisposition::Escalate).build())
                .build(),
        )
        .await
        .expect("create expiring demand");

    lifecycle.expire_due("flotilla").await.expect("early sweep");
    assert!(demands.get("never-claimed").await.expect("demand").status.is_none());

    clock.advance(Duration::seconds(10));
    lifecycle.expire_due("flotilla").await.expect("expiry sweep");

    let status = demands.get("never-claimed").await.expect("demand").status.expect("status");
    assert_eq!(status.state, DemandState::Escalated);
    assert_eq!(status.escalated.expect("escalation").as_of, timestamp(20));
    assert!(status.satisfied.is_none());
    assert!(status.verdict.is_none());
}
