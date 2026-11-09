//! The terminal port (#3108) over a decided store: launches are requested and renewed through it,
//! stepped as the TerminalSession controller steps them, and read back through it with the facts
//! each was realised with.

use flotilla_protocol::LifecycleReading;
use flotilla_provisioning_testkit::TerminalsFixture;
use flotilla_resources::terminal_records::{
    launched, newest_launches, AgentLaunch, TerminalLaunchFacts, TerminalRecord, TerminalRecords, TerminalRenewal,
};

fn facts() -> TerminalLaunchFacts {
    TerminalLaunchFacts {
        endpoint: Some(("/run/cleat".into(), "daemon-1".into())),
        agent: Some(AgentLaunch { adapter: "claude".into(), model: Some("opus".into()), stance: "bypass".into() }),
        build_jobs: Some(4),
    }
}

fn names(records: Vec<TerminalRecord>) -> Vec<String> {
    let mut names: Vec<_> = records.into_iter().map(|record| record.name).collect();
    names.sort();
    names
}

#[test]
fn a_request_is_served_by_its_live_launch_and_an_ended_one_starts_the_next() {
    let fixture = TerminalsFixture::default();
    let request = TerminalsFixture::agent("v1", "lead", "env-a");
    let first = fixture.request(&request);
    assert_eq!(launched(&first), "terminal-v1-lead");
    assert_eq!(fixture.record(&first).reading, LifecycleReading::InProgress);
    assert_eq!(fixture.request(&request), first, "a live launch serves the request");

    // One session is one environment's and one holder's.
    let elsewhere = TerminalsFixture::agent("v1", "lead", "env-b");
    assert!(fixture.terminals.request(elsewhere).is_err());

    fixture.ready(&first, "terminal-v1-lead", &facts());
    fixture.exited(&first, "exit 1");
    let second = fixture.request(&request);
    assert_ne!(second, first);
    assert_eq!(launched(&second), "terminal-v1-lead");
    assert!(fixture.record(&second).launch_number() > fixture.record(&first).launch_number());
    // The session's name reads its newest launch; a launch's name reads that launch.
    assert_eq!(fixture.terminals.get("terminal-v1-lead", None).expect("read").expect("held").name, second);
    assert!(fixture.record(&first).ended());
}

#[test]
fn a_realised_launch_reads_back_with_its_facts_through_each_index() {
    let fixture = TerminalsFixture::default();
    let lead = fixture.request(&TerminalsFixture::agent("v1", "lead", "env-a"));
    let review = fixture.request(&TerminalsFixture::agent("v1", "review", "env-a"));
    let other = fixture.request(&TerminalsFixture::agent("v2", "lead", "env-b"));
    fixture.ready(&lead, "terminal-v1-lead", &facts());

    let record = fixture.record(&lead);
    assert!(record.is_running());
    assert!(record.is_agent());
    assert_eq!(record.session_id(), Some("terminal-v1-lead"));
    assert_eq!(record.facts, Some(facts()));
    assert_eq!(record.vessel.as_deref(), Some("v1"));
    assert_eq!(record.crew_id(), Some(lead.as_str()), "a crew id is its launch name");
    assert!(record.started_at().is_some());
    // Not realised yet: no facts.
    assert_eq!(fixture.record(&review).facts, None);

    assert_eq!(names(fixture.terminals.of_vessel("v1").expect("of vessel")), names(vec![fixture.record(&lead), fixture.record(&review)]));
    assert_eq!(names(fixture.terminals.of_environment("env-b", "home").expect("of environment")), vec![other.clone()]);
    assert_eq!(names(fixture.terminals.at("home").expect("at")).len(), 3);
    assert_eq!(names(fixture.terminals.every().expect("every")).len(), 3);
    // Served through the engine's readings.
    assert!(fixture.reads.terminals().expect("served").get(&other, None).expect("read").is_some());
}

#[test]
fn a_renewal_starts_the_next_launch_only_once_the_newest_ended() {
    let fixture = TerminalsFixture::default();
    let first = fixture.request(&TerminalsFixture::agent("v1", "lead", "env-a"));
    fixture.ready(&first, "terminal-v1-lead", &facts());
    match fixture.terminals.renew("terminal-v1-lead").expect("renew") {
        TerminalRenewal::Live(record) => assert_eq!(record.name, first),
        TerminalRenewal::Started(name) => panic!("a live launch is not renewed: {name}"),
    }

    fixture.release(&first);
    let TerminalRenewal::Started(second) = fixture.terminals.renew("terminal-v1-lead").expect("renew") else {
        panic!("an ended launch is renewed")
    };
    assert_ne!(second, first);
    let renewed = fixture.record(&second);
    assert_eq!(renewed.environment, "env-a");
    assert_eq!(renewed.vessel.as_deref(), Some("v1"));
    assert_eq!(renewed.facts, None, "facts are each launch's own");

    let newest = newest_launches(fixture.terminals.every().expect("every"));
    assert_eq!(newest.len(), 1);
    assert_eq!(newest.values().next().expect("one").name, second);
}

#[test]
fn a_session_never_requested_here_cannot_be_renewed() {
    let fixture = TerminalsFixture::default();
    assert!(fixture.terminals.renew("terminal-v9-lead").is_err());
    assert!(fixture.terminals.get("terminal-v9-lead", None).expect("read").is_none());
}

// Behaviour (#3108): the old store's TerminalSession reads are served from the launches. Each
// session reads as its newest launch, with the engine's identity, phase and ending reason, and
// its writes are refused: sessions are requested and renewed through the terminal port.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn old_store_reads_see_each_sessions_newest_launch() {
    use std::sync::Arc;

    use flotilla_resources::{TerminalSession, TerminalSessionPhase};
    use flotilla_store::{ResourceBackend, TerminalViews};

    let fixture = TerminalsFixture::default();
    let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
    backend.install_engine_records(Arc::new(TerminalViews::new(backend.clone(), "flotilla", fixture.reads.clone())));
    backend.serve_engine_in("flotilla");
    let sessions = backend.using::<TerminalSession>("flotilla");
    let request = TerminalsFixture::agent("v1", "lead", "env-a");
    let read = || async { sessions.get(&request.session).await.expect("session").status.expect("status") };

    let first = fixture.request(&request);
    assert_eq!(read().await.phase, TerminalSessionPhase::Starting);
    fixture.ready(&first, "engine identity", &facts());
    let running = read().await;
    assert_eq!(running.phase, TerminalSessionPhase::Running);
    assert_eq!(running.session_id.as_deref(), Some("engine identity"));
    fixture.exited(&first, "agent exited with code 0");
    assert_eq!(read().await.phase, TerminalSessionPhase::Stopped, "an exited agent waits for its resume");

    let TerminalRenewal::Started(second) = fixture.terminals.renew(&request.session).expect("renew") else { panic!("next launch") };
    assert_eq!(read().await.phase, TerminalSessionPhase::Starting, "the newest launch is read");
    fixture.ready(&second, "s2", &facts());
    fixture.failed(&second, "engine ending reason");
    let failed = read().await;
    assert_eq!(failed.phase, TerminalSessionPhase::Failed);
    assert!(failed.message.expect("reason").contains("engine ending reason"));

    let listed = sessions.list().await.expect("list").items;
    assert_eq!(listed.iter().map(|session| session.metadata.name.as_str()).collect::<Vec<_>>(), [request.session.as_str()]);
    assert!(sessions.delete(&request.session).await.is_err(), "a session ends with its holder; it is not deleted");
}
