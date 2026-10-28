//! Artifact's stored shape and import: deterministic DDL and a readable view, checked in for the
//! owner's schema look-over, the plans of its statements, and the import of every artifact the
//! previous generation stored.

mod schema_support;

use std::collections::BTreeMap;

use chrono::{DateTime, TimeZone, Utc};
use flotilla_protocol::LeafAddress;
use flotilla_provisioning::{
    engine::{
        artifact_import::{import_artifact, rehearse_artifacts, ArtifactCopy, Imported},
        import::ImportReport,
    },
    stored::artifact as stored,
};
use flotilla_resources::{Artifact, ArtifactSpec, ArtifactStatus, ObjectMeta, ResourceObject};
use flotilla_schema::render_view;
use flotilla_schema_sqlite::{schema_ddl, DdlMode, Store};
use hegel::generators as gs;
use schema_support::{check, plan};

// Behaviour (CLAUDE.md schema review): Artifact renders deterministic DDL, checked in so the
// owner's look-over and every later change show as a diff.
#[test]
fn artifact_ddl_matches_the_snapshot() {
    check("artifact.sql", &schema_ddl(&stored::registry().expect("registry"), DdlMode::Create));
}

// Behaviour (CLAUDE.md schema review): the readable view of Artifact's tables.
#[test]
fn artifact_schema_view_matches_the_snapshot() {
    let view = format!(
        "# Artifact store schema view\n\nKind tables only; the shared tables are in `artifact.sql`.\n\n{}",
        render_view(stored::registry().expect("registry").iter())
    );
    check("artifact_schema_view.md", &view);
}

// Behaviour (#3320 production shape): every statement over Artifact's tables searches a key or a
// declared index, never a scan.
#[test]
fn artifact_statements_search_by_key() {
    let store =
        Store::open(rusqlite::Connection::open_in_memory().expect("memory"), "home", stored::registry().expect("registry")).expect("open");
    let statements = store.statements();
    let conn = store.into_connection();
    let failures: Vec<_> = statements
        .iter()
        .map(|(label, sql)| (label, plan(&conn, sql)))
        .filter(|(label, rows)| schema_support::unkeyed(label, rows))
        .collect();
    assert!(failures.is_empty(), "{failures:#?}");
}

fn object(name: &str, spec: ArtifactSpec) -> ResourceObject<Artifact> {
    let metadata = ObjectMeta {
        name: name.into(),
        namespace: "flotilla".into(),
        resource_version: "1".into(),
        labels: BTreeMap::new(),
        annotations: BTreeMap::new(),
        owner_references: Vec::new(),
        finalizers: Vec::new(),
        deletion_timestamp: None,
        creation_timestamp: DateTime::UNIX_EPOCH,
        merge: None,
    };
    ResourceObject { metadata, spec, status: None }
}

/// The fleet's artifacts at the last corpus refresh.
fn corpus() -> Vec<ResourceObject<Artifact>> {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../flotilla-store-testkit/tests/fixtures/stored-records/r355/Artifact.json");
    let records: Vec<serde_json::Value> = serde_json::from_str(&std::fs::read_to_string(path).expect("corpus")).expect("corpus json");
    records
        .into_iter()
        .enumerate()
        .map(|(index, record)| object(&format!("corpus-{index}"), serde_json::from_value(record["spec"].clone()).expect("corpus spec")))
        .collect()
}

fn spec(summary: BTreeMap<String, serde_json::Value>) -> ArtifactSpec {
    ArtifactSpec::builder()
        .convoy("demo".into())
        .producer("coder".into())
        .kind("decision-ledger".into())
        .subject("demo".into())
        .summary(summary)
        .digest("ef37".into())
        .size(43)
        .media_type("text/markdown".into())
        .expires_at(Utc.timestamp_opt(1_800_000_000, 0).single().expect("time"))
        .build()
}

fn copy(object: ResourceObject<Artifact>, namespace: &str, home: &str) -> ArtifactCopy {
    ArtifactCopy { namespace: namespace.into(), object, home: home.into() }
}

fn micros(time: DateTime<Utc>) -> DateTime<Utc> {
    DateTime::from_timestamp_micros(time.timestamp_micros()).expect("in range")
}

/// A summary value of every shape a put accepts: text, booleans, integers across the `i64` and
/// `u64` boundaries, and finite floats, integral ones among them.
fn summary_value(tc: &hegel::TestCase) -> serde_json::Value {
    match tc.draw(gs::integers::<u8>().min_value(0).max_value(5)) {
        0 => serde_json::Value::String(tc.draw(gs::text().max_size(8))),
        1 => serde_json::Value::Bool(tc.draw(gs::booleans())),
        2 => serde_json::Value::from(tc.draw(gs::sampled_from(vec![i64::MIN, -1, 0, 1, i64::MAX]))),
        3 => serde_json::Value::from(tc.draw(gs::integers::<u64>().min_value(i64::MAX as u64).max_value(u64::MAX))),
        4 => serde_json::Value::from(tc.draw(gs::sampled_from(vec![2.0, -0.5, 1e300, f64::MIN_POSITIVE]))),
        _ => serde_json::Value::from(tc.draw(gs::floats::<f64>().allow_nan(false).allow_infinity(false))),
    }
}

/// A time with sub-microsecond digits, either side of the epoch.
fn time(tc: &hegel::TestCase) -> DateTime<Utc> {
    let seconds = tc.draw(gs::integers::<i64>().min_value(-4_000_000_000).max_value(4_000_000_000));
    Utc.timestamp_opt(seconds, tc.draw(gs::integers::<u32>().min_value(0).max_value(999_999_999))).single().expect("time")
}

// Behaviour (ADR 0047, stored data rule): every artifact the previous generation could store
// imports at its home, restores into the rehearsal store (which reads it back and compares), and
// reads as the spec it was, its times to the microsecond; its reservations, if any, count as dropped.
#[hegel::test]
fn every_storable_artifact_imports_and_reads_back(tc: hegel::TestCase) {
    let entries = tc.draw(gs::integers::<usize>().min_value(0).max_value(3));
    let summary = (0..entries).map(|index| (format!("key-{index}"), summary_value(&tc))).collect();
    let mut old = spec(summary);
    old.convoy = tc.draw(gs::sampled_from(vec![String::new(), "demo".to_owned()]));
    old.size = tc.draw(gs::integers::<u64>().min_value(0).max_value(i64::MAX as u64));
    old.recorded_at = tc.draw(gs::booleans()).then(|| time(&tc));
    old.expires_at = time(&tc);
    old.pinned = tc.draw(gs::booleans());
    let reserved = tc.draw(gs::booleans());
    let mut object = object("slot", old.clone());
    if reserved {
        object.status = Some(ArtifactStatus {
            ledger_comment_creations: vec![LeafAddress::ChangeRequest { service: "github.com".into(), scope: "o/r".into(), number: 1 }],
        });
    }

    let Imported::Record(record) = import_artifact(&object).expect("imports") else { panic!("dropped") };
    let mut expected = old.clone();
    expected.recorded_at = old.recorded_at.map(micros);
    expected.expires_at = micros(old.expires_at);
    assert_eq!(record.spec().expect("reads as a spec"), expected);
    let mut report = ImportReport::default();
    rehearse_artifacts(&[copy(object, "flotilla", "alpha")], &mut report);
    assert!(report.refused.is_empty(), "{report:?}");
    assert_eq!(report.imported.values().sum::<usize>(), 1);
    assert_eq!(report.dropped.values().sum::<usize>(), usize::from(reserved), "{report:?}");
}

// Behaviour (ADR 0047): the corpus's artifacts import; a deleted copy drops; a copy whose summary
// holds a value no put accepts (an array, a null) is refused, named, as is a size above the stored
// integer range (#3320: stored integers never wrap); one name a home holds in two namespaces is
// refused in both (#3212: no namespaces), unless one of them is deleted, while the same name at
// two homes imports at each.
#[test]
fn copies_the_engine_cannot_hold_are_dropped_or_refused() {
    let corpus = corpus();
    assert!(!corpus.is_empty());
    let mut deleted = object("deleted", spec(BTreeMap::new()));
    deleted.metadata.deletion_timestamp = Some(Utc::now());
    let nested = object("nested", spec(BTreeMap::from([("list".to_owned(), serde_json::json!([1]))])));
    let null = object("null", spec(BTreeMap::from([("gone".to_owned(), serde_json::Value::Null)])));
    let mut huge = spec(BTreeMap::new());
    huge.size = i64::MAX as u64 + 1;
    let huge = object("huge", huge);
    let shared = object("shared", spec(BTreeMap::new()));
    let gone = object("gone", spec(BTreeMap::new()));
    let mut gone_elsewhere = gone.clone();
    gone_elsewhere.metadata.deletion_timestamp = Some(Utc::now());
    let mut copies: Vec<_> = corpus.into_iter().map(|object| copy(object, "flotilla", "alpha")).collect();
    let imported = copies.len() + 3;
    copies.extend([
        copy(deleted, "flotilla", "alpha"),
        copy(nested, "flotilla", "alpha"),
        copy(null, "flotilla", "alpha"),
        copy(huge, "flotilla", "alpha"),
        copy(shared.clone(), "flotilla", "alpha"),
        copy(shared.clone(), "flotilla", "beta"),
        copy(shared.clone(), "flotilla", "gamma"),
        copy(shared.clone(), "other", "gamma"),
        copy(gone.clone(), "flotilla", "delta"),
        copy(gone_elsewhere, "other", "delta"),
    ]);
    let mut report = ImportReport::default();
    rehearse_artifacts(&copies, &mut report);
    assert_eq!(report.imported.values().sum::<usize>(), imported, "{report:?}");
    assert_eq!(report.dropped.values().sum::<usize>(), 2, "{report:?}");
    let refused: Vec<_> = report.refused.iter().map(|refusal| (refusal.record.as_str(), refusal.reason.contains("`gamma`"))).collect();
    assert_eq!(refused, [("nested", false), ("null", false), ("huge", false), ("shared", true), ("shared", true)], "{report:?}");
}
