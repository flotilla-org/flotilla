//! The access layer's per-access status (#3964): AccessDelivery. Deterministic DDL and
//! a readable view, checked in for the owner's schema look-over, the code it would hold
//! beside the production and account access kinds, and the plans of its statements.

mod schema_support;

use flotilla_engine_store::ImportTarget;
use flotilla_provisioning::{
    engine::{
        access_delivery_import::{import_access_deliveries, AccessDeliveryCopy},
        import::ImportReport,
    },
    stored,
};
use flotilla_resources::{access_delivery, AccessDeliverySpec};
use flotilla_schema::{render_view, KindCode};
use flotilla_schema_sqlite::{schema_ddl, DdlMode, Store};
use schema_support::{check, plan};

// Behaviour (CLAUDE.md schema review): the access delivery kind renders deterministic DDL,
// checked in so the owner's look-over and every later change show as a diff.
#[test]
fn access_delivery_store_ddl_matches_the_snapshot() {
    check("access_delivery.sql", &schema_ddl(&access_delivery::registry().expect("registry"), DdlMode::Create));
}

// Behaviour (CLAUDE.md schema review): the readable view of the access delivery kind tables.
#[test]
fn access_delivery_store_schema_view_matches_the_snapshot() {
    let view = format!(
        "# Access delivery store schema view\n\nKind tables only; the shared tables are in `access_delivery.sql`.\n\n{}",
        render_view(access_delivery::registry().expect("registry").iter())
    );
    check("access_delivery_schema_view.md", &view);
}

// Behaviour (#3373 ruling 6): the access delivery kind is among the production kinds, under a
// code none of the others holds.
#[test]
fn access_delivery_kind_code_is_free_beside_the_production_kinds() {
    let registry = stored::production_kinds().expect("no code, name or table is held twice");
    assert_eq!(registry.iter().find(|set| set.kind == "AccessDelivery").map(|set| set.code), Some(KindCode::new("ad")));
}

// Behaviour (#3320 production shape, #3392 A1): every statement the kind's store generates
// searches a key or a declared index, never a scan or a temporary sort.
#[test]
fn access_delivery_statements_search_by_key() {
    let store =
        Store::open(rusqlite::Connection::open_in_memory().expect("memory"), "home", access_delivery::registry().expect("registry"))
            .expect("open");
    let statements = store.statements();
    let conn = store.into_connection();
    let mut failures = Vec::new();
    for (label, sql) in &statements {
        let rows = plan(&conn, sql);
        if schema_support::unkeyed(label, &rows) {
            failures.push(format!("{label}: {rows:?}"));
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

// Behaviour (#3964, ADR 0047): an access delivery reads back as written, through the resource
// store's serde form and through the generated tables' codec. The generator spans every
// mechanism, no needs through several, a retry or none, and timestamps across the epoch.
#[hegel::test]
fn access_delivery_round_trips(tc: hegel::TestCase) {
    use access_delivery::{DeliveredNeed, DeliveryMechanism, DeliveryRetry};
    use flotilla_schema::{Decoder, Encoder, Timestamp};
    use hegel::generators as gs;
    const MECHANISMS: [DeliveryMechanism; 6] = [
        DeliveryMechanism::HostPathReadOnlyLive,
        DeliveryMechanism::HostPathReadOnlySnapshot,
        DeliveryMechanism::HostPathReadWriteLive,
        DeliveryMechanism::HostPathReadWriteSnapshot,
        DeliveryMechanism::AgentFetchUrl,
        DeliveryMechanism::AgentPush,
    ];
    let text = || gs::text().max_size(12);
    let at = || Timestamp::from_micros(tc.draw(gs::integers::<i64>().min_value(-1 << 40).max_value(1 << 40)));
    let needs = (0..tc.draw(gs::integers::<usize>().max_value(3)))
        .map(|_| {
            let mechanism = MECHANISMS[tc.draw(gs::integers::<usize>().max_value(MECHANISMS.len() - 1))];
            (tc.draw(text()), DeliveredNeed { mechanism, digest: tc.draw(text()), at: at() })
        })
        .collect();
    let retry = tc.draw(gs::booleans()).then(|| DeliveryRetry {
        need: tc.draw(text()),
        attempts: tc.draw(gs::integers::<u32>()),
        error: tc.draw(text()),
        next_at: at(),
    });
    let spec = AccessDeliverySpec {
        access: tc.draw(text()),
        place: tc.draw(text()),
        harness: tc.draw(text()),
        home: tc.draw(text()),
        needs,
        retry,
    };
    let json = serde_json::to_value(&spec).expect("serialize");
    assert_eq!(serde_json::from_value::<AccessDeliverySpec>(json).expect("deserialize"), spec);
    assert_eq!(Decoder::decode::<AccessDeliverySpec>(Encoder::encode(&spec).expect("encode")).expect("decode"), spec);
}

fn copy(namespace: &str, name: &str, home: &str, spec: serde_json::Value) -> AccessDeliveryCopy {
    let document = serde_json::json!({
        "apiVersion": "flotilla.work/v1", "kind": "AccessDelivery",
        "metadata": {"name": name, "namespace": namespace, "resourceVersion": "4", "creationTimestamp": "2026-10-30T00:00:00Z",
            "labels": {"flotilla.work/convoy": "demo"}},
        "spec": spec,
    });
    let object =
        flotilla_resources::ResourceObject::from_k8s_object(serde_json::from_value(document).expect("k8s object")).expect("object");
    AccessDeliveryCopy { namespace: namespace.into(), object, home: home.into() }
}

fn delivered() -> serde_json::Value {
    serde_json::json!({"access": "flotilla/demo/implement/coder", "place": "env-a", "harness": "claude-code", "home": "/home/crew/.claude",
    "needs": {
        "account": {"mechanism": "agent-fetch-digest", "digest": "ab12", "at": 1_790_000_000_000_000_i64},
        "home": {"mechanism": "host-path-read-write-snapshot", "digest": "cd34", "at": 1_790_000_001_000_000_i64},
    }})
}

fn failed() -> serde_json::Value {
    let mut spec = delivered();
    spec["retry"] = serde_json::json!({"need": "account", "attempts": 7, "error": "permanent, not retried: refused", "next_at": i64::MAX});
    spec
}

// Behaviour (ADR 0047): every old record of this home imports, whatever it recorded, and reads
// back from the engine as the old spec it was; another home's is left to that home.
#[test]
fn every_record_of_this_home_imports_and_reads_back() {
    let mut report = ImportReport::default();
    let copies = [
        copy("flotilla", "access-a", "alpha", delivered()),
        copy("flotilla", "access-b", "alpha", failed()),
        copy("flotilla", "access-c", "beta", delivered()),
    ];
    let mut target = flotilla_engine_store::Import::in_memory("alpha", access_delivery::registry().expect("registry")).expect("target");
    import_access_deliveries(&copies, &mut target, &mut report);
    assert!(report.refused.is_empty(), "{:?}", report.refused);
    assert_eq!(report.imported.get(&("AccessDelivery", "delivered")), Some(&1));
    assert_eq!(report.imported.get(&("AccessDelivery", "retrying")), Some(&1));
    assert!(target.holds::<AccessDeliverySpec>("access-b", "alpha").expect("holds"));
    assert!(!target.holds::<AccessDeliverySpec>("access-c", "beta").expect("holds"));
}

// Behaviour: a deleted copy is dropped and counted, and a name one home holds in two namespaces,
// which the engine cannot tell apart, is refused by name.
#[test]
fn a_deleted_copy_is_dropped_and_a_name_in_two_namespaces_is_refused() {
    let mut deleted = copy("flotilla", "gone", "alpha", delivered());
    deleted.object.metadata.deletion_timestamp = Some(chrono::Utc::now());
    let copies = [deleted, copy("one", "twice", "alpha", delivered()), copy("two", "twice", "alpha", failed())];
    let mut report = ImportReport::default();
    let mut target = flotilla_engine_store::Import::in_memory("alpha", access_delivery::registry().expect("registry")).expect("target");
    import_access_deliveries(&copies, &mut target, &mut report);
    assert_eq!(report.dropped.get(&("AccessDelivery", "deleted")), Some(&1));
    assert!(report.imported.is_empty());
    assert_eq!(report.refused.iter().map(|refusal| refusal.record.as_str()).collect::<Vec<_>>(), ["twice", "twice"]);
}
