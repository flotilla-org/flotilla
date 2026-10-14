//! ADR 0064 gate boundaries outside the causally reachable scenario subset.
use state_machine_prototype::{
    attempt::AttemptPolicy,
    base::{ObjRef, Time},
    home::Home,
    kinds::{ensure::*, message::*},
    view::{Fact, MemoryView, Transport},
};
use std::time::Duration;
fn reference(kind: &str, name: &str) -> ObjRef {
    ObjRef::new(kind, name)
}
fn message(expectation: Expectation) -> (Home<Message>, ObjRef) {
    let mut home = Home::new("home").expect("definition");
    let id = home
        .create(
            "m",
            MessageSpec {
                sender: "sender".into(),
                inbox: "role".into(),
                body: "brief".into(),
                expectation,
                deadline: Some(Time(1)),
                unbound_after: Duration::from_millis(1),
            },
        )
        .expect("message");
    (home, id)
}
// Expiry needs time strictly past the deadline AND an applied prefix strictly past
// it when bound. A visible reply wins. Enumerate both booleans and all boundary times.
#[test]
fn expiry_gate_at_every_clock_and_watermark_boundary() {
    for now in 0..=2 {
        for mark in 0..=2 {
            for bound in [false, true] {
                for met in [false, true] {
                    let (mut home, id) = message(Expectation::Reply);
                    let mut view = MemoryView::new();
                    view.set_now(Time(now));
                    if bound {
                        view.observe(&reference("binding", "role"), "home", Fact::Consumer { host: "consumer".into() });
                    }
                    view.observe(&reference("keepalive", "consumer"), "home", Fact::AppliedThrough(Time(mark)));
                    view.observe(&reference("reply-index", &id.to_string()), "home", Fact::ReplyObserved(met));
                    let result = home.apply(&id, MessageEdge::Drop { reason: DropReason::Expired }, &view);
                    assert_eq!(result.is_ok(), now > 1 && (!bound || mark > 1) && !met, "now={now}, mark={mark}, bound={bound}, met={met}");
                    // Gate evidence includes the local clock value that allowed expiry.
                    if let Ok(event) = result {
                        assert!(event.inputs.iter().any(|input| input.subject == reference("clock", "home") && input.version == now));
                    }
                }
            }
        }
    }
}
// A recorded receipt must name both the supplied receiver and transport. This
// finite generator spans missing, mismatching and matching evidence independently.
#[test]
fn delivery_gate_requires_the_exact_receipt() {
    for present in [false, true] {
        for receiver_matches in [false, true] {
            for transport_matches in [false, true] {
                let (mut home, id) = message(Expectation::Nothing);
                let mut view = MemoryView::new();
                let receipt = reference("receipt", "r");
                if present {
                    view.observe(
                        &receipt,
                        "consumer",
                        Fact::Receipt {
                            session: if receiver_matches { "S" } else { "T" }.into(),
                            transport: if transport_matches { Transport::Turn } else { Transport::Muted },
                        },
                    );
                }
                let result = home.apply(&id, MessageEdge::Deliver { receiver: "S".into(), receipt, transport: Transport::Turn }, &view);
                assert_eq!(result.is_ok(), present && receiver_matches && transport_matches);
            }
        }
    }
}
// An outcome may settle Pending directly; a late receipt cannot append an edge.
#[test]
fn pending_outcome_settlement_closes_the_write_path() {
    let condition = reference("condition", "outcome");
    let (mut home, id) = message(Expectation::Outcome { condition: condition.clone() });
    let mut view = MemoryView::new();
    view.observe(&condition, "consumer", Fact::OutcomeMet(true));
    home.apply(&id, MessageEdge::Settle { by: Settlement::OutcomeObserved { observation: condition } }, &view).expect("settles Pending");
    let receipt = reference("receipt", "late");
    view.observe(&receipt, "consumer", Fact::Receipt { session: "S".into(), transport: Transport::Turn });
    assert!(home.apply(&id, MessageEdge::Deliver { receiver: "S".into(), receipt, transport: Transport::Turn }, &view).is_err());
    let resource = home.get(&id).expect("message");
    assert_eq!(resource.state(), MessageState::Settled);
    assert_eq!(resource.events.len(), 1);
}
// Withdrawal and deliberate unreachable drops require an authorised decision;
// the closed enum carries the reason into the event, including withdrawn admission.
#[test]
fn deliberate_drop_reasons_are_authorised_and_recorded() {
    for unreachable in [false, true] {
        for authorised in [false, true] {
            let (mut home, id) = message(Expectation::Reply);
            let mut view = MemoryView::new();
            let authority = reference("authority", "owner");
            if authorised {
                view.observe(&authority, "home", Fact::Authorised { party: "owner".into() });
            }
            let reason = if unreachable {
                DropReason::ConsumerUnreachable { by: "owner".into(), authority }
            } else {
                DropReason::Withdrawn { by: "owner".into(), authority }
            };
            assert_eq!(home.apply(&id, MessageEdge::Drop { reason: reason.clone() }, &view).is_ok(), authorised);
            if authorised {
                let MessageEdge::Drop { reason: actual } = &home.get(&id).expect("message").last_event("drop").expect("drop event").edge
                else {
                    panic!("drop payload")
                };
                assert_eq!(actual, &reason);
            }
        }
    }
}
// Bind needs all three independent facts; merely being created or recorded is
// insufficient. Unbind must observe the actual bound fulfiller, not another Gone row.
#[test]
fn ensure_bind_checks_every_input_and_unbind_checks_identity() {
    for recorded in [false, true] {
        for realised in [false, true] {
            for can_receive in [false, true] {
                let mut home = Home::<Ensure>::new("home").expect("definition");
                let id = home
                    .create(
                        "ensure",
                        EnsureSpec { policy: AttemptPolicy { max_attempts: 1, base: Duration::ZERO, factor: 1, cap: Duration::ZERO } },
                    )
                    .expect("ensure");
                let fulfiller = reference("fulfiller", "one");
                let relation = reference("recorded", "one");
                let mut view = MemoryView::new();
                if recorded {
                    view.observe(&relation, "home", Fact::FulfillerRecorded { fulfiller: fulfiller.clone() });
                }
                view.observe(&fulfiller, "home", Fact::Fulfiller { realised, can_receive, gone: false });
                let bound = home.apply(&id, EnsureEdge::Bind { fulfiller, recorded: relation }, &view).is_ok();
                assert_eq!(bound, recorded && realised && can_receive);
                if bound {
                    let other = reference("fulfiller", "other");
                    view.observe(&other, "home", Fact::Fulfiller { realised: false, can_receive: false, gone: true });
                    assert!(home.apply(&id, EnsureEdge::Unbind { reason: UnbindReason::Replaced, observed: other }, &view).is_err());
                }
            }
        }
    }
}

// Only the message's declared expectation can beat expiry. An unrelated reply
// or outcome is not evidence that this message did its job.
#[test]
fn expiry_ignores_evidence_for_an_undeclared_expectation() {
    let condition = reference("condition", "outcome");
    for expectation in [Expectation::Nothing, Expectation::Reply, Expectation::Outcome { condition: condition.clone() }] {
        for reply in [false, true] {
            for outcome in [false, true] {
                let met = match expectation {
                    Expectation::Nothing => false,
                    Expectation::Reply => reply,
                    Expectation::Outcome { .. } => outcome,
                };
                let (mut home, id) = message(expectation.clone());
                let mut view = MemoryView::new();
                view.set_now(Time(2));
                view.observe(&reference("binding", "role"), "home", Fact::Consumer { host: "consumer".into() });
                view.observe(&reference("keepalive", "consumer"), "home", Fact::AppliedThrough(Time(2)));
                view.observe(&reference("reply-index", &id.to_string()), "home", Fact::ReplyObserved(reply));
                view.observe(&condition, "consumer", Fact::OutcomeMet(outcome));
                assert_eq!(home.apply(&id, MessageEdge::Drop { reason: DropReason::Expired }, &view).is_ok(), !met);
            }
        }
    }
}

// ADR 0064: a silent consumer leaves a message Awaiting. Unbinding/replacing
// the role consumer must not bypass the already-delivered receiver's watermark.
#[test]
fn awaiting_expiry_keeps_the_recorded_receiver_after_unbind_or_replacement() {
    for replacement in [None, Some("replacement")] {
        let (mut home, id) = message(Expectation::Reply);
        let mut view = MemoryView::new();
        let receipt = reference("receipt", "delivered");
        view.observe(&receipt, "original", Fact::Receipt { session: "S".into(), transport: Transport::Turn });
        home.apply(&id, MessageEdge::Deliver { receiver: "S".into(), receipt, transport: Transport::Turn }, &view).expect("delivery");
        view.set_now(Time(2));
        let binding = replacement.map_or(Fact::UnboundSince(Time(0)), |host| Fact::Consumer { host: host.into() });
        view.observe(&reference("binding", "role"), "home", binding);
        view.observe(&reference("keepalive", "original"), "home", Fact::AppliedThrough(Time(0)));
        view.observe(&reference("keepalive", "replacement"), "home", Fact::AppliedThrough(Time(2)));
        assert!(home.apply(&id, MessageEdge::Drop { reason: DropReason::Expired }, &view).is_err());
        assert_eq!(home.get(&id).expect("message").state(), MessageState::Awaiting);
        view.observe(&reference("keepalive", "original"), "home", Fact::AppliedThrough(Time(2)));
        home.apply(&id, MessageEdge::Drop { reason: DropReason::Expired }, &view).expect("original receiver prefix is now complete");
    }
}
