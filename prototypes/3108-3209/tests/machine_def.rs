//! The declaration rules, checked on deliberately bad machines, and the diagram.

use state_machine_prototype::base::ObjRef;
use state_machine_prototype::home::Home;
use state_machine_prototype::kinds::lifecycle::{CloneMachine, ImageBuildMachine, LifecycleState::*};
use state_machine_prototype::kinds::message::Message;
use state_machine_prototype::machine::{DefError, EdgeDef, GateFailure, Machine, MachineDef};
use state_machine_prototype::record::RecordLog;

type D = MachineDef<CloneMachine>;

fn open() -> state_machine_prototype::machine::GateFn<CloneMachine> {
    |_, _, _| Ok(())
}

fn errs(def: D) -> Vec<DefError> {
    def.validate().unwrap_err()
}

#[test]
fn shipped_machines_pass_validation() {
    CloneMachine::definition().validate().unwrap();
    ImageBuildMachine::definition().validate().unwrap();
    Message::definition().validate().unwrap();
    assert!(Home::<Message>::new("kiwi").is_ok());
}

#[test]
fn an_edge_out_of_an_end_state_is_rejected() {
    let def = CloneMachine::definition().edge(EdgeDef::new("resurrect").from([Gone]).to(Requested).gate("never", open()));
    assert!(errs(def).contains(&DefError::EdgeLeavesEnd { edge: "resurrect", end: "Gone" }));
}

#[test]
fn a_state_and_an_edge_may_not_share_a_name() {
    let def = D::new()
        .edge(EdgeDef::new("realised").from([Requested]).to(Realised).gate("x", open()))
        .edge(EdgeDef::new("go").from([Realised]).to(Gone).gate("x", open()))
        .end(Gone, "r");
    assert!(errs(def).contains(&DefError::NameClash("realised")));
}

#[test]
fn ungated_unsourced_and_targetless_edges_are_rejected() {
    let mut bad = EdgeDef::<CloneMachine>::new("poke");
    bad.target = None;
    let e = errs(D::new().edge(bad));
    for want in [DefError::UngatedEdge("poke"), DefError::EdgeWithoutTarget("poke"), DefError::EdgeWithoutSource("poke")] {
        assert!(e.contains(&want), "{want:?} in {e:?}");
    }
}

#[test]
fn unreachable_dead_and_reasonless_states_are_rejected() {
    let def = D::new().edge(EdgeDef::new("realise").from([Requested]).to(Realised).gate("x", open())).end(Gone, " ");
    let e = errs(def);
    assert!(e.contains(&DefError::UnreachableState("Gone")));
    assert!(e.contains(&DefError::UnreachableState("GaveUp")));
    assert!(e.contains(&DefError::DeadState("Realised")), "{e:?}");
    assert!(e.contains(&DefError::EndWithoutReason("Gone")));
}

#[test]
fn duplicate_edge_names_are_rejected() {
    let def = D::new()
        .edge(EdgeDef::new("realise").from([Requested]).to(Realised).gate("x", open()))
        .edge(EdgeDef::new("realise").from([Realised]).to(Gone).gate("x", open()));
    assert!(errs(def).contains(&DefError::DuplicateEdge("realise")));
}

#[test]
fn a_chosen_target_outside_its_declared_set_is_refused_at_apply() {
    use state_machine_prototype::home::Refusal;
    use state_machine_prototype::kinds::lifecycle::{CloneRequest, LifecycleEdge};
    use state_machine_prototype::view::MemoryView;
    // A machine whose picker lies: declares {Realised} but picks Gone.
    #[derive(Clone, Debug)]
    struct Liar;
    impl Machine for Liar {
        const NAME: &'static str = "Liar";
        type Spec = CloneRequest;
        type State = state_machine_prototype::kinds::lifecycle::LifecycleState;
        type Edge = LifecycleEdge;
        fn initial() -> Self::State {
            Requested
        }
        fn definition() -> MachineDef<Self> {
            MachineDef::new()
                .edge(EdgeDef::new("realise").from([Requested]).to_chosen(vec![Realised], |_, _| Gone).gate("x", |_, _, _| Ok(())))
                .edge(EdgeDef::new("vanish").from([Realised]).to(Gone).gate("x", |_, _, _| Err(GateFailure("no".into()))))
                .edge(EdgeDef::new("give up").from([Requested]).to(GaveUp).gate("x", |_, _, _| Ok(())))
                .end(Gone, "r")
                .end(GaveUp, "r")
        }
    }
    let mut home = Home::<Liar>::new("kiwi").unwrap();
    let id = home.create("l", CloneRequest { url: "u".into(), path: "p".into() }).unwrap();
    let edge = LifecycleEdge::Realise { identity: "i".into(), observed: ObjRef::new("o", "o") };
    let err = home.apply(&id, edge, &MemoryView::new()).unwrap_err();
    assert_eq!(err, Refusal::BadTarget { edge: "realise", picked: "Gone" });
}

#[test]
fn the_message_machine_draws_as_the_agreed_diagram() {
    let d = Message::definition().mermaid();
    for line in [
        "[*] --> Pending",
        "Pending --> Settled: deliver (target chosen)",
        "Pending --> Awaiting: deliver (target chosen)",
        "Awaiting --> Settled: settle",
        "Pending --> Dropped: drop",
        "Awaiting --> Dropped: drop",
        "Settled --> [*]",
        "Dropped --> [*]",
    ] {
        assert!(d.contains(line), "missing {line:?} in\n{d}");
    }
    for end in ["Settled", "Dropped"] {
        let leaving: Vec<_> = d.lines().filter(|l| l.trim_start().starts_with(&format!("{end} -->")) && !l.ends_with("[*]")).collect();
        assert!(leaving.is_empty(), "{leaving:?}");
    }
}

#[test]
fn the_lifecycle_draws_as_four_states_four_edges() {
    let d = CloneMachine::definition().mermaid();
    for line in
        ["Requested --> Realised: realise", "Realised --> Gone: vanish", "Realised --> Gone: release", "Requested --> GaveUp: give up"]
    {
        assert!(d.contains(line), "missing {line:?} in\n{d}");
    }
    let docs = CloneMachine::definition().gate_docs();
    assert!(docs.contains(&("give up", "the attempt policy is exhausted")));
}

#[test]
fn record_logs_expose_no_way_to_rewrite_history() {
    let mut log = RecordLog::new(ObjRef::new("Clone", "c"));
    log.append("a");
    log.append("b");
    // Only append/read APIs exist; positions are stable and dense.
    assert_eq!(log.rows(), &["a", "b"]);
    assert_eq!(log.get(0), Some(&"a"));
    assert_eq!(log.iter().map(|(k, _)| k.position).collect::<Vec<_>>(), vec![0, 1]);
}
