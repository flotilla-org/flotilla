use std::collections::BTreeMap;

use chrono::Utc;
use flotilla_resources::{
    CheckoutPhase, ConvoyPhase, ConvoyStatus, ConvoyStatusPatch, CrewWorkPhase, CrewWorkState, EnvironmentPhase, EnvironmentStatus,
    EnvironmentStatusPatch, StatusPatch, TurnDeliveryEpisode, TurnDeliveryOutcome, TurnDeliveryRung, VesselPhase, VesselStatus,
    VesselStatusPatch, WorkPhase, WorkState,
};
use hegel::generators as gs;

// ADR 0047: previous-generation phase names decode into live phases, but writes
// use only the new spelling. Every retired phase is covered.
#[test]
fn retired_phases_decode_but_are_never_written() {
    fn decodes<T: serde::de::DeserializeOwned + serde::Serialize + PartialEq + std::fmt::Debug>(old: &str, new: T, spelling: &str) {
        let value: T = serde_json::from_value(serde_json::json!(old)).expect("previous generation decodes");
        assert_eq!(value, new);
        assert_eq!(serde_json::to_value(value).expect("serialize"), serde_json::json!(spelling));
    }
    decodes("Anchored", ConvoyPhase::Landing, "Landing");
    decodes("Cancelled", ConvoyPhase::Abandoned, "Abandoned");
    decodes("TearingDown", VesselPhase::Interrupted, "Interrupted");
    decodes("Terminating", EnvironmentPhase::Lost, "Lost");
    decodes("Preparing", CheckoutPhase::Pending, "Pending");
    decodes("Terminating", CheckoutPhase::Gone, "Gone");
}

// A lost environment retains its identity and evidence, becomes unavailable,
// and is distinct from a failed provisioning attempt. Duplicate loss is harmless.
#[hegel::test]
fn loss_preserves_recorded_identity(tc: hegel::TestCase) {
    let reason = if tc.draw(gs::booleans()) { "host reboot" } else { "container gone" };
    let mut environment = EnvironmentStatus {
        phase: EnvironmentPhase::Ready,
        ready: true,
        docker_container_id: Some("retained-container".into()),
        ..Default::default()
    };
    let mut vessel = VesselStatus { phase: VesselPhase::Ready, environment_ref: Some("retained-environment".into()), ..Default::default() };
    for _ in 0..2 {
        EnvironmentStatusPatch::MarkLost { message: reason.into() }.apply(&mut environment);
        VesselStatusPatch::MarkLost { message: reason.into() }.apply(&mut vessel);
        assert_eq!(environment.phase, EnvironmentPhase::Lost);
        assert!(!environment.ready);
        assert_eq!(environment.docker_container_id.as_deref(), Some("retained-container"));
        assert_eq!(vessel.phase, VesselPhase::Lost);
        assert_eq!(vessel.environment_ref.as_deref(), Some("retained-environment"));
        assert_eq!(environment.message.as_deref(), Some(reason));
        assert_eq!(vessel.message.as_deref(), Some(reason));
    }
}

// Resume and turn delivery cannot silently reopen any terminal convoy, even
// when stale controller patches or queued follow-ups arrive after settlement.
// Generate all three terminal phases, both admission rungs, and repeated patches.
#[hegel::test]
fn terminal_convoys_ignore_silent_continuations(tc: hegel::TestCase) {
    let phase =
        [ConvoyPhase::Landed, ConvoyPhase::Failed, ConvoyPhase::Abandoned][tc.draw(gs::integers::<usize>().min_value(0).max_value(2))];
    let at = Utc::now();
    let mut status = ConvoyStatus {
        phase,
        finished_at: Some(at),
        work: BTreeMap::from([("work".into(), WorkState::builder().phase(WorkPhase::Complete).build())]),
        crew_work: BTreeMap::from([(
            "work".into(),
            BTreeMap::from([("coder".into(), CrewWorkState::builder().phase(CrewWorkPhase::Done).build())]),
        )]),
        ..Default::default()
    };
    let original = status.clone();
    let rung = if tc.draw(gs::booleans()) { TurnDeliveryRung::WarmSession } else { TurnDeliveryRung::FreshAgent };
    for _ in 0..tc.draw(gs::integers::<usize>().min_value(1).max_value(5)) {
        ConvoyStatusPatch::ResumeCrewWork {
            vessel: "work".into(),
            role: "coder".into(),
            resumed_at: at,
            prompt: "resume".into(),
            brief_id: None,
        }
        .apply(&mut status);
        assert_eq!(status, original);
        ConvoyStatusPatch::RecordTurnDelivery {
            source: "review".into(),
            episode: TurnDeliveryEpisode::builder()
                .subject_revision("head".into())
                .evidence_at(at)
                .judged_claim_at(at)
                .outcome(TurnDeliveryOutcome::Delivered { rung, delivered_at: at })
                .build(),
            vessel: "work".into(),
            role: "coder".into(),
            prompt: "review".into(),
        }
        .apply(&mut status);
        assert_eq!(status, original);
        ConvoyStatusPatch::RollUpPhase { phase: ConvoyPhase::Active, started_at: Some(at), finished_at: None }.apply(&mut status);
        assert_eq!(status, original);
    }
}
