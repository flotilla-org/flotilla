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

fn in_state(state: &'static str) -> impl Fn(&Row) -> bool {
    move |row| text(row, "state") == state
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

// The resource commands refuse a kind the engine serves through a port of its own, naming the
// commands that read or change it, rather than reading or writing the old store.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_resource_commands_refuse_kinds_served_through_their_own_ports() {
    use flotilla_protocol::{CommandAction, CommandValue};
    let daemon = TestDaemon::builder().old_store(OldStoreMode::Recording).start().await;
    daemon.stage("refusals");
    let refusal = |value: CommandValue| match value {
        CommandValue::Error { message } => message,
        other => panic!("not refused: {other:?}"),
    };
    let clone = refusal(
        daemon.execute(CommandAction::QueryResourceGet { namespace: "flotilla".into(), kind: "clones".into(), name: "any".into() }).await,
    );
    assert!(clone.contains("Clone is served by the engine") && clone.contains("flotilla convoy explain"), "{clone}");
    let messages = refusal(
        daemon
            .execute(CommandAction::QueryResourceList { namespace: "flotilla".into(), kind: "Message".into(), include_replicas: true })
            .await,
    );
    assert!(messages.contains("flotilla message inbox"), "{messages}");
    let environment = refusal(
        daemon
            .execute(CommandAction::ResourceDelete {
                namespace: "flotilla".into(),
                kind: "Environment".into(),
                name: "any".into(),
                replica_origin: None,
            })
            .await,
    );
    assert!(environment.contains("Environment is served by the engine"), "{environment}");
    let report = daemon.old_store_report().expect("recorded");
    let refusals = report.split("stage refusals:").nth(1).unwrap_or_default();
    for kind in ["Clone", "Message", "Environment"] {
        assert!(!refusals.contains(&format!("  {kind} ")), "{kind} reached the old store:\n{report}");
    }
}

async fn delivered_convoy(daemon: &TestDaemon) {
    daemon.stage("admission");
    daemon.request_convoy(CrewMix::CoderReviewer, DUMMY_FULFILMENT_KIND).await;
    // Admission: the convoy is Open, holding its crews' promises.
    let open = daemon.wait_row("Open", "convoy", "", in_state("Open")).await;
    let convoy = text(&open, "resource.name").to_owned();
    assert!(!convoy.is_empty(), "the convoy row names its convoy: {open:?}");
    let promises = daemon.rows("convoy_promises", "").await;
    assert!(promises.iter().any(|row| text(row, "resource.name") == convoy), "the convoy's promises: {promises:?}");

    // Placement and provisioning: a vessel per crew, each Realised, with its
    // environment, the coder's checkout and both terminal sessions.
    daemon.stage("vessels");
    let of_convoy = |rows: Vec<Row>| rows.into_iter().filter(names("convoy", &convoy)).collect::<Vec<_>>();
    daemon.wait_row("a vessel Realised", "vessel", "", |row| names("convoy", &convoy)(row) && in_state("Realised")(row)).await;
    let vessels: Vec<String> = of_convoy(daemon.rows("vessel", "").await).iter().map(|row| text(row, "resource.name").to_owned()).collect();
    assert_eq!(vessels.len(), 2, "a vessel per crew: {vessels:?}");
    daemon.stage("environments");
    for vessel in &vessels {
        daemon
            .wait_row("its environment Realised", "environment", "", |row| names("holder", vessel)(row) && in_state("Realised")(row))
            .await;
    }
    daemon.stage("checkout");
    daemon
        .wait_row("the coder's checkout Realised", "checkout", "", |row| names("holder", &convoy)(row) && in_state("Realised")(row))
        .await;
    daemon.stage("terminal sessions");
    for vessel in &vessels {
        daemon
            .wait_row("its terminal session Realised", "terminal_session", "", |row| {
                names("holder", vessel)(row) && in_state("Realised")(row)
            })
            .await;
    }

    // Crew turns and messages: each crew's session takes its turns, and the coder's
    // review request and the reviewer's answer settle in their inboxes.
    daemon.stage("crew turns and messages");
    daemon.wait_row("a live agent session", "agent_session", "", |_| true).await;
    daemon
        .wait_row("a settled message in the convoy's inboxes", "message", "", |row| {
            text(row, "inbox").contains(&format!("convoy:{convoy}")) && in_state("Settled")(row)
        })
        .await;

    // The change request: the dummy forge's, observed on the engine, keeps the coder's promise.
    daemon.stage("change request");
    daemon
        .wait_row("the coder's change request submitted", "convoy_promises_submissions", "", |row| text(row, "resource.name") == convoy)
        .await;
    daemon
        .wait_row("a kept promise", "convoy_promises", "", |row| text(row, "resource.name") == convoy && text(row, "progress") == "Kept")
        .await;

    // Landing: every promise kept, the convoy is Delivered.
    daemon.stage("landing");
    daemon.wait_row("Delivered", "convoy", "", |row| text(row, "resource.name") == convoy && in_state("Delivered")(row)).await;

    // Teardown: everything held for the convoy ends Gone, and the vendor's backings are destroyed.
    daemon.stage("teardown");
    for vessel in &vessels {
        daemon.wait_row("the vessel Gone", "vessel", "", |row| text(row, "resource.name") == vessel && in_state("Gone")(row)).await;
        daemon.wait_row("its environment Gone", "environment", "", |row| names("holder", vessel)(row) && in_state("Gone")(row)).await;
        daemon
            .wait_row("its terminal session Gone", "terminal_session", "", |row| names("holder", vessel)(row) && in_state("Gone")(row))
            .await;
    }
    daemon.wait_row("the coder's checkout Gone", "checkout", "", |row| names("holder", &convoy)(row) && in_state("Gone")(row)).await;
    for table in ["vessel", "environment", "terminal_session", "checkout"] {
        let held = |row: &Row| {
            names("convoy", &convoy)(row) || names("holder", &convoy)(row) || vessels.iter().any(|vessel| names("holder", vessel)(row))
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
