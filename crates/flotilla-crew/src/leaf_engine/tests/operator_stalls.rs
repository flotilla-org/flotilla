use super::*;
use flotilla_resources::attentions::{OpenAttention, Sender, Subject};

/// A launch whose coder works in session `coder-session`, with no supervisor declared, at a home
/// whose fleet is designated so a person can be told. Its coder's actor row is armed.
async fn unsupervised_launch() -> Case {
    let (backend, home) = engine_backend().await;
    let wake = supervision_wake(&backend, &home);
    let delivery = Arc::new(RecordingTurnDelivery::default());
    wake.subscriptions.set_turn_delivery_actuator(delivery.clone()).await;
    let work = home.admit("unsupervised-work", convoy_spec(), single_agent("coder")).await;
    let vessel = home.vessel(&work, "work", "env");
    home.seat(&backend, &work, (&vessel, "work"), ("coder", "coder-session"), CrewWorkPhase::Working).await;
    let case = Case { backend, home, wake, delivery, work, vessel, governors: BTreeMap::new() };
    arm_coder(&case.wake, &case.work).await;
    case
}

/// Arms `wake`'s actor row for the coder of launch `work`.
async fn arm_coder(wake: &ReconcilerWake, work: &str) {
    let id = uuid::Uuid::new_v4();
    let leaf = leaf(LeafAddress::Work { convoy: work.to_owned(), work: "work".into() }, ".crew.coder.phase", "Done");
    wake.subscriptions.inner.rows.lock().await.insert(
        id,
        LeafSubscriptionRow {
            id,
            namespace: "flotilla".into(),
            leaves: vec![leaf],
            watcher: LeafWatcher::ReconcilerWake { convoy: work.to_owned() },
            maker: LeafMaker::Actor { vessel: "work".into(), role: "coder".into() },
            freshness_demand: None,
            created_at: Utc::now(),
            episode_key: EpisodeKeyFields::default(),
        },
    );
}

/// The home restarts: its notes on the launch and its wake's memory are gone, and a new wake
/// with the coder's row armed judges it.
async fn restart(case: Case) -> Case {
    case.home.engine().memory().forget(&case.work);
    let wake = supervision_wake(&case.backend, &case.home);
    wake.subscriptions.set_turn_delivery_actuator(case.delivery.clone()).await;
    arm_coder(&wake, &case.work).await;
    Case { wake, ..case }
}

/// The coder's session reads `state` at `now`, and the case is judged then.
async fn coder_reads(case: &Case, state: TerminalAttentionState, now: DateTime<Utc>) {
    observe_session(&case.home, "coder-session", Some(TerminalAttention { state, as_of: now, source: TerminalAttentionSource::Screen }));
    observe_agent(&case.home, "coder-session", state, TerminalAttentionSource::Screen, now);
    case.judge(&case.wake, now).await;
}

/// The stall judge's open attentions.
async fn stall_attentions(case: &Case) -> Vec<OpenAttention> {
    case.backend.attentions().open("flotilla", Some(Sender::StallJudge), None).await.expect("open attentions")
}

// #3108: a stall that reaches the operator with no person named tells one: one open attention for
// the stalled seat, however often it is judged, withdrawn when the crew works again.
#[tokio::test]
async fn unnamed_operator_stall_is_told_once_and_withdrawn_when_it_closes() {
    let case = unsupervised_launch().await;
    let start = Utc::now();
    let at = |tick: i64| start + chrono::Duration::seconds(tick * 600);
    // The idle coder is nudged until its nudges run out, then its stall reaches the operator.
    let mut tick = 0;
    loop {
        coder_reads(&case, TerminalAttentionState::Idle, at(tick)).await;
        tick += 1;
        if case.status().await.stalled.is_some_and(|stall| stall.rung == StallRung::Operator) || tick == 20 {
            break;
        }
    }
    let stall = case.status().await.stalled.expect("the idle coder stalls");
    assert_eq!((stall.rung, stall.supervisor.as_ref()), (StallRung::Operator, None), "{stall:?}");
    let told = stall_attentions(&case).await;
    let [attention] = told.as_slice() else { panic!("one attention for the stalled seat: {told:?}") };
    assert_eq!(attention.subject, Subject::ConvoyRole { convoy: case.work.clone(), role: "coder".into() });
    assert!(attention.reason.contains("Supervise stalled crew coder@work"), "{}", attention.reason);
    for later in tick..tick + 3 {
        coder_reads(&case, TerminalAttentionState::Idle, at(later)).await;
    }
    assert_eq!(stall_attentions(&case).await, told, "judging the same stall again tells no one again");
    coder_reads(&case, TerminalAttentionState::Working, at(tick + 3)).await;
    assert!(case.status().await.stalled.is_none(), "the working coder's stall closes");
    assert!(stall_attentions(&case).await.is_empty(), "the closed stall's attention is withdrawn");
}

// #3108: the attention is a told stall's durable record. A crew's declared stall at the unnamed
// operator rung outlives its home's restart: the seat still reads Stalled from the open attention,
// no second one is raised, and a new turn closes both.
#[tokio::test]
async fn told_stall_is_noted_again_after_a_restart() {
    let case = unsupervised_launch().await;
    let start = Utc::now();
    let at = |tick: i64| start + chrono::Duration::seconds(tick);
    case.home
        .note(&case.backend, &case.work, |status| {
            flotilla_resources::ConvoyStatusPatch::MarkCrewStalled {
                convoy: case.work.clone(),
                vessel: "work".into(),
                role: "coder".into(),
                at: start,
                reason: flotilla_resources::StallReason::Decision,
                proposed_disposition: None,
                message: "Need a decision on the schema".into(),
            }
            .apply(status);
        })
        .await;
    coder_reads(&case, TerminalAttentionState::Idle, at(1)).await;
    let told = stall_attentions(&case).await;
    let [attention] = told.as_slice() else { panic!("one attention for the declared stall: {told:?}") };
    assert!(attention.reason.contains("Need a decision on the schema"), "{}", attention.reason);

    let case = restart(case).await;
    assert!(case.status().await.stalled.is_none(), "a restarted home's notes are empty");
    for tick in 2..5 {
        coder_reads(&case, TerminalAttentionState::Idle, at(tick)).await;
    }
    let status = case.status().await;
    assert_eq!(status.crew_work["work"]["coder"].phase, CrewWorkPhase::Stalled, "{:?}", status.crew_work);
    let stall = status.stalled.expect("the told stall is noted again");
    assert_eq!((stall.rung, stall.supervisor.as_ref(), stall.seat()), (StallRung::Operator, None, Some(("work", "coder"))));
    assert_eq!(stall_attentions(&case).await, told, "the restarted home tells no one again");

    case.home
        .note(&case.backend, &case.work, |status| {
            flotilla_resources::ConvoyStatusPatch::ResumeCrewWork {
                intent: flotilla_resources::ResumeIntent::Reopen,
                vessel: "work".into(),
                role: "coder".into(),
                resumed_at: at(5),
                prompt: "Use the second schema".into(),
                brief_id: None,
            }
            .apply(status);
        })
        .await;
    coder_reads(&case, TerminalAttentionState::Working, at(6)).await;
    assert!(case.status().await.stalled.is_none(), "the new turn closes the stall");
    assert!(stall_attentions(&case).await.is_empty(), "and withdraws its attention");
}
