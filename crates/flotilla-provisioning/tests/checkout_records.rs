//! The checkout port (#3108) over a decided store: checkouts are requested through it, stepped as
//! the Checkout controller steps them, and read back through it.

use flotilla_protocol::LifecycleReading;
use flotilla_provisioning_testkit::{checkouts::CheckoutFixture, Ending};
use flotilla_resources::{
    checkout_records::{launch, CheckoutHolder, CheckoutRecords, Inspection, Landed},
    CheckoutSpec, CheckoutWorktreeSpec, ObservedCheckoutSpec, RepositoryKey,
};

fn repository() -> RepositoryKey {
    RepositoryKey("github.com/owner/repo".into())
}

fn worktree(branch: &str) -> CheckoutSpec {
    CheckoutSpec::Worktree(CheckoutWorktreeSpec {
        repo_ref: repository(),
        env_ref: "host-direct-home".into(),
        r#ref: branch.into(),
        base_ref: Some("main".into()),
        target_path: format!("/work/{branch}"),
        clone_ref: "clone-abc#1".into(),
    })
}

fn convoy(name: &str) -> CheckoutHolder {
    CheckoutHolder::Convoy(name.into())
}

#[test]
fn a_request_is_served_by_its_live_launch_and_an_ended_one_starts_the_next() {
    let fixture = CheckoutFixture::default();
    fixture.open_convoy("c1");
    let first = fixture.request("co", &worktree("feature"), Some(&convoy("c1")));
    assert_eq!(first, launch("co", 1));
    assert_eq!(fixture.record(&first).reading, LifecycleReading::InProgress);
    assert_eq!(fixture.request("co", &worktree("feature"), Some(&convoy("c1"))), first);
    // One name is one checkout: a live launch requested otherwise is refused.
    assert!(fixture.checkouts.request("co", &worktree("other"), Some(&convoy("c1"))).is_err());
    // A launch's own name is no checkout's.
    assert!(fixture.checkouts.request("co#1", &worktree("feature"), None).is_err());

    fixture.finish(&first, Ending::Lost("gone"));
    let second = fixture.request("co", &worktree("feature"), Some(&convoy("c1")));
    assert_eq!(second, launch("co", 2));
    // The checkout's name reads its latest launch; a launch's name reads that launch.
    assert_eq!(fixture.checkouts.get("co", None).expect("read").expect("held").name, second);
    assert!(fixture.record(&first).ended());
}

#[test]
fn a_realised_checkout_reads_back_with_its_facts_through_each_index() {
    let fixture = CheckoutFixture::default();
    fixture.open_convoy("c1");
    let held = fixture.request("co", &worktree("feature"), Some(&convoy("c1")));
    fixture.ready(&held, Some("objects not shared"));
    fixture.open_convoy("c3");
    fixture.put_ready("standing", &worktree("standing"), Some(&convoy("c3")));

    let record = fixture.record(&held);
    assert!(record.is_ready());
    assert_eq!(record.spec, worktree("feature"));
    assert_eq!(record.holder, Some(convoy("c1")));
    assert_eq!(record.degraded.as_deref(), Some("objects not shared"));
    assert_eq!(record.path(), Some("/work/feature"));
    assert_eq!(record.integration, None);

    let names = |records: Vec<flotilla_resources::checkout_records::CheckoutRecord>| {
        let mut names: Vec<_> = records.into_iter().map(|record| record.name).collect();
        names.sort();
        names
    };
    assert_eq!(names(fixture.checkouts.held(&convoy("c1")).expect("held")), vec![held.clone()]);
    assert_eq!(names(fixture.checkouts.held(&convoy("c3")).expect("held")), vec!["standing#1".to_owned()]);
    assert_eq!(names(fixture.checkouts.held(&convoy("c2")).expect("held")), Vec::<String>::new());
    assert_eq!(names(fixture.checkouts.of_repository(&repository()).expect("of")), vec![held.clone(), "standing#1".into()]);
    assert_eq!(names(fixture.checkouts.in_environment("host-direct-home", None).expect("in")), vec![held.clone(), "standing#1".into()]);
    assert_eq!(names(fixture.checkouts.here().expect("here")), vec![held, "standing#1".into()]);
    // Served through the engine's readings.
    assert!(fixture.reads.checkouts().expect("served").get("standing", None).expect("read").is_some());
}

#[test]
fn an_adopted_checkout_is_its_hosts_and_reads_back_observed() {
    let fixture = CheckoutFixture::default();
    fixture.open_convoy("c1");
    let spec = CheckoutSpec::Observed(ObservedCheckoutSpec {
        r#ref: "main".into(),
        path: "/src/repo".into(),
        repo_ref: repository(),
        host_ref: "home".into(),
        is_main: true,
    });
    let adopted = fixture.put_ready("adopted", &spec, Some(&convoy("c1")));
    let record = fixture.record(&adopted);
    assert_eq!(record.spec, spec);
    assert_eq!(record.path(), Some("/src/repo"));
    assert_eq!(fixture.checkouts.in_environment("host-direct-home", None).expect("in").len(), 1);
}

#[test]
fn an_inspection_is_written_only_when_it_changes_and_a_landing_latches() {
    let fixture = CheckoutFixture::default();
    let held = fixture.put_ready("co", &worktree("feature"), None);
    let landed = Inspection {
        head_revision: Some("abc".into()),
        clean: Some(true),
        pushed: Some(true),
        landed: Some(true),
        landed_details: vec!["merged as #7".into()],
        change_request: Some(7),
        landed_evidence: Some(Landed { change_request: 7, merged_at: None, target_ref: Some("main".into()), head_in_merged_head: true }),
        remote_digest: Some("abc".into()),
        ..Inspection::default()
    };
    assert!(fixture.checkouts.inspected(&held, landed.clone()).expect("written"));
    assert!(!fixture.checkouts.inspected(&held, landed.clone()).expect("unchanged"));
    let integration = fixture.record(&held).integration.expect("inspected");
    assert_eq!(integration.inspection, landed);
    assert_eq!(integration.failing_since, None);

    // An inspection that cannot judge the landing keeps the held evidence.
    let unjudged = Inspection { landed: None, landed_details: vec![], landed_evidence: None, change_request: None, ..landed.clone() };
    assert!(!fixture.checkouts.inspected(&held, unjudged).expect("latched"));
    assert_eq!(fixture.record(&held).integration.expect("inspected").inspection, landed);

    // A judged change is written.
    let dirty = Inspection { clean: Some(false), clean_details: vec!["modified: a.rs".into()], ..landed.clone() };
    assert!(fixture.checkouts.inspected(&held, dirty.clone()).expect("written"));
    assert_eq!(fixture.record(&held).integration.expect("inspected").inspection, dirty);
}

#[test]
fn a_failing_inspection_keeps_its_values_confirmed_when_it_started_to_fail() {
    let fixture = CheckoutFixture::default();
    let held = fixture.put_ready("co", &worktree("feature"), None);
    let clean = Inspection { clean: Some(true), ..Inspection::default() };
    fixture.checkouts.inspected(&held, clean.clone()).expect("written");

    fixture.checkouts.inspection_failed(&held).expect("failed");
    let failing = fixture.record(&held).integration.expect("inspected");
    let since = failing.failing_since.expect("failing");
    assert_eq!(failing.confirmed_at, since);
    assert_eq!(failing.inspection, clean);
    // A second failure keeps when it started.
    fixture.checkouts.inspection_failed(&held).expect("failed");
    assert_eq!(fixture.record(&held).integration.expect("inspected").failing_since, Some(since));

    // A success clears it: this home's values are confirmed now.
    assert!(fixture.checkouts.inspected(&held, clean).expect("written"));
    let confirmed = fixture.record(&held).integration.expect("inspected");
    assert_eq!(confirmed.failing_since, None);
    assert!(confirmed.confirmed_at >= since);

    // A checkout never inspected that fails records only since when.
    let other = fixture.put_ready("other", &worktree("other"), None);
    fixture.checkouts.inspection_failed(&other).expect("failed");
    let failing = fixture.record(&other).integration.expect("recorded");
    assert_eq!(failing.inspection, Inspection::default());
    assert!(failing.failing_since.is_some());
    // Only this home's held launches are inspected.
    assert!(fixture.checkouts.inspected("absent#1", Inspection::default()).is_err());
}
