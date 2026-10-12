//! Option C: Stateright explores the real Rust `step` directly.
//! The model *is* the implementation; properties are Rust closures.
use convoy_spike::{step, CrewPhase, Event, Phase, State};
use stateright::report::WriteReporter;
use stateright::{Checker, Model, Property};
use std::hash::{Hash, Hasher};

#[derive(Clone, Debug, PartialEq, Eq)]
struct S(State);
impl Hash for S {
    fn hash<H: Hasher>(&self, h: &mut H) {
        format!("{:?}", self.0).hash(h) // spike shortcut; real code derives Hash
    }
}

struct ConvoyModel;
const EVENTS: [Event; 10] = [
    Event::Submit, Event::Complete, Event::Handoff, Event::Merge, Event::VerdictKept,
    Event::VerdictRejected, Event::TurnDelivery, Event::Settle, Event::Abandon, Event::Teardown,
];

impl Model for ConvoyModel {
    type State = S;
    type Action = Event;
    fn init_states(&self) -> Vec<S> { vec![S(State::default())] }
    fn actions(&self, s: &S, out: &mut Vec<Event>) {
        out.extend(EVENTS.iter().copied().filter(|e| step(&s.0, *e).is_ok()));
    }
    fn next_state(&self, s: &S, e: Event) -> Option<S> { step(&s.0, e).ok().map(|(n, _)| S(n)) }
    // crew_epoch is unbounded (handoff/turn-delivery loops), so bound it.
    fn within_boundary(&self, s: &S) -> bool { s.0.crew_epoch <= 3 }
    fn properties(&self) -> Vec<Property<Self>> {
        vec![
            Property::always("landed only after merge", |_, s: &S| s.0.phase != Phase::Landed || s.0.pr_merged),
            Property::always("no work without vessel", |_, s: &S| s.0.crew != CrewPhase::Working || s.0.vessel_live),
            Property::always("landed claims fresh", |_, s: &S| {
                s.0.phase != Phase::Landed || s.0.landing_entry.is_some_and(|e| e.claims_epoch == s.0.crew_epoch)
            }),
            Property::sometimes("can land", |_, s: &S| s.0.phase == Phase::Landed),
            Property::eventually("reaches terminal", |_, s: &S| s.0.phase.is_terminal()),
        ]
    }
}

#[test]
fn stateright_checks_real_step() {
    let c = ConvoyModel.checker().spawn_bfs().join().report(&mut WriteReporter::new(&mut std::io::stdout()));
    let d = c.discoveries();
    for (name, path) in &d { println!("discovery: {name}: {:?}", path.clone().into_actions()); }
    assert!(d.contains_key("can land"));
    assert!(!d.contains_key("landed only after merge"));
    assert!(!d.contains_key("landed claims fresh"));
}
