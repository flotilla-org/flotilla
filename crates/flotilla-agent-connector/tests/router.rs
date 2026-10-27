use std::{
    collections::BTreeSet,
    future::Future,
    pin::pin,
    sync::{Arc, Mutex},
    task::{Context, Poll, Waker},
};

use async_trait::async_trait;
use chrono::DateTime;
use flotilla_agent_connector::{
    AgentTransport, Delivery, DeliveryTarget, HarnessRef, Operation, OperationDeclaration, OperationPolicy, RouteDeclarations,
    RouterConfig, ShapeId, StateSignal, SubmissionRef, TransportId, TransportObservation, TransportOffer, TransportOutcome,
    TransportRouter, KEYSTROKE, TUI_SHAPE,
};
use hegel::generators as gs;

// The scripted transports never wait, so polling with a no-op waker drives
// every router future to completion without a runtime.
fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    let mut context = Context::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(output) = future.as_mut().poll(&mut context) {
            return output;
        }
    }
}

type Calls = Arc<Mutex<Vec<(TransportId, Option<TransportId>, String)>>>;

// In-memory transport: records each submission and answers with a scripted outcome.
struct ScriptedTransport {
    id: TransportId,
    outcome: TransportOutcome,
    calls: Calls,
}

#[async_trait]
impl AgentTransport for ScriptedTransport {
    fn id(&self) -> TransportId {
        self.id.clone()
    }
    async fn observe(&self, _: &DeliveryTarget, _: Option<&SubmissionRef>) -> Result<TransportObservation, String> {
        Ok(TransportObservation { evidence: Some(self.id.to_string()), ..Default::default() })
    }
    async fn submit(&self, delivery: &Delivery) -> TransportOutcome {
        self.calls.lock().expect("calls").push((self.id.clone(), delivery.submission.transport.clone(), delivery.text.clone()));
        self.outcome.clone()
    }
    async fn poll(&self, _: &Delivery) -> TransportOutcome {
        TransportOutcome::Accepted { evidence: format!("polled {}", self.id) }
    }
}

struct Offers(Vec<TransportOffer>);

impl RouteDeclarations for Offers {
    fn declare(&self, operation: Operation, _: Option<&str>) -> OperationDeclaration {
        match operation {
            Operation::NewTurn => OperationDeclaration::offering(self.0.clone()),
            _ => OperationDeclaration::none(),
        }
    }
}

fn target(session: &str) -> DeliveryTarget {
    DeliveryTarget {
        namespace: "project".into(),
        holder: "terminal".into(),
        session: Some(session.into()),
        crew_id: Some("crew".into()),
        harness: Some(HarnessRef::new("harness", None)),
        live_shapes: None,
    }
}

fn delivery(session: &str, text: &str) -> Delivery {
    Delivery {
        operation: Operation::NewTurn,
        target: target(session),
        submission: SubmissionRef {
            operator_reason: None,
            batch_id: format!("batch-{text}"),
            crew_id: "crew".into(),
            session: session.into(),
            started_at: DateTime::from_timestamp(0, 0).expect("timestamp"),
            members: vec![text.into()],
            transport: None,
        },
        text: text.into(),
    }
}

fn outcome(choice: usize) -> TransportOutcome {
    match choice {
        0 => TransportOutcome::Pending,
        1 => TransportOutcome::Accepted { evidence: "receipt".into() },
        2 => TransportOutcome::NotSubmitted { reason: "not ready".into() },
        3 => TransportOutcome::Unconfirmed { reason: "maybe typed".into() },
        _ => TransportOutcome::Unsupported { reason: "old build".into() },
    }
}

fn possibly_delivered(outcome: &TransportOutcome) -> bool {
    matches!(outcome, TransportOutcome::Pending | TransportOutcome::Accepted { .. } | TransportOutcome::Unconfirmed { .. })
}

// Behaviour (ADR 0059 §4, #3545): each delivery goes through one transport,
// never two where that can be avoided, and at least one delivers when any
// eligible transport can; the router reports the transport whose outcome it
// returns, and a transport the session's build refused is never chosen
// again for that session. Generator: 1-3 registered transports, offered in
// a drawn order (with duplicates and unregistered offers), each scripted
// with every outcome variant, with and without accepted repeats, over a
// sequence of 1-4 deliveries to one session. Invariants are checked after
// every delivery against a model of the eligible order.
// Each case runs in microseconds, so the property affords more cases than the
// default profile to reach the rarer outcome/order/repeat combinations.
#[test]
fn router_delivers_through_one_transport_and_names_it() {
    hegel::Hegel::new(router_delivers_through_one_transport).settings(hegel::Settings::new().test_cases(300)).run();
}

fn router_delivers_through_one_transport(tc: hegel::TestCase) {
    let registered = tc.draw(gs::integers::<usize>().min_value(1).max_value(3));
    let outcomes: Vec<_> = (0..registered).map(|_| outcome(tc.draw(gs::integers::<usize>().min_value(0).max_value(4)))).collect();
    let offer_count = tc.draw(gs::integers::<usize>().min_value(0).max_value(4));
    // Index `registered` is an offer for a transport this host never registered.
    let offers: Vec<usize> = (0..offer_count).map(|_| tc.draw(gs::integers::<usize>().min_value(0).max_value(registered))).collect();
    let accept_repeat = tc.draw(gs::booleans());
    let deliveries = tc.draw(gs::integers::<usize>().min_value(1).max_value(4));
    let name = |index: usize| TransportId::new(format!("t{index}"));
    let calls: Calls = Default::default();
    let mut router = TransportRouter::new()
        .with_connector("harness", Arc::new(Offers(offers.iter().map(|index| TransportOffer::new(name(*index), TUI_SHAPE)).collect())))
        .with_config(RouterConfig { operations: [(Operation::NewTurn, OperationPolicy { accept_repeat, ..Default::default() })].into() });
    for (index, outcome) in outcomes.iter().enumerate() {
        router = router.with_transport(Arc::new(ScriptedTransport { id: name(index), outcome: outcome.clone(), calls: calls.clone() }));
    }
    let mut refused = BTreeSet::new();
    for step in 0..deliveries {
        let text = format!("message-{step}");
        let routed = block_on(router.submit(&delivery("session", &text)));
        let made: Vec<_> = calls.lock().expect("calls").drain(..).collect();
        // Model: offered, registered, deduplicated, not refused, in declaration order.
        let mut eligible = Vec::new();
        for index in &offers {
            if *index < registered && !eligible.contains(index) && !refused.contains(index) {
                eligible.push(*index);
            }
        }
        let stops = |index: &usize| {
            let outcome = &outcomes[*index];
            matches!(outcome, TransportOutcome::Pending | TransportOutcome::Accepted { .. })
                || (!accept_repeat && matches!(outcome, TransportOutcome::Unconfirmed { .. }))
        };
        let tried: Vec<usize> = match eligible.iter().position(stops) {
            Some(stop) => eligible[..=stop].to_vec(),
            None => eligible.clone(),
        };
        assert_eq!(made.iter().map(|call| call.0.clone()).collect::<Vec<_>>(), tried.iter().map(|index| name(*index)).collect::<Vec<_>>());
        for (transport, recorded, sent) in &made {
            assert_eq!(recorded.as_ref(), Some(transport), "a transport sees itself as the submission's transport");
            assert_eq!(sent, &text);
        }
        let deliveries = tried.iter().filter(|index| possibly_delivered(&outcomes[**index])).count();
        if !accept_repeat {
            assert!(deliveries <= 1, "never two deliveries without an accepted repeat");
        }
        if tried.iter().any(|index| possibly_delivered(&outcomes[*index])) {
            assert!(deliveries >= 1);
        }
        match tried.last() {
            None => {
                assert_eq!(routed.transport, None);
                assert!(matches!(routed.outcome, TransportOutcome::NotSubmitted { .. }));
            }
            Some(last) => {
                // Exhausted after an accepted repeat: the last transport that may have delivered.
                let used = if stops(last) {
                    *last
                } else {
                    *tried.iter().rev().find(|index| matches!(outcomes[**index], TransportOutcome::Unconfirmed { .. })).unwrap_or(last)
                };
                assert_eq!(routed.transport, Some(name(used)), "the router names the transport whose outcome it returns");
                assert_eq!(routed.failovers.len(), tried.len() - 1);
                if stops(last) {
                    assert_eq!(routed.outcome, outcomes[*last]);
                } else if tried.iter().any(|index| matches!(outcomes[*index], TransportOutcome::Unconfirmed { .. })) {
                    assert!(matches!(routed.outcome, TransportOutcome::Unconfirmed { .. }), "a repeat may have delivered");
                } else {
                    assert!(matches!(routed.outcome, TransportOutcome::NotSubmitted { .. }));
                }
            }
        }
        refused.extend(tried.iter().copied().filter(|index| matches!(outcomes[*index], TransportOutcome::Unsupported { .. })));
    }
}

// Behaviour (#3566's fallback rule): when the guarded transport is refused as
// unsupported, the router falls back once to the unguarded one, and later
// deliveries to that session skip the guarded transport without retrying it;
// a new session id starts clean.
#[test]
fn unsupported_guard_falls_back_once_per_session() {
    block_on(async {
        let calls: Calls = Default::default();
        let router = TransportRouter::new()
            .with_connector(
                "harness",
                Arc::new(Offers(vec![TransportOffer::new("keystroke-guarded", TUI_SHAPE), TransportOffer::new(KEYSTROKE, TUI_SHAPE)])),
            )
            .with_transport(Arc::new(ScriptedTransport {
                id: "keystroke-guarded".into(),
                outcome: TransportOutcome::Unsupported { reason: "daemon lacks the guard".into() },
                calls: calls.clone(),
            }))
            .with_transport(Arc::new(ScriptedTransport {
                id: KEYSTROKE.into(),
                outcome: TransportOutcome::Accepted { evidence: "typed".into() },
                calls: calls.clone(),
            }));
        let first = router.submit(&delivery("old", "one")).await;
        assert_eq!(first.transport, Some(KEYSTROKE.into()));
        assert_eq!(first.failovers.len(), 1);
        assert_eq!(first.failovers[0].transport, TransportId::new("keystroke-guarded"));
        let second = router.submit(&delivery("old", "two")).await;
        assert_eq!(second.transport, Some(KEYSTROKE.into()));
        assert!(second.failovers.is_empty());
        router.submit(&delivery("relaunched", "three")).await;
        let order: Vec<_> = calls.lock().expect("calls").iter().map(|call| call.0.to_string()).collect();
        assert_eq!(order, ["keystroke-guarded", "keystroke", "keystroke", "keystroke-guarded", "keystroke"]);
    });
}

// Behaviour: configuration reorders and withholds offered transports but
// never adds one; offers on shapes not live for the session are skipped.
#[test]
fn configuration_and_live_shapes_filter_offers() {
    block_on(async {
        let calls: Calls = Default::default();
        let transport = |id: &str| -> Arc<dyn AgentTransport> {
            Arc::new(ScriptedTransport { id: id.into(), outcome: TransportOutcome::Pending, calls: calls.clone() })
        };
        let declarations = Arc::new(Offers(vec![
            TransportOffer::new("a", TUI_SHAPE),
            TransportOffer::new("b", "app-server"),
            TransportOffer::new("c", TUI_SHAPE),
        ]));
        let router = TransportRouter::new()
            .with_connector("harness", declarations)
            .with_transport(transport("a"))
            .with_transport(transport("b"))
            .with_transport(transport("c"))
            .with_transport(transport("unoffered"))
            .with_config(RouterConfig {
                operations: [(
                    Operation::NewTurn,
                    OperationPolicy {
                        prefer: vec!["unoffered".into(), "c".into()],
                        deny: ["a".into()].into(),
                        state_order: Some(vec![StateSignal::Screen]),
                        ..Default::default()
                    },
                )]
                .into(),
            });
        let mut both = target("session");
        assert_eq!(router.candidates(Operation::NewTurn, &both), ["c".into(), "b".into()]);
        both.live_shapes = Some([ShapeId::new(TUI_SHAPE)].into());
        assert_eq!(router.candidates(Operation::NewTurn, &both), [TransportId::new("c")]);
        assert_eq!(router.state_order(Operation::NewTurn, &both), [StateSignal::Screen]);
        assert!(router.candidates(Operation::Interrupt, &both).is_empty());
    });
}

// Behaviour: a session with no registered connector gets the keystroke
// declaration, so today's cleat path stays the route for new turns. A
// recorded submission's transport answers observe and poll, never another.
#[test]
fn unknown_harness_uses_keystroke_and_recorded_transport_answers() {
    block_on(async {
        let calls: Calls = Default::default();
        let router = TransportRouter::new()
            .with_transport(Arc::new(ScriptedTransport { id: "other".into(), outcome: TransportOutcome::Pending, calls: calls.clone() }))
            .with_transport(Arc::new(ScriptedTransport { id: KEYSTROKE.into(), outcome: TransportOutcome::Pending, calls: calls.clone() }));
        let mut delivery = delivery("session", "one");
        delivery.target.harness = None;
        assert_eq!(router.candidates(Operation::NewTurn, &delivery.target), [TransportId::new(KEYSTROKE)]);
        let (used, observed) = router.observe(Operation::NewTurn, &delivery.target, None).await.expect("observe");
        assert_eq!(used, TransportId::new(KEYSTROKE));
        assert_eq!(observed.evidence.as_deref(), Some(KEYSTROKE));
        delivery.submission.transport = Some("other".into());
        let (used, observed) = router.observe(Operation::NewTurn, &delivery.target, Some(&delivery.submission)).await.expect("observe");
        assert_eq!(used, TransportId::new("other"));
        assert_eq!(observed.evidence.as_deref(), Some("other"));
        assert_eq!(router.poll(&delivery).await, TransportOutcome::Accepted { evidence: "polled other".into() });
        delivery.submission.transport = Some("gone".into());
        assert!(matches!(router.poll(&delivery).await, TransportOutcome::Unconfirmed { .. }));
        assert!(router.observe(Operation::NewTurn, &delivery.target, Some(&delivery.submission)).await.is_err());
        assert!(calls.lock().expect("calls").is_empty(), "observe and poll never submit");
    });
}

// Behaviour (ADR 0059 §4): an unknown outcome counts as possibly delivered, so
// without an accepted repeat the router never tries another transport after
// it; with one, it moves on and the result stays unconfirmed.
#[test]
fn unconfirmed_fails_over_only_when_configuration_accepts_a_repeat() {
    for accept_repeat in [false, true] {
        let calls: Calls = Default::default();
        let router = TransportRouter::new()
            .with_connector("harness", Arc::new(Offers(vec![TransportOffer::new("a", TUI_SHAPE), TransportOffer::new("b", TUI_SHAPE)])))
            .with_transport(Arc::new(ScriptedTransport {
                id: "a".into(),
                outcome: TransportOutcome::Unconfirmed { reason: "maybe typed".into() },
                calls: calls.clone(),
            }))
            .with_transport(Arc::new(ScriptedTransport {
                id: "b".into(),
                outcome: TransportOutcome::NotSubmitted { reason: "not live".into() },
                calls: calls.clone(),
            }))
            .with_config(RouterConfig {
                operations: [(Operation::NewTurn, OperationPolicy { accept_repeat, ..Default::default() })].into(),
            });
        let routed = block_on(router.submit(&delivery("session", "one")));
        let order: Vec<_> = calls.lock().expect("calls").iter().map(|call| call.0.to_string()).collect();
        if accept_repeat {
            assert_eq!(order, ["a", "b"]);
            assert_eq!(routed.transport, Some("a".into()), "polls follow the transport that may have delivered");
            assert_eq!(routed.failovers, [flotilla_agent_connector::Failover { transport: "b".into(), reason: "not live".into() }]);
            assert!(matches!(routed.outcome, TransportOutcome::Unconfirmed { .. }), "a repeat may have delivered");
        } else {
            assert_eq!(order, ["a"]);
            assert_eq!(routed.transport, Some("a".into()));
            assert_eq!(routed.outcome, TransportOutcome::Unconfirmed { reason: "maybe typed".into() });
        }
    }
}

// Behaviour: a refusal is remembered under the session `candidates` filters
// on (the target's live session), even when the recorded submission names a
// different one, so it takes effect on the next delivery.
#[test]
fn refusal_is_keyed_by_the_target_session() {
    let calls: Calls = Default::default();
    let router = TransportRouter::new()
        .with_connector(
            "harness",
            Arc::new(Offers(vec![TransportOffer::new("keystroke-guarded", TUI_SHAPE), TransportOffer::new(KEYSTROKE, TUI_SHAPE)])),
        )
        .with_transport(Arc::new(ScriptedTransport {
            id: "keystroke-guarded".into(),
            outcome: TransportOutcome::Unsupported { reason: "old build".into() },
            calls: calls.clone(),
        }))
        .with_transport(Arc::new(ScriptedTransport { id: KEYSTROKE.into(), outcome: TransportOutcome::Pending, calls: calls.clone() }));
    let mut first = delivery("live", "one");
    first.submission.session = "recorded".into();
    block_on(router.submit(&first));
    assert_eq!(router.candidates(Operation::NewTurn, &target("live")), [TransportId::new(KEYSTROKE)]);
    assert_eq!(router.candidates(Operation::NewTurn, &target("other")).len(), 2);
}

// A session that requires an agent restart cannot recover by selecting another
// input transport. Even accepting repeats must preserve the named failure.
#[hegel::test]
fn session_restart_failure_does_not_fall_back(tc: hegel::TestCase) {
    // Cover both repeat policies and every count of alternative transports 0..=4.
    let accept_repeat = tc.draw(gs::booleans());
    let alternatives = tc.draw(gs::integers::<usize>().min_value(0).max_value(4));
    let calls: Calls = Default::default();
    let failure = TransportOutcome::Deferred { reason: "cleat composer did not match".into(), reform_batch: false };
    let offers: Vec<_> = (0..=alternatives).map(|i| TransportOffer::new(TransportId::new(format!("transport-{i}")), TUI_SHAPE)).collect();
    let mut router = TransportRouter::new().with_connector("harness", Arc::new(Offers(offers)));
    for i in 0..=alternatives {
        router = router.with_transport(Arc::new(ScriptedTransport {
            id: TransportId::new(format!("transport-{i}")),
            outcome: failure.clone(),
            calls: calls.clone(),
        }));
    }
    router = router
        .with_config(RouterConfig { operations: [(Operation::NewTurn, OperationPolicy { accept_repeat, ..Default::default() })].into() });
    let routed = block_on(router.submit(&delivery("session", "turn")));
    assert_eq!(routed.outcome, failure);
    assert!(routed.failovers.is_empty());
    assert_eq!(calls.lock().expect("submissions").len(), 1);
}
