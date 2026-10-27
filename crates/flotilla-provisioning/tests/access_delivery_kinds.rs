//! The access layer's per-access status (#3964): AccessDelivery. Deterministic DDL and
//! a readable view, checked in for the owner's schema look-over, the code it would hold
//! beside the production and account access kinds, and the plans of its statements.

mod schema_support;

use flotilla_provisioning::stored;
use flotilla_resources::access_delivery;
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

// Behaviour (#3373 ruling 6): the access delivery kind registers beside every production
// kind and the account access kinds under a code none of them holds.
#[test]
fn access_delivery_kind_code_is_free_beside_the_production_kinds() {
    // The production kinds include the account access kinds (#3824 layer 1).
    let mut registry = stored::production_kinds().expect("production kinds");
    for set in access_delivery::registry().expect("registry").iter() {
        registry.register_table_set(set.clone()).expect("no code, name or table is held twice");
    }
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
        if rows.iter().any(|row| row.contains("SCAN") || row.contains("TEMP B-TREE")) {
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
    use flotilla_resources::AccessDeliverySpec;
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
