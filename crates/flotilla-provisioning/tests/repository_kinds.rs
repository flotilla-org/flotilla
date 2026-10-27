//! Repository's stored shape and import (#3824 layer 3a): deterministic DDL and a readable view,
//! checked in for the owner's schema look-over, the plans of its statements, and the import of
//! every live repository record the fleet held at the last corpus refresh.

mod schema_support;

use std::collections::BTreeMap;

use flotilla_provisioning::{
    engine::{
        import::ImportReport,
        repository_import::{import_repository, rehearse_repositories, Imported, RepositoryCopy},
    },
    stored::repository::{self as stored, VerificationProvenance},
};
use flotilla_resources::{
    definition_provenance::{SUPERSEDED_BY_ANNOTATION, VERIFICATION_PROJECT_ANNOTATION, VERIFICATION_PROVENANCE_ANNOTATION},
    ObjectMeta, Repository, RepositorySpec, RepositoryStatus, ResourceObject,
};
use flotilla_schema::render_view;
use flotilla_schema_sqlite::{schema_ddl, DdlMode, Store};
use schema_support::{check, plan};

// Behaviour (CLAUDE.md schema review): Repository renders deterministic DDL, checked in so the
// owner's look-over and every later change show as a diff.
#[test]
fn repository_ddl_matches_the_snapshot() {
    check("repository.sql", &schema_ddl(&stored::registry().expect("registry"), DdlMode::Create));
}

// Behaviour (CLAUDE.md schema review): the readable view of Repository's tables.
#[test]
fn repository_schema_view_matches_the_snapshot() {
    let view = format!(
        "# Repository store schema view\n\nKind tables only; the shared tables are in `repository.sql`.\n\n{}",
        render_view(stored::registry().expect("registry").iter())
    );
    check("repository_schema_view.md", &view);
}

// Behaviour (#3320 production shape, #3392 A1): every statement over Repository's tables searches
// a key or a declared index, never a scan.
#[test]
fn repository_statements_search_by_key() {
    let store =
        Store::open(rusqlite::Connection::open_in_memory().expect("memory"), "home", stored::registry().expect("registry")).expect("open");
    let statements = store.statements();
    let conn = store.into_connection();
    let failures: Vec<_> = statements
        .iter()
        .map(|(label, sql)| (label, plan(&conn, sql)))
        .filter(|(_, rows)| rows.iter().any(|row| row.contains("SCAN") || row.contains("TEMP B-TREE")))
        .collect();
    assert!(failures.is_empty(), "{failures:#?}");
}

/// The fleet's repository records at the last corpus refresh, each named by its key.
fn corpus() -> Vec<ResourceObject<Repository>> {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../flotilla-store-testkit/tests/fixtures/stored-records/r355/Repository.json");
    let records: Vec<serde_json::Value> = serde_json::from_str(&std::fs::read_to_string(path).expect("corpus")).expect("corpus json");
    records
        .into_iter()
        .map(|record| {
            let spec: RepositorySpec = serde_json::from_value(record["spec"].clone()).expect("corpus spec");
            let status: Option<RepositoryStatus> =
                record.get("status").map(|status| serde_json::from_value(status.clone()).expect("status"));
            let metadata = ObjectMeta {
                name: spec.key().to_string(),
                namespace: "flotilla".into(),
                resource_version: "1".into(),
                labels: BTreeMap::new(),
                annotations: BTreeMap::new(),
                owner_references: Vec::new(),
                finalizers: Vec::new(),
                deletion_timestamp: None,
                creation_timestamp: chrono::DateTime::UNIX_EPOCH,
                merge: None,
            };
            ResourceObject { metadata, spec, status }
        })
        .collect()
}

fn provenance() -> Vec<VerificationProvenance> {
    vec![VerificationProvenance {
        source_repository: "ops".into(),
        source_commit: "abc123".into(),
        entry_path: ".flotilla/ops/checks.yaml".into(),
        manifest_root: Some("ops-00aa".into()),
    }]
}

// Behaviour (ADR 0047, #3824 layer 3a): every live repository the fleet held imports, at each home
// that held it, with its spec and its observed default branch; the verification annotations
// become typed fields; deleted and superseded copies drop; a copy named other than its key, or
// with provenance that does not decode, is refused.
#[test]
fn every_repository_imports_with_its_spec_ops_data_and_observations() {
    let corpus = corpus();
    assert!(!corpus.is_empty());
    for old in &corpus {
        let Imported::Record(record) = import_repository(old).expect("imports") else { panic!("{} dropped", old.metadata.name) };
        assert_eq!(record.spec(), &old.spec);
        let status = old.status.clone().unwrap_or_default();
        assert_eq!(
            (&record.default_branch, &record.default_branch_observations),
            (&status.default_branch, &status.default_branch_observations)
        );
        assert_eq!((record.verification_project.as_ref(), record.verification_provenance.len()), (None, 0));
    }

    let mut owned = corpus[0].clone();
    owned.metadata.annotations.insert(VERIFICATION_PROJECT_ANNOTATION.into(), "app".into());
    owned.metadata.annotations.insert(VERIFICATION_PROVENANCE_ANNOTATION.into(), serde_json::to_string(&provenance()).expect("json"));
    let Imported::Record(record) = import_repository(&owned).expect("owned") else { panic!("dropped") };
    assert_eq!((record.verification_project.as_deref(), record.verification_provenance.clone()), (Some("app"), provenance()));

    let mut superseded = corpus[0].clone();
    superseded.metadata.annotations.insert(SUPERSEDED_BY_ANNOTATION.into(), "other".into());
    let mut deleted = corpus[0].clone();
    deleted.metadata.deletion_timestamp = Some(chrono::Utc::now());
    let mut misnamed = corpus[0].clone();
    misnamed.metadata.name = "not-its-key".into();
    let mut undecodable = corpus[0].clone();
    undecodable.metadata.annotations.insert(VERIFICATION_PROVENANCE_ANNOTATION.into(), "{".into());

    let copy = |object: ResourceObject<Repository>, home: &str| RepositoryCopy { namespace: "flotilla".into(), object, home: home.into() };
    // The corpus gathers several hosts' stores, so a key recurs: each recurrence is another home's.
    let mut seen = BTreeMap::<String, usize>::new();
    let mut copies: Vec<_> = corpus
        .iter()
        .cloned()
        .map(|object| {
            let recurrence = seen.entry(object.metadata.name.clone()).or_default();
            *recurrence += 1;
            copy(object, &format!("alpha-{recurrence}"))
        })
        .collect();
    // A repository two homes observed imports at both.
    copies.push(copy(corpus[0].clone(), "beta"));
    copies.extend([copy(superseded, "gamma"), copy(deleted, "delta"), copy(misnamed, "epsilon"), copy(undecodable, "zeta")]);
    let mut report = ImportReport::default();
    rehearse_repositories(&copies, &mut report);
    assert_eq!(report.imported.values().sum::<usize>(), corpus.len() + 1, "{report:?}");
    assert_eq!(report.dropped.values().sum::<usize>(), 2, "{report:?}");
    let refused: Vec<_> = report.refused.iter().map(|refusal| refusal.reason.split(':').next().unwrap_or_default().to_owned()).collect();
    assert_eq!(refused, ["at `epsilon`", "at `zeta`"], "{report:?}");
}

// Behaviour (#3212: no namespaces): one key a home holds in two namespaces is refused in both.
#[test]
fn a_key_held_in_two_namespaces_at_one_home_is_refused() {
    let old = corpus().remove(0);
    let copies =
        ["flotilla", "other"].map(|namespace| RepositoryCopy { namespace: namespace.into(), object: old.clone(), home: "alpha".into() });
    let mut report = ImportReport::default();
    rehearse_repositories(&copies, &mut report);
    assert_eq!((report.imported.values().sum::<usize>(), report.refused.len()), (0, 2), "{report:?}");
}
