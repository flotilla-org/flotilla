//! The crew kinds' stored shape (#3629 Track B layer 5): Vessel and Ensure, declared by the
//! engine, and TerminalSession. Deterministic DDL and a readable view, checked in for the owner's
//! schema look-over, the plans of their statements, and a session through the schema codec.

use std::collections::BTreeMap;

mod schema_support;

use flotilla_provisioning::stored::{
    self,
    environment::Environment,
    terminal_session::{Launch, TerminalSessionKind, TerminalSessionRequest},
    Holder,
};
use flotilla_resources::TerminalBrief;
use flotilla_schema::{render_view, Decoder, Encoder, Frozen, Kind, Ref, State, Timestamp};
use flotilla_schema_sqlite::{schema_ddl, DdlMode, Store};
use flotilla_state_machines::{
    machines::lifecycle::{Lifecycle, LifecycleState},
    Resource, Version,
};
use schema_support::{check, plan};

// Behaviour (CLAUDE.md schema review): the crew kinds render deterministic DDL, checked in so the
// owner's look-over and every later change show as a diff.
#[test]
fn crew_store_ddl_matches_the_snapshot() {
    check("crew.sql", &schema_ddl(&stored::crew_registry().expect("registry"), DdlMode::Create));
}

// Behaviour (CLAUDE.md schema review): the readable view of the crew kind tables.
#[test]
fn crew_store_schema_view_matches_the_snapshot() {
    let view = format!(
        "# Crew store schema view\n\nKind tables only; the shared tables are in `crew.sql`.\n\n{}",
        render_view(stored::crew_registry().expect("registry").iter())
    );
    check("crew_schema_view.md", &view);
}

// Behaviour (#3392 A1, #3320 production shape): every statement for the crew kinds searches a key
// or a declared index, and removing an identity checks each root reference through its index (a
// vessel's convoy, a session's environment; a session's holder is no reference). Only plans are
// read.
#[test]
fn crew_statements_and_identity_removal_search_by_key() {
    let store = Store::open(rusqlite::Connection::open_in_memory().expect("memory"), "home", stored::crew_registry().expect("registry"))
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
    for index in ["vessel_by_convoy", "terminal_session_by_environment"] {
        assert!(rows.iter().any(|row| row.contains(index)), "{index}: {rows:?}");
    }
}

// Behaviour (ADR 0064 D5): a session survives the schema codec in every lifecycle label, as a tool
// and as an agent with a frozen brief and launch values.
#[test]
fn a_terminal_session_survives_the_codec() {
    let brief =
        TerminalBrief { path: "/w/.flotilla/brief.md".into(), content: "# brief".into(), artifact_digest: None, copies: vec!["/x".into()] };
    let agent = Launch::Agent {
        capability: "coding".into(),
        adapter: Some("claude-code".into()),
        model: None,
        brief: Frozen::new(brief).expect("freeze"),
    };
    for (source, holder) in [(Launch::Tool { command: "bash".into() }, None), (agent, Some(Holder::vessel("convoy-work")))] {
        for state in [LifecycleState::requested(), LifecycleState::Realised { identity: "cleat-1".into() }, LifecycleState::Gone] {
            let spec = TerminalSessionRequest {
                environment: Ref::<Resource<Environment>>::new("env", "feta"),
                holder: holder.clone(),
                role: "coder".into(),
                pool: "cleat".into(),
                cwd: "/workspace".into(),
                env: BTreeMap::from([("A".into(), "1".into())]),
                source: source.clone(),
            };
            let resource =
                Resource::<Lifecycle<TerminalSessionKind>>::stored("s", spec, State::new(state, Timestamp::from_micros(1)), Version(2));
            let decoded: Resource<Lifecycle<TerminalSessionKind>> =
                Decoder::decode(Encoder::encode(&resource).expect("encodes")).expect("decodes");
            assert_eq!(decoded.with_identity("s", 2), resource);
        }
    }
}
