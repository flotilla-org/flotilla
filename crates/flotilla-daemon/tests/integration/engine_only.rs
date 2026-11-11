//! The fleet on the engine store alone (#3108, ADR 0064): a daemon composed with
//! no old resource store runs an ordinary convoy from admission to delivery, and
//! every fact it reads or writes on the way is an engine record. Any access to the
//! old store fails the scenario with where the access came from.

use flotilla_daemon_testkit::daemon::{OldStoreMode, TestDaemon};
use flotilla_dummy_fleet::catalog::{CrewMix, DUMMY_FULFILMENT_KIND};
use flotilla_protocol::table::Row;

fn text<'a>(row: &'a Row, column: &str) -> &'a str {
    row.get(column).and_then(serde_json::Value::as_str).unwrap_or_default()
}

/// A row whose `column` names `name`: a reference to it, or an address ending in it.
fn names<'a>(column: &'a str, name: &'a str) -> impl Fn(&Row) -> bool + 'a {
    move |row| {
        let value = text(row, column);
        value == name || value.ends_with(&format!(":{name}")) || value.ends_with(&format!("/{name}"))
    }
}

// One coder and its reviewer, each in its own vessel: admission opens the convoy,
// placement leases a vessel per crew, each vessel's environment is realised, the
// coder's checkout and both crews' terminal sessions are realised; the crews take
// turns and exchange messages; the coder's change request on the dummy forge keeps
// its promise; the convoy is delivered, and everything held for it ends Gone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn on_the_engine_alone_a_convoy_is_delivered_and_its_resources_end_gone() {
    let daemon = TestDaemon::builder().old_store(OldStoreMode::Unavailable).start().await;
    delivered_convoy(&daemon).await;
    daemon.assert_old_store_untouched();
}

// The same convoy over a recording old store: lists, by stage, every access the
// scenario still makes there, and where each first came from.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "a census of the old store's remaining accesses, run by hand"]
async fn census_of_old_store_accesses_on_the_way_to_delivery() {
    let daemon = TestDaemon::builder().old_store(OldStoreMode::Recording).start().await;
    let outcome = futures::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(delivered_convoy(&daemon))).await;
    println!("{}", daemon.old_store_report().expect("recorded"));
    if let Err(failure) = outcome {
        std::panic::resume_unwind(failure);
    }
}

// `flotilla apply` of an AccountProvision writes the engine's record, and the provision
// controller wakes on that change alone: its place is no environment, so it removes it. The old
// store is recorded rather than refused, because other controllers still reach it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_applied_account_provision_wakes_its_controller_on_the_engine() {
    use flotilla_resources::account_access::{ProvisionPlan, UpdateMode};
    let daemon = TestDaemon::builder().old_store(OldStoreMode::Recording).start().await;
    daemon.stage("provision");
    let spec = flotilla_resources::AccountProvisionSpec {
        binding: flotilla_schema::Ref::new("absent-binding".to_owned(), "elsewhere".to_owned()),
        place: "no-such-environment".into(),
        environment_kind: "docker".into(),
        plan: ProvisionPlan {
            implementation: "claude-direct".into(),
            effective_access: "account".into(),
            mechanism: "oauth-token".into(),
            update_mode: UpdateMode::Push,
        },
        home: "/home/agent/.claude".into(),
        delivered: None,
    };
    daemon.apply::<flotilla_resources::AccountProvision>("applied", spec).await;
    daemon.wait_gone::<flotilla_resources::AccountProvision>("applied").await;
    // Other controllers still reach the old store meanwhile; no access is of an account kind.
    let report = daemon.old_store_report().expect("recorded");
    assert!(!report.contains("Account"), "the provision reached the old store:\n{report}");
}

// The resource commands read a crew's clones and environments from the engine. The provisioning
// reconcilers alone write them, so a delete is refused, naming the command that shows them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_resource_commands_read_clones_and_environments_from_the_engine() {
    use flotilla_protocol::{CommandAction, CommandValue};
    use flotilla_resources::{Clone, Environment};
    let daemon = TestDaemon::builder().old_store(OldStoreMode::Recording).start().await;
    daemon.dispatch(CrewMix::Coder, DUMMY_FULFILMENT_KIND).await;
    daemon.stage("lifecycle");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    let (clones, environments) = loop {
        let (clones, environments) = (daemon.list::<Clone>().await, daemon.list::<Environment>().await);
        if !clones.is_empty() && !environments.is_empty() {
            break (clones, environments);
        }
        assert!(tokio::time::Instant::now() < deadline, "the crew's clone and environment are listed: {clones:?} {environments:?}");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    };
    let clone = daemon.get::<Clone>(&clones[0].metadata.name).await.expect("the listed clone reads by name");
    assert_eq!(clone.spec, clones[0].spec);
    let environment = daemon.get::<Environment>(&environments[0].metadata.name).await.expect("the listed environment reads by name");
    assert_eq!(environment.metadata.name, environments[0].metadata.name);
    let deleted = daemon
        .execute(CommandAction::ResourceDelete {
            namespace: "flotilla".into(),
            kind: "Environment".into(),
            name: environment.metadata.name.clone(),
            replica_origin: None,
        })
        .await;
    let CommandValue::Error { message } = deleted else { panic!("an environment delete is refused: {deleted:?}") };
    assert!(message.contains("flotilla convoy explain"), "{message}");
    let report = daemon.old_store_report().expect("recorded");
    let lifecycle = report.split("stage lifecycle:").nth(1).unwrap_or_default();
    for kind in ["Clone", "Environment"] {
        assert!(!lifecycle.contains(&format!("  {kind} ")), "{kind} reached the old store:\n{report}");
    }
}

// The resource commands read a crew's messages from the engine. A message's state moves only
// through its machine, so a delete is refused, naming the command that does change it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_resource_commands_read_messages_from_the_engine() {
    use flotilla_protocol::{CommandAction, CommandValue};
    use flotilla_resources::Message;
    let daemon = TestDaemon::builder().old_store(OldStoreMode::Recording).start().await;
    daemon.dispatch(CrewMix::CoderReviewer, DUMMY_FULFILMENT_KIND).await;
    daemon.stage("messages");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    let listed = loop {
        let listed = daemon.list::<Message>().await;
        if !listed.is_empty() {
            break listed;
        }
        assert!(tokio::time::Instant::now() < deadline, "no message listed for the dispatched crew");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    };
    let name = listed[0].metadata.name.clone();
    let got = daemon.get::<Message>(&name).await.expect("the listed message reads by name");
    assert_eq!(got.spec, listed[0].spec);
    let deleted = daemon
        .execute(CommandAction::ResourceDelete { namespace: "flotilla".into(), kind: "Message".into(), name, replica_origin: None })
        .await;
    let CommandValue::Error { message } = deleted else { panic!("a message delete is refused: {deleted:?}") };
    assert!(message.contains("flotilla message acknowledge"), "{message}");
    // Messages reach other homes by log following, so no digest is kept of them.
    let digest = daemon
        .execute(CommandAction::QueryResourceDigest {
            namespace: "flotilla".into(),
            kind: "Message".into(),
            query: flotilla_protocol::ResourceDigestQuery::Root,
        })
        .await;
    let CommandValue::Error { message } = digest else { panic!("a message digest is refused: {digest:?}") };
    assert!(message.contains("log following"), "{message}");
    let report = daemon.old_store_report().expect("recorded");
    let messages = report.split("stage messages:").nth(1).unwrap_or_default();
    assert!(!messages.contains("  Message "), "Message reached the old store:\n{report}");
}

// A home's messages reach a peer by following its log: listed there through the resource
// commands, with the old store's replication never consulted for them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn messages_reach_a_peer_by_log_following() {
    let (a, b) = tokio::join!(
        TestDaemon::builder().settings("host_name = \"engine-a\"\n").old_store(OldStoreMode::Recording).start(),
        TestDaemon::builder().settings("host_name = \"engine-b\"\n").old_store(OldStoreMode::Recording).start()
    );
    a.dispatch(CrewMix::CoderReviewer, DUMMY_FULFILMENT_KIND).await;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    let sent = loop {
        let sent = message_names(&a).await;
        if !sent.is_empty() {
            break sent;
        }
        assert!(tokio::time::Instant::now() < deadline, "no message listed at its home");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    };
    a.stage("peered");
    b.stage("peered");
    let _peering = TestDaemon::peer(&a, &b).await;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let missing: Vec<_> = sent.difference(&message_names(&b).await).cloned().collect();
        if missing.is_empty() {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            let got = b
                .execute(flotilla_protocol::CommandAction::QueryResourceGet {
                    namespace: "flotilla".into(),
                    kind: "Message".into(),
                    name: missing[0].clone(),
                })
                .await;
            panic!("messages not listed at the peer: {missing:?}; get: {got:?}");
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    // A host role's message (A's forge observer's) lists at both homes with its receiver as the
    // engine stores it: no convoy, vessel or project names a host role.
    let receivers = |listed: Vec<flotilla_resources::ResourceObject<flotilla_resources::Message>>| {
        listed.into_iter().map(|message| (message.metadata.name, message.spec.receiver)).collect::<std::collections::BTreeMap<_, _>>()
    };
    let (at_a, at_b) = (receivers(a.list().await), receivers(b.list().await));
    let host_role = at_a
        .iter()
        .find(|(_, receiver)| receiver.starts_with("host:") && receiver.contains("/role:"))
        .unwrap_or_else(|| panic!("a message to a host role: {at_a:?}"));
    assert_eq!(at_b.get(host_role.0), Some(host_role.1), "the host role's message at the peer");
    for daemon in [&a, &b] {
        let report = daemon.old_store_report().expect("recorded");
        let peered = report.split("stage peered:").nth(1).unwrap_or_default();
        assert!(!peered.contains("  Message "), "Message reached the old store:\n{report}");
    }
}

/// The names of the Messages `daemon` lists.
async fn message_names(daemon: &TestDaemon) -> std::collections::BTreeSet<String> {
    daemon.list::<flotilla_resources::Message>().await.into_iter().map(|message| message.metadata.name).collect()
}

// Each stage waits for a state its record reached, read from the kind's log: the dummy fleet
// moves records on between two reads, so the current state may already be past it.
async fn delivered_convoy(daemon: &TestDaemon) {
    daemon.stage("admission");
    daemon.request_convoy(CrewMix::CoderReviewer, DUMMY_FULFILMENT_KIND).await;
    // Admission: the convoy is Open, holding its crews' promises.
    let open = daemon.wait_reached("Open", "convoy", "Open", |_| true).await;
    // The engine convoy is a launch of the admitted convoy's name: vessels name the convoy, its
    // launch holds the checkouts and its promises.
    let launch = text(&open, "resource.name").to_owned();
    let convoy = text(&open, "launch_of").to_owned();
    assert!(!convoy.is_empty() && launch.starts_with(&format!("{convoy}#")), "the convoy row names its launch: {open:?}");
    let promises = daemon.rows("convoy_promises", "").await;
    assert!(promises.iter().any(|row| text(row, "resource.name") == launch), "the convoy's promises: {promises:?}");

    // Placement and provisioning: a vessel per crew, each Realised, with its
    // environment, the coder's checkout and both terminal sessions.
    daemon.stage("vessels");
    let of_convoy = |rows: Vec<Row>| rows.into_iter().filter(names("convoy", &convoy)).collect::<Vec<_>>();
    daemon.wait_reached("a vessel Realised", "vessel", "Realised", names("convoy", &convoy)).await;
    let vessels: Vec<String> = of_convoy(daemon.rows("vessel", "").await).iter().map(|row| text(row, "resource.name").to_owned()).collect();
    assert_eq!(vessels.len(), 2, "a vessel per crew: {vessels:?}");
    daemon.stage("environments");
    for vessel in &vessels {
        daemon.wait_reached("its environment Realised", "environment", "Realised", names("holder", vessel)).await;
    }
    daemon.stage("checkout");
    daemon.wait_reached("the coder's checkout Realised", "checkout", "Realised", names("holder", &launch)).await;
    daemon.stage("terminal sessions");
    for vessel in &vessels {
        daemon.wait_reached("its terminal session Realised", "terminal_session", "Realised", names("holder", vessel)).await;
    }

    // Crew turns and messages: each crew's session takes its turns, and the coder's
    // review request and the reviewer's answer settle in their inboxes.
    daemon.stage("crew turns and messages");
    daemon.wait_row("a live agent session", "agent_session", "", |_| true).await;
    daemon
        .wait_reached("a settled message in the convoy's inboxes", "message", "Settled", |row| {
            text(row, "inbox").contains(&format!("convoy:{convoy}"))
        })
        .await;

    // The change request: the dummy forge's, observed on the engine, keeps the coder's promise.
    daemon.stage("change request");
    daemon
        .wait_row("the coder's change request submitted", "convoy_promises_submissions", "", |row| text(row, "resource.name") == launch)
        .await;
    daemon
        .wait_row("a kept promise", "convoy_promises", "", |row| text(row, "resource.name") == launch && text(row, "progress") == "Kept")
        .await;

    // Landing: every promise kept, the convoy is Delivered.
    daemon.stage("landing");
    daemon.wait_reached("Delivered", "convoy", "Delivered", |row| text(row, "resource.name") == launch).await;

    // Teardown: everything held for the convoy ends Gone, and the vendor's backings are destroyed.
    daemon.stage("teardown");
    for vessel in &vessels {
        daemon.wait_reached("the vessel Gone", "vessel", "Gone", |row| text(row, "resource.name") == vessel).await;
        daemon.wait_reached("its environment Gone", "environment", "Gone", names("holder", vessel)).await;
        daemon.wait_reached("its terminal session Gone", "terminal_session", "Gone", names("holder", vessel)).await;
    }
    daemon.wait_reached("the coder's checkout Gone", "checkout", "Gone", names("holder", &launch)).await;
    for table in ["vessel", "environment", "terminal_session", "checkout"] {
        let held = |row: &Row| {
            names("convoy", &convoy)(row) || names("holder", &launch)(row) || vessels.iter().any(|vessel| names("holder", vessel)(row))
        };
        let live: Vec<Row> = daemon
            .rows(table, "")
            .await
            .into_iter()
            .filter(|row| matches!(text(row, "state"), "Requested" | "Realised"))
            .filter(held)
            .collect();
        assert!(live.is_empty(), "{table} still live for {convoy}: {live:?}");
    }
    let counts = daemon.counts();
    assert_eq!((counts.environments_provisioned, counts.environments_destroyed, counts.live_environments), (2, 2, 0), "{counts:?}");
    assert_eq!((counts.change_requests_opened, counts.crews_failed, counts.command_errors), (1, 0, 0), "{counts:?}");
}

// The resource commands read vessels from the engine's launches and refuse to write them, naming
// where a vessel is seen; neither reaches the old store.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_resource_commands_read_vessels_from_the_engine_and_refuse_writes() {
    use flotilla_protocol::{CommandAction, CommandValue};
    let daemon = TestDaemon::builder().old_store(OldStoreMode::Recording).start().await;
    daemon.stage("vessels");
    let listed = daemon
        .execute(CommandAction::QueryResourceList { namespace: "flotilla".into(), kind: "vessels".into(), include_replicas: true })
        .await;
    assert!(!matches!(listed, CommandValue::Error { .. }), "listed: {listed:?}");
    let deleted = daemon
        .execute(CommandAction::ResourceDelete {
            namespace: "flotilla".into(),
            kind: "Vessel".into(),
            name: "any#1".into(),
            replica_origin: None,
        })
        .await;
    match deleted {
        CommandValue::Error { message } => assert!(message.contains("flotilla convoy explain"), "{message}"),
        other => panic!("not refused: {other:?}"),
    }
    let report = daemon.old_store_report().expect("recorded");
    let vessels = report.split("stage vessels:").nth(1).unwrap_or_default();
    assert!(!vessels.contains("  Vessel "), "Vessel reached the old store:\n{report}");
}

// A home's vessel and terminal session launches reach a peer by following its log: listed there
// through the resource commands, never through the old store.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn vessels_and_terminal_sessions_reach_a_peer_by_log_following() {
    use flotilla_resources::{TerminalSession, Vessel};
    let (a, b) = tokio::join!(
        TestDaemon::builder().settings("host_name = \"engine-a\"\n").old_store(OldStoreMode::Recording).start(),
        TestDaemon::builder().settings("host_name = \"engine-b\"\n").old_store(OldStoreMode::Recording).start()
    );
    a.dispatch(CrewMix::CoderReviewer, DUMMY_FULFILMENT_KIND).await;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    let (vessels, sessions) = loop {
        let vessels = names_of(a.list::<Vessel>().await);
        let sessions = names_of(a.list::<TerminalSession>().await);
        if !vessels.is_empty() && !sessions.is_empty() {
            break (vessels, sessions);
        }
        assert!(tokio::time::Instant::now() < deadline, "no vessel and session listed at their home");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    };
    a.stage("peered");
    b.stage("peered");
    let _peering = TestDaemon::peer(&a, &b).await;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let missing_vessels: Vec<_> = vessels.difference(&names_of(b.list::<Vessel>().await)).cloned().collect();
        let missing_sessions: Vec<_> = sessions.difference(&names_of(b.list::<TerminalSession>().await)).cloned().collect();
        if missing_vessels.is_empty() && missing_sessions.is_empty() {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "not listed at the peer: vessels {missing_vessels:?}, sessions {missing_sessions:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    for daemon in [&a, &b] {
        let report = daemon.old_store_report().expect("recorded");
        let peered = report.split("stage peered:").nth(1).unwrap_or_default();
        assert!(!peered.contains("  Vessel ") && !peered.contains("  TerminalSession "), "reached the old store:\n{report}");
    }
}

fn names_of<T: flotilla_resources::Resource>(objects: Vec<flotilla_resources::ResourceObject<T>>) -> std::collections::BTreeSet<String> {
    objects.into_iter().map(|object| object.metadata.name).collect()
}
