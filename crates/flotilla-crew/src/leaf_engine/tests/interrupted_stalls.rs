use super::*;
use crate::leaf_engine::stalls::stalled_source_actor;

/// A launch whose work is crewed by a coder, working in session `coder-session`, and a reviewer
/// whose session `reviewer-session` was lost, so the launch reads Interrupted. The actor rows of
/// `armed` are armed.
async fn interrupted_launch(armed: &[&str]) -> Case {
    let (backend, home) = engine_backend();
    let wake = supervision_wake(&backend, &home);
    let delivery = Arc::new(RecordingTurnDelivery::default());
    wake.subscriptions.set_turn_delivery_actuator(delivery.clone()).await;
    let mut status = single_agent("coder");
    let snapshot = status.workflow_snapshot.as_mut().expect("workflow");
    snapshot.vessels[0].crew.push(flotilla_engine_convoys_testkit::agent("reviewer"));
    let spec = ConvoySpec::builder().workflow_ref("workflow".into()).project_ref("wheelhouse".into()).build();
    let work = home.admit("interrupted-work", spec, status).await;
    let vessel = home.vessel(&work, "work", "env");
    home.seat(&backend, &work, (&vessel, "work"), ("coder", "coder-session"), CrewWorkPhase::Working).await;
    home.seat(&backend, &work, (&vessel, "work"), ("reviewer", "reviewer-session"), CrewWorkPhase::Interrupted).await;
    let case = Case { backend, home, wake, delivery, work, vessel, governors: BTreeMap::new() };
    assert_eq!(case.status().await.phase, ConvoyPhase::Interrupted);
    for role in armed {
        let id = uuid::Uuid::new_v4();
        let leaf = leaf(LeafAddress::Work { convoy: case.work.clone(), work: "work".into() }, &format!(".crew.{role}.phase"), "Done");
        case.wake.subscriptions.inner.rows.lock().await.insert(
            id,
            LeafSubscriptionRow {
                id,
                namespace: "flotilla".into(),
                leaves: vec![leaf],
                watcher: LeafWatcher::ReconcilerWake { convoy: case.work.clone() },
                maker: LeafMaker::Actor { vessel: "work".into(), role: (*role).into() },
                freshness_demand: None,
                created_at: Utc::now(),
                episode_key: EpisodeKeyFields::default(),
            },
        );
    }
    case
}

/// The coder's session reads `state` at `now`, and the case is judged then.
async fn coder_reads(case: &Case, state: TerminalAttentionState, now: DateTime<Utc>) {
    observe_session(&case.home, "coder-session", Some(TerminalAttention { state, as_of: now, source: TerminalAttentionSource::Screen }));
    observe_agent(&case.home, "coder-session", state, TerminalAttentionSource::Screen, now);
    case.judge(&case.wake, now).await;
}

// #3108: a crew that is still live in an Interrupted launch is judged as in an Active one: a
// silent crew is a stall, whatever happened to another seat's session.
#[tokio::test]
async fn live_seat_of_interrupted_launch_stalls() {
    let case = interrupted_launch(&["coder", "reviewer"]).await;
    let start = Utc::now();
    for second in [0, 120, 600] {
        coder_reads(&case, TerminalAttentionState::Idle, start + chrono::Duration::seconds(second)).await;
    }
    let status = case.status().await;
    assert_eq!(status.phase, ConvoyPhase::Interrupted);
    let stall = status.stalled.expect("the idle coder stalls");
    assert_eq!(stalled_source_actor(&stall), Some(("work", "coder")), "{stall:?}");
    assert!(!stall.evidence.contains("session dead"), "the interrupted seat is not the stall's evidence: {}", stall.evidence);
}

// #3108: the seat whose session stopped is interrupted, and its relaunch is its path: the judge
// does not read it Stalled.
#[tokio::test]
async fn interrupted_seat_does_not_read_stalled() {
    let case = interrupted_launch(&["reviewer"]).await;
    let start = Utc::now();
    for second in [0, 120, 600] {
        coder_reads(&case, TerminalAttentionState::Idle, start + chrono::Duration::seconds(second)).await;
    }
    let status = case.status().await;
    assert_eq!(status.phase, ConvoyPhase::Interrupted);
    assert!(status.stalled.is_none(), "{:?}", status.stalled);
    assert_eq!(status.crew_work["work"]["reviewer"].phase, CrewWorkPhase::Interrupted);
}

// #3108: a seat whose session failed is interrupted as a lost one is, and the judge skips it the
// same way.
#[tokio::test]
async fn failed_session_seat_is_interrupted_not_stalled() {
    let (backend, home) = engine_backend();
    let wake = supervision_wake(&backend, &home);
    let delivery = Arc::new(RecordingTurnDelivery::default());
    wake.subscriptions.set_turn_delivery_actuator(delivery.clone()).await;
    let mut status = single_agent("coder");
    let snapshot = status.workflow_snapshot.as_mut().expect("workflow");
    snapshot.vessels[0].crew.push(flotilla_engine_convoys_testkit::agent("reviewer"));
    let spec = ConvoySpec::builder().workflow_ref("workflow".into()).project_ref("wheelhouse".into()).build();
    let work = home.admit("failed-session-work", spec, status).await;
    let vessel = home.vessel(&work, "work", "env");
    home.seat(&backend, &work, (&vessel, "work"), ("coder", "coder-session"), CrewWorkPhase::Working).await;
    let reviewer = home.seat(&backend, &work, (&vessel, "work"), ("reviewer", "reviewer-session"), CrewWorkPhase::Working).await;
    home.engine.terminals.failed(&reviewer.expect("the reviewer's session"), "agent launch rejected");
    let case = Case { backend, home, wake, delivery, work, vessel, governors: BTreeMap::new() };
    let id = uuid::Uuid::new_v4();
    case.wake.subscriptions.inner.rows.lock().await.insert(
        id,
        LeafSubscriptionRow {
            id,
            namespace: "flotilla".into(),
            leaves: vec![leaf(LeafAddress::Work { convoy: case.work.clone(), work: "work".into() }, ".crew.reviewer.phase", "Done")],
            watcher: LeafWatcher::ReconcilerWake { convoy: case.work.clone() },
            maker: LeafMaker::Actor { vessel: "work".into(), role: "reviewer".into() },
            freshness_demand: None,
            created_at: Utc::now(),
            episode_key: EpisodeKeyFields::default(),
        },
    );
    let start = Utc::now();
    for second in [0, 120, 600] {
        coder_reads(&case, TerminalAttentionState::Idle, start + chrono::Duration::seconds(second)).await;
    }
    let status = case.status().await;
    assert_eq!(status.phase, ConvoyPhase::Interrupted);
    assert_eq!(status.crew_work["work"]["reviewer"].phase, CrewWorkPhase::Interrupted);
    assert!(status.stalled.is_none(), "{:?}", status.stalled);
}
