//! The ManifestRoot port contract (#3824 layer 2c): one behaviour, run against the old store's
//! roots and the engine's, so either can stand behind the manifest reconciler, project ops, the
//! CLI's resolutions and the views.

use std::sync::Arc;

use chrono::{TimeZone, Utc};
use flotilla_provisioning::engine::manifest_root::EngineManifestRoots;
use flotilla_resources::{
    manifest_roots::{ManifestRoots, PassReport, RootDeclaration},
    CharterSource, Clock, DocumentKey, DocumentPhase, DocumentState, ResolutionAction, ResolutionOutcome, SystemClock,
};
use flotilla_schema_sqlite::stream::Service;
use flotilla_store::{OldStoreManifestRoots, ResourceBackend};
use flotilla_store_memory::InMemoryBackend;

const NS: &str = "flotilla";
const ROOT: &str = "manifest-0011223344556677";

fn key(path: &str) -> DocumentKey {
    DocumentKey { path: path.into(), kind: "Forge".into(), namespace: NS.into(), name: path.trim_end_matches(".yaml").into() }
}

fn declaration(binding: Option<&str>) -> RootDeclaration {
    RootDeclaration {
        host: "feta".into(),
        path: "/manifests".into(),
        source: "project-map".into(),
        binding: binding.map(|repo| CharterSource::Repository { repo: repo.into(), branch: "main".into(), path: String::new() }),
        ops: false,
    }
}

fn document(phase: DocumentPhase, hash: &str, reason: Option<&str>) -> DocumentState {
    DocumentState {
        phase,
        reason: reason.map(str::to_owned),
        live_hash: Some(hash.into()),
        desired_hash: Some(hash.into()),
        baseline_hash: (phase != DocumentPhase::Refused).then(|| hash.into()),
        observed_at: Utc.timestamp_opt(1_700_000_000, 0).single().expect("time"),
        resolved_token: None,
        resolution_outcome: None,
    }
}

fn pass(revision: &str, complete: bool, documents: Vec<(DocumentKey, DocumentState)>) -> PassReport {
    PassReport { revision: revision.into(), complete, documents: documents.into_iter().collect(), at: Utc::now() }
}

/// The contract. Each step states the behaviour it checks.
async fn contract(roots: &dyn ManifestRoots) {
    // A declared root reads back homed here with its declaration, and no records yet.
    roots.declare(NS, ROOT, declaration(None)).await.expect("declare");
    let root = roots.root(NS, ROOT).await.expect("read").expect("declared");
    assert!(root.local);
    assert_eq!((root.spec.host.as_str(), root.spec.path.as_str(), root.spec.source.as_str()), ("feta", "/manifests", "project-map"));
    assert_eq!(root.status.applied_revision, None);
    assert_eq!(roots.roots(NS).await.expect("list").len(), 1);

    // Declaring again moves only the binding; a declaration naming another host is refused.
    roots.declare(NS, ROOT, declaration(Some("project-map"))).await.expect("rebind");
    assert!(roots.root(NS, ROOT).await.expect("read").expect("root").spec.binding.is_some());
    assert!(roots.declare(NS, ROOT, RootDeclaration { host: "kiwi".into(), ..declaration(None) }).await.is_err());

    // A complete pass records its revision, each document's baseline and each refusal, and the
    // refusals make the root's stall.
    let a = key("a.yaml");
    let b = key("b.yaml");
    let first = pass(
        "r1",
        true,
        vec![
            (a.clone(), document(DocumentPhase::Applied, "ha", None)),
            (b.clone(), document(DocumentPhase::Refused, "hb", Some("unmanaged object"))),
        ],
    );
    roots.record_pass(NS, ROOT, first).await.expect("pass");
    let root = roots.root(NS, ROOT).await.expect("read").expect("root");
    assert_eq!(root.status.applied_revision.as_deref(), Some("r1"));
    assert_eq!(root.baseline(&a), Some("ha"));
    assert_eq!(root.status.documents[&b].phase, DocumentPhase::Refused);
    assert_eq!(root.status.documents[&b].reason.as_deref(), Some("unmanaged object"));
    assert!(root.status.stalled.as_ref().expect("stall").evidence.contains("b.yaml: unmanaged object"));

    // A source that does not load keeps the revision and baselines, and its error is the stall.
    roots.record_source(NS, ROOT, None, Some("fetch failed")).await.expect("source failure");
    let root = roots.root(NS, ROOT).await.expect("read").expect("root");
    assert_eq!(root.status.source_error.as_deref(), Some("fetch failed"));
    assert_eq!(root.status.applied_revision.as_deref(), Some("r1"));
    assert_eq!(root.baseline(&a), Some("ha"));
    assert_eq!(root.status.stalled.as_ref().expect("stall").evidence, "fetch failed");

    // A resolution is requested under a token, claimed once, and settled by the pass that ran it.
    let token = roots.request_resolution(NS, ROOT, &b, ResolutionAction::Sync, "operator").await.expect("request");
    let root = roots.root(NS, ROOT).await.expect("read").expect("root");
    assert_eq!(root.spec.resolutions[&b].token, token);
    assert!(roots.claim_resolution(NS, ROOT, &b, &token).await.expect("claim"), "the request is claimed");
    assert!(!roots.claim_resolution(NS, ROOT, &b, &token).await.expect("claim again"), "a claim is made once");
    assert!(!roots.claim_resolution(NS, ROOT, &b, "stale").await.expect("stale claim"), "a stale token claims nothing");
    let mut synced = document(DocumentPhase::Applied, "hb", None);
    synced.resolved_token = Some(token.clone());
    synced.resolution_outcome = Some(ResolutionOutcome::Synced);
    let second = pass("r2", true, vec![(a.clone(), document(DocumentPhase::Applied, "ha", None)), (b.clone(), synced)]);
    roots.record_pass(NS, ROOT, second).await.expect("pass");
    let root = roots.root(NS, ROOT).await.expect("read").expect("root");
    assert_eq!(root.status.applied_revision.as_deref(), Some("r2"));
    assert_eq!(root.status.source_error, None, "a pass clears the source error");
    assert!(root.status.stalled.is_none(), "no refusal stands");
    assert_eq!(root.status.documents[&b].resolved_token.as_deref(), Some(token.as_str()));
    assert_eq!(root.status.documents[&b].resolution_outcome, Some(ResolutionOutcome::Synced));
    assert!(!roots.claim_resolution(NS, ROOT, &b, &token).await.expect("claim settled"), "a settled request is never claimed again");

    // An incomplete pass that did not reach `b` keeps its baseline and the applied revision.
    roots.record_pass(NS, ROOT, pass("r3", false, vec![(a.clone(), document(DocumentPhase::Applied, "ha3", None))])).await.expect("pass");
    let root = roots.root(NS, ROOT).await.expect("read").expect("root");
    assert_eq!(root.status.applied_revision.as_deref(), Some("r2"));
    assert_eq!(root.baseline(&a), Some("ha3"));
    assert_eq!(root.baseline(&b), Some("hb"));

    // Suspension is the operator's, and is undone the same way.
    roots.set_suspended(NS, ROOT, &a, true).await.expect("suspend");
    assert!(roots.root(NS, ROOT).await.expect("read").expect("root").spec.suspended.contains(&a));
    roots.set_suspended(NS, ROOT, &a, false).await.expect("resume");
    assert!(!roots.root(NS, ROOT).await.expect("read").expect("root").spec.suspended.contains(&a));

    // A project's ops root follows its binding's path; a retired root is no longer declared, and
    // declaring its source again gives a root with no records.
    let ops = "ops-00aa";
    let ops_declaration = |path: &str| RootDeclaration { path: path.into(), ops: true, ..declaration(Some("app")) };
    roots.declare(NS, ops, ops_declaration("ops")).await.expect("ops root");
    roots.record_source(NS, ops, Some("c1".into()), None).await.expect("ops source");
    assert_eq!(roots.root(NS, ops).await.expect("read").expect("ops").status.applied_revision.as_deref(), Some("c1"));
    roots.declare(NS, ops, ops_declaration("ops/moved")).await.expect("moved");
    assert_eq!(roots.root(NS, ops).await.expect("read").expect("ops").spec.path, "ops/moved");
    roots.retire(NS, ops).await.expect("retire");
    roots.retire(NS, ops).await.expect("retiring an absent root is done");
    assert!(roots.root(NS, ops).await.expect("read").is_none());
    roots.declare(NS, ops, ops_declaration("ops")).await.expect("declared again");
    assert_eq!(roots.root(NS, ops).await.expect("read").expect("ops").status.applied_revision, None);
    assert_eq!(roots.roots(NS).await.expect("list").len(), 2);
}

#[tokio::test]
async fn the_old_store_keeps_the_manifest_roots_contract() {
    contract(&OldStoreManifestRoots::new(ResourceBackend::new(InMemoryBackend::default()))).await;
}

fn engine() -> (tempfile::TempDir, EngineManifestRoots) {
    let directory = tempfile::tempdir().expect("temp dir");
    let mut kinds = flotilla_schema::KindRegistry::new();
    flotilla_provisioning::stored::manifest_root::register(&mut kinds).expect("register");
    let service = Arc::new(Service::open(&directory.path().join("decided.sqlite"), "alpha", kinds).expect("store"));
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let roots = EngineManifestRoots::open(service, clock).expect("open");
    (directory, roots)
}

#[tokio::test]
async fn the_engine_keeps_the_manifest_roots_contract() {
    let (_directory, roots) = engine();
    contract(&roots).await;
}

// Behaviour (ADR 0064): the engine's records survive a restart: a home opened again over its store
// reads its roots' revision, baselines and refusals as they were, though the last pass's judgement
// (kept in memory) is gone.
#[tokio::test]
async fn an_engine_root_reads_back_after_a_restart() {
    let directory = tempfile::tempdir().expect("temp dir");
    let open = || {
        let mut kinds = flotilla_schema::KindRegistry::new();
        flotilla_provisioning::stored::manifest_root::register(&mut kinds).expect("register");
        let service = Arc::new(Service::open(&directory.path().join("decided.sqlite"), "alpha", kinds).expect("store"));
        EngineManifestRoots::open(service, Arc::new(SystemClock) as Arc<dyn Clock>).expect("open")
    };
    let a = key("a.yaml");
    let b = key("b.yaml");
    {
        let roots = open();
        roots.declare(NS, ROOT, declaration(None)).await.expect("declare");
        let report = pass(
            "r1",
            true,
            vec![
                (a.clone(), document(DocumentPhase::Applied, "ha", None)),
                (b.clone(), document(DocumentPhase::Refused, "hb", Some("unmanaged object"))),
            ],
        );
        roots.record_pass(NS, ROOT, report).await.expect("pass");
    }
    let roots = open();
    let root = roots.root(NS, ROOT).await.expect("read").expect("root");
    assert_eq!(root.status.applied_revision.as_deref(), Some("r1"));
    assert_eq!(root.baseline(&a), Some("ha"));
    assert_eq!(root.status.documents[&b].phase, DocumentPhase::Refused);
    assert_eq!(root.status.documents[&a].live_hash, None, "the judgement is not stored");
}
