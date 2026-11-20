//! ManifestRoot's stored shape (#3824 layer 2c): deterministic DDL and a readable view, checked in
//! for the owner's schema look-over, the plans of its statements, and a run of every edge through
//! the store's log, read back.

mod schema_support;

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use flotilla_engine_store::StoreLog;
use flotilla_provisioning::stored::manifest_root::{
    self as stored, Act, Applied, ManifestRoot, Outcome, Rebind, RequestResolution, Resolve, Retire, RootRequest, SourceFailed, Suspend,
    Unsuspend,
};
use flotilla_resources::CharterSource;
use flotilla_schema::{render_view, Kind, Timestamp};
use flotilla_schema_sqlite::{schema_ddl, DdlMode, Store, StoreHandle};
use flotilla_state_machines::{Home, Resource};
use schema_support::{check, plan};

fn open() -> Store {
    Store::open(rusqlite::Connection::open_in_memory().expect("memory"), "home", stored::registry().expect("registry")).expect("open")
}

// Behaviour (CLAUDE.md schema review): ManifestRoot renders deterministic DDL, checked in so the
// owner's look-over and every later change show as a diff.
#[test]
fn manifest_root_ddl_matches_the_snapshot() {
    check("manifest_root.sql", &schema_ddl(&stored::registry().expect("registry"), DdlMode::Create));
}

// Behaviour (CLAUDE.md schema review): the readable view of ManifestRoot's tables.
#[test]
fn manifest_root_schema_view_matches_the_snapshot() {
    let view = format!(
        "# ManifestRoot store schema view\n\nKind tables only; the shared tables are in `manifest_root.sql`.\n\n{}",
        render_view(stored::registry().expect("registry").iter())
    );
    check("manifest_root_schema_view.md", &view);
}

// Behaviour (#3320 production shape, #3392 A1): every statement over ManifestRoot's tables searches
// a key or a declared index, never a scan.
#[test]
fn manifest_root_statements_search_by_key() {
    let store = open();
    let statements = store.statements();
    let conn = store.into_connection();
    let failures: Vec<_> = statements
        .iter()
        .map(|(label, sql)| (label, plan(&conn, sql)))
        .filter(|(label, rows)| schema_support::unkeyed(label, rows))
        .collect();
    assert!(failures.is_empty(), "{failures:#?}");
}

// Behaviour (ADR 0064 storage): every edge's writes reach the store. After each edge, the root the
// store holds (its row, its collections and its state data) reads back as the root the home holds.
#[test]
fn every_edge_of_a_root_reads_back_from_the_store() {
    let store = Arc::new(StoreHandle::new(open()));
    let mut home = Home::<ManifestRoot, StoreLog>::with_log("home", StoreLog::new(Arc::clone(&store))).expect("declaration");
    let at = |seconds: i64| Timestamp::from_micros(seconds * 1_000_000);
    let repository = CharterSource::Repository { repo: "project-map".into(), branch: "main".into(), path: String::new() };
    let request = RootRequest { host: "feta".into(), path: "/manifests".into(), source: "project-map".into(), binding: Some(repository) };
    home.create("root", request, &(), at(1)).expect("declared");
    let docs = |names: &[&str]| names.iter().map(|name| name.to_string()).collect::<BTreeSet<_>>();
    let hashes = |pairs: &[(&str, &str)]| pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect::<BTreeMap<_, _>>();
    let read_back = |home: &Home<ManifestRoot, StoreLog>| {
        let mut store = store.lock().expect("store");
        let mut read = store.read().expect("read");
        let id = read.resolve(Resource::<ManifestRoot>::CODE, "root", "home").expect("resolve").expect("stored");
        let loaded = read.load::<Resource<ManifestRoot>>(id).expect("load").expect("row");
        assert_eq!(&loaded.value.with_identity(&loaded.name, loaded.version), home.get("root").expect("held"));
    };
    read_back(&home);
    let step = |home: &mut Home<ManifestRoot, StoreLog>, seconds: i64, apply: &dyn Fn(&mut Home<ManifestRoot, StoreLog>, Timestamp)| {
        apply(home, at(seconds));
        read_back(home);
    };
    step(&mut home, 2, &|home, at| {
        let pass = Applied {
            revision: "r1".into(),
            complete: true,
            reached: docs(&["a.yaml", "b.yaml"]),
            baselines: hashes(&[("a.yaml", "ha")]),
            refusals: hashes(&[("b.yaml", "unmanaged object")]),
            removals: BTreeMap::new(),
        };
        home.apply("root", pass, &(), at).expect("applied");
    });
    step(&mut home, 3, &|home, at| {
        home.apply("root", Suspend { document: "a.yaml".into() }, &(), at).expect("suspended");
    });
    step(&mut home, 4, &|home, at| {
        let request = RequestResolution { document: "b.yaml".into(), act: Act::Adopt, token: "t".into(), by: "operator".into() };
        home.apply("root", request, &(), at).expect("requested");
    });
    step(&mut home, 5, &|home, at| {
        home.apply("root", Resolve { document: "b.yaml".into(), token: "t".into(), outcome: Outcome::Started }, &(), at).expect("claimed");
    });
    step(&mut home, 6, &|home, at| {
        let failed = Outcome::Failed { reason: "live spec changed while adopting".into() };
        home.apply("root", Resolve { document: "b.yaml".into(), token: "t".into(), outcome: failed }, &(), at).expect("settled");
    });
    step(&mut home, 7, &|home, at| {
        home.apply("root", SourceFailed { error: "fetch failed".into() }, &(), at).expect("failed");
    });
    step(&mut home, 8, &|home, at| {
        home.apply("root", Unsuspend { document: "a.yaml".into() }, &(), at).expect("unsuspended");
    });
    step(&mut home, 9, &|home, at| {
        home.apply("root", Rebind { binding: None }, &(), at).expect("rebound");
    });
    step(&mut home, 10, &|home, at| {
        let pass = Applied {
            revision: "r2".into(),
            complete: true,
            reached: docs(&["a.yaml", "b.yaml"]),
            baselines: hashes(&[("a.yaml", "ha2")]),
            refusals: BTreeMap::new(),
            removals: hashes(&[("b.yaml", "r2")]),
        };
        home.apply("root", pass, &(), at).expect("removed");
    });
    step(&mut home, 11, &|home, at| {
        home.apply("root", Retire {}, &(), at).expect("retired");
    });
}
