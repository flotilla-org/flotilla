use super::*;
use flotilla_provisioning_testkit::TerminalsFixture;
use flotilla_resources::vessel_seats::VesselSeats;
use flotilla_store_memory::InMemoryBackend;

// Behaviour (#3629 layer 8, #3108): a vessel's seats are the newest launch of each session the
// engine holds for it, by role: a relaunch serves its role in place of the launch it follows, and
// another vessel's launches serve none of its seats.
#[tokio::test]
async fn a_vessels_seats_are_the_newest_launches_it_holds() {
    let terminals = TerminalsFixture::default();
    let coder = terminals.request(&TerminalsFixture::agent("vessel-a", "coder", "env-a"));
    terminals.ready(&coder, "s1", &Default::default());
    terminals.exited(&coder, "exit 0");
    let relaunched = terminals.request(&TerminalsFixture::agent("vessel-a", "coder", "env-a"));
    terminals.ready(&relaunched, "s2", &Default::default());
    let reviewer = terminals.request(&TerminalsFixture::agent("vessel-a", "reviewer", "env-a"));
    terminals.request(&TerminalsFixture::agent("vessel-b", "coder", "env-a"));

    let seats = EngineVesselSeats { backend: ResourceBackend::new(InMemoryBackend::default()), reads: terminals.reads.clone() };
    let served: BTreeMap<String, Option<String>> = seats
        .seats(NAMESPACE, "vessel-a")
        .await
        .expect("seats")
        .into_iter()
        .map(|(role, session)| (role, flotilla_resources::terminal_records::launch_of(&session.object).map(str::to_owned)))
        .collect();
    assert_ne!(coder, relaunched);
    assert_eq!(served, BTreeMap::from([("coder".to_string(), Some(relaunched)), ("reviewer".to_string(), Some(reviewer))]));
}
