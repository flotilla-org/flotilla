//! What an AgentSession is created with and how its row is addressed (#4438): a crew launch's
//! session works for the role the launch runs, in the convoy of the vessel the launch is held for,
//! and is addressed under that role of that convoy.

use flotilla_address::{record_name, Address, Kind};
use flotilla_provisioning_testkit::{EngineFixture, TerminalsFixture};
use flotilla_resources::terminal_records::{TerminalLaunchRequest, TerminalLaunchSource};
use flotilla_schema::Ref;
use serde_json::json;

// Behaviour: a crew launch's session request names the launch's role, its vessel at the vessel's
// home, and that vessel's convoy at the convoy's home. A launch held for no vessel has no convoy
// to work for, so it gets no request; nor does a launch not held here.
#[test]
fn a_launchs_session_works_for_its_vessels_convoy() {
    let engine = EngineFixture::default();
    let vessel = engine.vessels.request("convoy-a#7", "work", "home");
    let launch = engine.terminals.request(&TerminalsFixture::agent(&vessel, "coder", "env-a"));
    let request = flotilla_engine_messages::session_request(engine.reads().as_ref(), &launch).expect("a crew launch's session");
    assert_eq!(request.convoy, Ref::new("convoy-a#7", "home"));
    assert_eq!(request.vessel, Some(Ref::new(vessel, "home")));
    assert_eq!(request.role, "coder");

    let tool = TerminalLaunchRequest {
        session: "terminal-shell".into(),
        vessel: None,
        source: TerminalLaunchSource::Tool { command: "bash".into() },
        ..TerminalsFixture::agent("none", "shell", "env-a")
    };
    let shell = engine.terminals.request(&tool);
    assert!(flotilla_engine_messages::session_request(engine.reads().as_ref(), &shell).is_err(), "a tool works for no convoy");
    assert!(flotilla_engine_messages::session_request(engine.reads().as_ref(), "terminal-elsewhere#1").is_err());
}

// Behaviour: the session subject table is `agent_session`, whose row is addressed as the session
// of its role in its convoy, named at the convoy's home; the address names the row's record, so
// it resolves to that row. A row an earlier build wrote names no convoy and has no address.
#[test]
fn a_session_row_is_addressed_under_its_role_in_its_convoy() {
    let mut registry = flotilla_verbs::VerbRegistry::new();
    flotilla_engine_messages::register_subjects(&mut registry).expect("registered once");
    let [subject] = registry.subjects() else { panic!("one subject table") };
    assert_eq!((subject.kind, subject.table), (Kind::Session, "agent_session"));
    let row = json!({"resource.home": "home", "resource.name": "terminal-v-coder#9", "convoy": "convoy-a#7", "convoy.home": "beta", "role": "coder"});
    let address: Address = (subject.address)(&row).expect("addressed");
    assert_eq!(address.kind(), Kind::Session);
    assert_eq!(record_name(&address).as_deref(), Some("terminal-v-coder#9"));
    assert_eq!(
        (address.get(Kind::Host), address.get(Kind::Convoy), address.get(Kind::Role)),
        (Some("beta"), Some("convoy-a#7"), Some("coder"))
    );
    let earlier =
        json!({"resource.home": "home", "resource.name": "terminal-v-coder#3", "convoy": null, "convoy.home": null, "role": null});
    assert_eq!((subject.address)(&earlier), None);
}
