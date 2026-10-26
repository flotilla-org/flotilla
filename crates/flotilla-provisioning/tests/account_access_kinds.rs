//! The account access kinds' stored shape (#3907): AccountBinding and AccountProvision.
//! Deterministic DDL and a readable view, checked in for the owner's schema look-over, the codes
//! they would hold beside the production kinds, the plans of their statements, and both kinds
//! through the schema codec and serde. No row is written to or read from a store here.

use std::collections::{BTreeMap, BTreeSet};

mod schema_support;

use flotilla_provisioning::stored;
use flotilla_resources::account_access::{
    self, AccessName, AccountBinding, AccountProvision, BindingState, CredentialName, Delivered, EpochEntry, GrantName, GrantScope,
    ProvisionPlan, Selection, UpdateMode,
};
use flotilla_schema::{render_view, Address, Decoder, Encoder, KindCode, Records, Ref, State, Timestamp};
use flotilla_schema_sqlite::{schema_ddl, DdlMode, Store};
use schema_support::{check, plan};

// Behaviour (CLAUDE.md schema review): the account access kinds render deterministic DDL, checked
// in so the owner's look-over and every later change show as a diff.
#[test]
fn account_access_store_ddl_matches_the_snapshot() {
    check("account_access.sql", &schema_ddl(&account_access::registry().expect("registry"), DdlMode::Create));
}

// Behaviour (CLAUDE.md schema review): the readable view of the account access kind tables.
#[test]
fn account_access_store_schema_view_matches_the_snapshot() {
    let view = format!(
        "# Account access store schema view\n\nKind tables only; the shared tables are in `account_access.sql`.\n\n{}",
        render_view(account_access::registry().expect("registry").iter())
    );
    check("account_access_schema_view.md", &view);
}

// Behaviour (#3373 ruling 6): the account access kinds register beside every production kind
// under codes none of them holds, so the engine store can take them on at the cut-over.
#[test]
fn account_access_kind_codes_are_free_beside_the_production_kinds() {
    let mut registry = stored::production_kinds().expect("production kinds");
    for set in account_access::registry().expect("registry").iter() {
        registry.register_table_set(set.clone()).expect("no code, name or table is held twice");
    }
    let held = |kind: &str| registry.iter().find(|set| set.kind == kind).map(|set| set.code);
    assert_eq!(held("AccountBinding"), Some(KindCode::new("ab")));
    assert_eq!(held("AccountProvision"), Some(KindCode::new("ap")));
}

// Behaviour (#3320 production shape, #3392 A1): every statement for these kinds searches a key or
// a declared index, and removing a binding's identity finds its provision records by index.
#[test]
fn account_access_statements_search_by_key() {
    let store = Store::open(rusqlite::Connection::open_in_memory().expect("memory"), "home", account_access::registry().expect("registry"))
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
    let sql = &statements.iter().find(|(label, _)| label == "identity_delete").expect("shared statement").1;
    let rows = plan(&conn, sql);
    assert!(rows.iter().any(|row| row.contains("account_provision_by_binding")), "{rows:?}");
}

fn binding(state: BindingState, selection: Selection) -> AccountBinding {
    let mut epochs = Records::new();
    epochs.append(1, EpochEntry { epoch: "1".into(), observed_at: Timestamp::from_micros(10) }).expect("append");
    epochs.append(2, EpochEntry { epoch: "2".into(), observed_at: Timestamp::from_micros(20) }).expect("append");
    AccountBinding {
        requester: "project:flotilla/convoy:c1/role:coder".parse::<Address>().expect("address"),
        item: "claude-code".into(),
        requirement: AccessName("claude:subscription".into()),
        credential: CredentialName("claude-jseo-oauth".into()),
        account: Some("someone@example.com".into()),
        grants: BTreeMap::from([
            (GrantName("claude-box-accounts".into()), GrantScope { scope: "account".into() }),
            (GrantName("claude-max-contained".into()), GrantScope { scope: "account".into() }),
        ]),
        candidates: BTreeSet::from([
            CredentialName("claude-jseo-oauth".into()),
            CredentialName("claude-max".into()),
            CredentialName("claude-time-to-change-oauth".into()),
        ]),
        exclusions: BTreeMap::from([(CredentialName("claude-time-to-change-oauth".into()), "replacement required".into())]),
        selection,
        epochs,
        opened_at: Timestamp::from_micros(5),
        state: State::new(state, Timestamp::from_micros(30)),
    }
}

// Behaviour (ADR 0064 D5, ADR 0047): a binding survives the schema codec and the resource store's
// JSON in every state and selection, its epoch lineage and selection record included.
#[test]
fn a_binding_survives_the_codec_and_json() {
    let states = [
        BindingState::Open,
        BindingState::Closed { reason: "rebind".into(), delivered_until: Some(Timestamp::from_micros(40)) },
        BindingState::Revoked { reason: "binding revoked: no authorizing grant".into(), delivered_until: None },
    ];
    let selections = [Selection::Pin { pinned_by: "convoy start".into() }, Selection::Draw { index: 1 }];
    for state in states {
        for selection in selections.clone() {
            let value = binding(state.clone(), selection);
            let decoded: AccountBinding = Decoder::decode(Encoder::encode(&value).expect("encodes")).expect("decodes");
            assert_eq!(decoded, value);
            let json: AccountBinding = serde_json::from_value(serde_json::to_value(&value).expect("json")).expect("from json");
            assert_eq!(json, value);
        }
    }
}

// Behaviour (ADR 0064 D5, ADR 0047): a provision record survives the codec and JSON, delivered or
// not, in every update mode.
#[test]
fn a_provision_record_survives_the_codec_and_json() {
    for update_mode in [UpdateMode::Push, UpdateMode::Mechanism, UpdateMode::Restart] {
        for delivered in [None, Some(Delivered { epoch: "1".into(), generation: "g7".into(), at: Timestamp::from_micros(50) })] {
            let value = AccountProvision {
                binding: Ref::new("c1.coder.claude-subscription", "udder"),
                place: "env-c1-work".into(),
                environment_kind: "docker".into(),
                plan: ProvisionPlan {
                    implementation: "claude-direct".into(),
                    effective_access: "account".into(),
                    mechanism: "oauth-token-file:CLAUDE_CONFIG_DIR".into(),
                    update_mode,
                },
                home: "/tmp/flotilla-config/agent-homes/requesters/c1/coder/claude".into(),
                delivered,
            };
            let decoded: AccountProvision = Decoder::decode(Encoder::encode(&value).expect("encodes")).expect("decodes");
            assert_eq!(decoded, value);
            let json: AccountProvision = serde_json::from_value(serde_json::to_value(&value).expect("json")).expect("from json");
            assert_eq!(json, value);
        }
    }
}
