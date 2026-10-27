//! ManifestRoot's stored shape (#3824 layer 2c): deterministic DDL and a readable view, checked in
//! for the owner's schema look-over, the plans of its statements, and a run of every edge through
//! the store's log, read back.

mod schema_support;

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
};

use flotilla_engine_store::StoreLog;
use flotilla_provisioning::stored::manifest_root::{
    self as stored, Act, Applied, ManifestRoot, Outcome, Rebind, RequestResolution, Resolve, Retire, RootRequest, SourceFailed, Suspend,
    Unsuspend,
};
use flotilla_resources::CharterSource;
use flotilla_schema::{render_view, Kind, Timestamp};
use flotilla_schema_sqlite::{schema_ddl, DdlMode, Store};
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
        .filter(|(_, rows)| rows.iter().any(|row| row.contains("SCAN") || row.contains("TEMP B-TREE")))
        .collect();
    assert!(failures.is_empty(), "{failures:#?}");
}

// Behaviour (ADR 0064 storage): every edge's writes reach the store. After each edge, the root the
// store holds (its row, its collections and its state data) reads back as the root the home holds.
#[test]
fn every_edge_of_a_root_reads_back_from_the_store() {
    let store = Arc::new(Mutex::new(open()));
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
            reached: docs(&["a.yaml"]),
            baselines: hashes(&[("a.yaml", "ha2")]),
            refusals: BTreeMap::new(),
        };
        home.apply("root", pass, &(), at).expect("pruned");
    });
    step(&mut home, 11, &|home, at| {
        home.apply("root", Retire {}, &(), at).expect("retired");
    });
}

mod import {
    use std::collections::{BTreeMap, BTreeSet};

    use chrono::{TimeZone, Utc};
    use flotilla_provisioning::{
        engine::{
            import::ImportReport,
            manifest_root_import::{document_name, import_root, rehearse_roots},
        },
        stored::manifest_root::{phase, Outcome, Phase, RootState},
    };
    use flotilla_resources::{
        CharterSource, DocumentKey, DocumentPhase, DocumentState, ManifestRootSpec, ManifestRootStatus, ObjectMeta, Resolution,
        ResolutionAction, ResolutionOutcome, ResourceObject,
    };
    use hegel::{generators as gs, TestCase};

    /// One document as the old reconciler left it: its phase, the hashes it recorded, and its
    /// live and desired hashes.
    struct Old {
        key: DocumentKey,
        state: DocumentState,
        suspended: bool,
    }

    fn hash(tc: &TestCase) -> String {
        ["h1", "h2", "h3"][tc.draw(gs::integers::<usize>().min_value(0).max_value(2))].to_owned()
    }

    // Every shape the old reconciler writes (resource_manifest.rs `reconcile_document`): Applied
    // with live, desired and baseline equal; Drifted with the baseline off the live hash, which is
    // off the desired one; Refused with any hashes, a reason or none; Suspended with any.
    fn document(tc: &TestCase, index: usize) -> Old {
        let key =
            DocumentKey { path: format!("d{index}.yaml"), kind: "Forge".into(), namespace: "flotilla".into(), name: format!("f{index}") };
        let (phase, live, desired, baseline, reason) = match tc.draw(gs::integers::<u8>().min_value(0).max_value(3)) {
            0 => {
                let h = hash(tc);
                (DocumentPhase::Applied, Some(h.clone()), Some(h.clone()), Some(h), None)
            }
            1 => (DocumentPhase::Drifted, Some("live".into()), Some("desired".into()), Some(hash(tc)), Some("live spec drift".into())),
            2 => {
                let reason = tc.draw(gs::booleans()).then(|| "unmanaged object".to_owned());
                (DocumentPhase::Refused, Some(hash(tc)), Some(hash(tc)), tc.draw(gs::booleans()).then(|| hash(tc)), reason)
            }
            _ => (DocumentPhase::Suspended, Some(hash(tc)), Some(hash(tc)), tc.draw(gs::booleans()).then(|| hash(tc)), None),
        };
        let observed_at = Utc.timestamp_opt(1_700_000_000 + index as i64, 0).single().expect("time");
        let state = DocumentState {
            phase,
            reason,
            live_hash: live,
            desired_hash: desired,
            baseline_hash: baseline,
            observed_at,
            resolved_token: None,
            resolution_outcome: None,
        };
        Old { key, state, suspended: phase == DocumentPhase::Suspended }
    }

    fn root(documents: &[Old], resolutions: BTreeMap<DocumentKey, Resolution>) -> ResourceObject<flotilla_resources::ManifestRoot> {
        let status = ManifestRootStatus {
            applied_revision: Some("r1".into()),
            source_error: None,
            documents: documents.iter().map(|old| (old.key.clone(), old.state.clone())).collect(),
            stalled: None,
        };
        let spec = ManifestRootSpec {
            binding: Some(CharterSource::LocalDirectory { directory: "/charter".into() }),
            host: "feta".into(),
            path: "/manifests".into(),
            source: "project-map".into(),
            suspended: documents.iter().filter(|old| old.suspended).map(|old| old.key.clone()).collect::<BTreeSet<_>>(),
            resolutions,
        };
        let metadata = ObjectMeta {
            name: "manifest-0011223344556677".into(),
            namespace: "flotilla".into(),
            resource_version: "3".into(),
            labels: BTreeMap::new(),
            annotations: BTreeMap::new(),
            owner_references: Vec::new(),
            finalizers: Vec::new(),
            deletion_timestamp: None,
            creation_timestamp: Utc.timestamp_opt(1_600_000_000, 0).single().expect("time"),
            merge: None,
        };
        ResourceObject { metadata, spec, status: Some(status) }
    }

    // Behaviour (ADR 0047, the 2026-10-26 ruling): an old root imports with its binding, applied
    // revision, baselines, refusals and suspensions, and the phase judgement over the imported
    // records reads every document as the old status recorded it; the rehearsal restores it.
    #[hegel::test]
    fn the_judgement_reads_every_imported_document_as_the_old_status_did(tc: TestCase) {
        let documents: Vec<_> = (0..tc.draw(gs::integers::<usize>().min_value(0).max_value(4))).map(|index| document(&tc, index)).collect();
        let old = root(&documents, BTreeMap::new());
        let imported = import_root(&old).expect("imports");
        assert_eq!(imported.state().applied(), Some("r1"));
        assert!(matches!(imported.state(), RootState::Bound { binding: Some(CharterSource::LocalDirectory { .. }), .. }));
        for old in &documents {
            let expected = match old.state.phase {
                DocumentPhase::Applied => Phase::Applied,
                DocumentPhase::Drifted => Phase::Drifted,
                DocumentPhase::Refused => Phase::Refused,
                DocumentPhase::Suspended => Phase::Suspended,
            };
            let judged = phase(
                imported.spec(),
                &document_name(&old.key).expect("name"),
                old.state.live_hash.as_deref(),
                old.state.desired_hash.as_deref(),
            );
            assert_eq!(judged, expected, "{:?}", old.key);
        }
        let mut report = ImportReport::default();
        rehearse_roots(&[(old, "feta".into())], &mut report);
        assert!(report.refused.is_empty(), "{:?}", report.refused);
    }

    // Behaviour (ADR 0047, the review of #3959): a resolution imports as Requested until the old
    // status claimed its token, then as the outcome recorded for that token; an outcome recorded
    // for an earlier token leaves the current request Requested.
    #[test]
    fn resolutions_import_with_the_outcome_of_their_own_token() {
        let mut documents: Vec<_> = (0..3)
            .map(|index| Old {
                key: DocumentKey {
                    path: format!("d{index}.yaml"),
                    kind: "Forge".into(),
                    namespace: "flotilla".into(),
                    name: format!("f{index}"),
                },
                state: DocumentState {
                    phase: DocumentPhase::Applied,
                    reason: None,
                    live_hash: Some("h".into()),
                    desired_hash: Some("h".into()),
                    baseline_hash: Some("h".into()),
                    observed_at: Utc.timestamp_opt(1_700_000_000, 0).single().expect("time"),
                    resolved_token: None,
                    resolution_outcome: None,
                },
                suspended: false,
            })
            .collect();
        documents[1].state.resolved_token = Some("t".into());
        documents[1].state.resolution_outcome = Some(ResolutionOutcome::Synced);
        documents[2].state.resolved_token = Some("earlier".into());
        documents[2].state.resolution_outcome = Some(ResolutionOutcome::Failed("refused".into()));
        let resolutions = documents
            .iter()
            .map(|old| (old.key.clone(), Resolution { action: ResolutionAction::Sync, token: "t".into(), requested_by: "operator".into() }))
            .collect();
        let imported = import_root(&root(&documents, resolutions)).expect("imports");
        let outcome = |index: usize| imported.spec().resolutions[&document_name(&documents[index].key).expect("name")].outcome.clone();
        assert_eq!(outcome(0), Outcome::Requested);
        assert_eq!(outcome(1), Outcome::Synced);
        assert_eq!(outcome(2), Outcome::Requested);
    }
}
