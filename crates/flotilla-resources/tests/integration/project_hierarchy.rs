use std::collections::BTreeMap;

use flotilla_protocol::NodeId;
use flotilla_resources::{
    apply_manifest_resource_document, apply_resource_document, FleetDesignation, FleetDesignationSpec, InMemoryBackend, InputMeta, Project,
    ProjectHierarchy, ProjectSpec, ResourceBackend, SqliteBackend, FLEET_DESIGNATION_NAME,
};
use hegel::generators as gs;
use serde_json::json;

fn project(parent: Option<&str>) -> ProjectSpec {
    ProjectSpec::builder()
        .display_name("Project".into())
        .default_workflow_ref("work".into())
        .maybe_parent(parent.map(str::to_string))
        .build()
}

// #2718: nearest-first ancestry and sorted descendants agree for every tree,
// including implicit fleet edges, explicit edges and a fleet with no children.
#[hegel::test]
fn generated_parent_chains(tc: hegel::TestCase) {
    // Zero to twelve children, each explicitly parented to an earlier node or
    // implicitly to the fleet: covers empty, branching and maximum-depth trees.
    let count = tc.draw(gs::integers::<usize>().min_value(0).max_value(12));
    let mut declared = BTreeMap::from([("fleet".to_string(), None)]);
    let mut expected = BTreeMap::<String, Vec<String>>::from([("fleet".to_string(), vec![])]);
    for index in 0..count {
        let parent_index = tc.draw(gs::integers::<usize>().min_value(0).max_value(index));
        let explicit = tc.draw(gs::booleans());
        let parent = if parent_index == index || !explicit { "fleet".to_string() } else { format!("p{parent_index:02}") };
        let name = format!("p{index:02}");
        let mut ancestors = vec![parent.clone()];
        ancestors.extend(expected[&parent].clone());
        expected.insert(name.clone(), ancestors);
        declared.insert(name, explicit.then_some(parent));
    }
    let hierarchy = ProjectHierarchy::new(declared, Some("fleet".into())).expect("valid tree");
    assert_eq!(hierarchy.parent("fleet").expect("root"), None);
    for (name, ancestors) in &expected {
        assert_eq!(&hierarchy.ancestors(name).expect("ancestors"), ancestors);
        let descendants = expected.iter().filter(|(_, chain)| chain.contains(name)).map(|(name, _)| name.clone()).collect::<Vec<_>>();
        assert_eq!(hierarchy.descendants(name).expect("descendants"), descendants);
    }
}

// #2718: both operator and manifest apply refuse dangling parents and cycles,
// preserving the previous definition. They permit explicit and implicit parents.
#[tokio::test]
async fn apply_refuses_invalid_hierarchy_with_reasons() {
    for (backend, manifest) in [
        (ResourceBackend::InMemory(InMemoryBackend::default()), false),
        (ResourceBackend::InMemory(InMemoryBackend::default()), true),
        (ResourceBackend::Sqlite(SqliteBackend::open_in_memory().expect("sqlite")), false),
        (ResourceBackend::Sqlite(SqliteBackend::open_in_memory().expect("sqlite")), true),
    ] {
        let apply =
            |kind: &str, name: &str, spec| json!({"apiVersion":"flotilla.work/v1", "kind":kind,"metadata":{"name":name},"spec":spec});
        async fn submit(
            backend: &ResourceBackend,
            manifest: bool,
            document: serde_json::Value,
        ) -> Result<(), flotilla_resources::ResourceError> {
            if manifest {
                apply_manifest_resource_document(backend, "flotilla", document).await.map(|_| ())
            } else {
                apply_resource_document(backend, "flotilla", document).await.map(|_| ())
            }
        }
        let projects = backend.definitions::<Project>("flotilla");
        projects.apply(&InputMeta::builder().name("root".into()).build(), &project(None)).await.expect("root");
        submit(&backend, manifest, apply("FleetDesignation", "fleet", json!({"project":"root"}))).await.expect("designation");
        submit(&backend, manifest, apply("Project", "a", json!(project(None)))).await.expect("implicit");
        submit(&backend, manifest, apply("Project", "b", json!(project(Some("a"))))).await.expect("explicit");
        let hierarchy = ProjectHierarchy::load(&backend, "flotilla").await.expect("hierarchy");
        assert_eq!(hierarchy.ancestors("b").expect("chain"), ["a", "root"]);
        assert_eq!(hierarchy.descendants("root").expect("descendants"), ["a", "b"]);
        for (name, parent, reason) in
            [("a", "b", "cycle"), ("a", "a", "cycle"), ("a", "missing", "not declared"), ("root", "a", "cannot have a parent")]
        {
            let error = submit(&backend, manifest, apply("Project", name, json!(project(Some(parent))))).await.expect_err("refused");
            assert!(error.to_string().contains(reason), "{error}");
        }
        assert_eq!(projects.get("a").await.expect("unchanged").spec.parent, None);
        assert!(submit(&backend, manifest, apply("FleetDesignation", "other", json!({"project":"root"}))).await.is_err());
        assert!(submit(&backend, manifest, apply("FleetDesignation", "fleet", json!({"project":"missing"}))).await.is_err());
        assert!(submit(&backend, manifest, apply("FleetDesignation", "fleet", json!({"project":"b"}))).await.is_err());
        assert!(projects.delete("root").await.is_err());
        assert!(projects.delete("a").await.is_err());
        // Clearing an explicit parent restores the implicit fleet edge, including
        // through the causal Definition merge (null must be an authored value).
        submit(&backend, manifest, apply("Project", "b", json!(project(None)))).await.expect("clear parent");
        assert_eq!(ProjectHierarchy::load(&backend, "flotilla").await.expect("cleared hierarchy").ancestors("b").expect("chain"), ["root"]);
    }
}

// #2718 and ADR 0047: stored Projects written before parent existed retain
// their contents and decode with no declared parent. No corpus is regenerated.
#[test]
fn stored_projects_without_parent_decode() {
    let spec: ProjectSpec = serde_json::from_value(json!({"display_name":"Legacy", "default_workflow_ref":"work", "repositories":[]}))
        .expect("previous generation");
    assert_eq!(spec.parent, None);
    assert_eq!(spec.display_name, "Legacy");
    assert!(serde_json::to_value(spec).expect("encode")["parent"].is_null());
}

// #2718: fleet designation and a parent declared only at another root are
// available through Definitions federation; a child can be authored locally.
#[tokio::test]
async fn designation_and_parent_federate() {
    let source = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("fleet-store"));
    let target = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("member-store"));
    source.definitions::<Project>("flotilla").apply(&InputMeta::builder().name("root".into()).build(), &project(None)).await.expect("root");
    source
        .definitions::<FleetDesignation>("flotilla")
        .apply(&InputMeta::builder().name(FLEET_DESIGNATION_NAME.into()).build(), &FleetDesignationSpec {
            project: "root".into(),
            image_cache: None,
            image_gc: None,
        })
        .await
        .expect("designation");
    target
        .replica_writer::<Project>(NodeId::new("fleet-store"), "flotilla")
        .replace(&source.using::<Project>("flotilla").list().await.expect("projects"), chrono::Utc::now())
        .await
        .expect("replicate projects");
    target
        .replica_writer::<FleetDesignation>(NodeId::new("fleet-store"), "flotilla")
        .replace(&source.using::<FleetDesignation>("flotilla").list().await.expect("designation"), chrono::Utc::now())
        .await
        .expect("replicate fleet");
    target
        .definitions::<Project>("flotilla")
        .apply(&InputMeta::builder().name("child".into()).build(), &project(Some("root")))
        .await
        .expect("replica parent");
    let hierarchy = ProjectHierarchy::load(&target, "flotilla").await.expect("hierarchy");
    assert_eq!(hierarchy.fleet(), Some("root"));
    assert_eq!(hierarchy.ancestors("child").expect("ancestors"), ["root"]);
}

// #2718: closing a chain into a cycle is refused at every depth; a missing
// declared parent and an undeclared fleet root are also refused with reasons.
#[hegel::test]
fn generated_invalid_parent_chains(tc: hegel::TestCase) {
    // Includes self-cycles, two-node cycles and long cycles up to twelve nodes.
    let count = tc.draw(gs::integers::<usize>().min_value(1).max_value(12));
    let mut declared = BTreeMap::new();
    for index in 0..count {
        declared.insert(format!("p{index}"), Some(format!("p{}", (index + 1) % count)));
    }
    assert!(ProjectHierarchy::new(declared.clone(), None).expect_err("cycle refused").to_string().contains("cycle"));
    declared.insert(format!("p{}", count - 1), Some("missing".into()));
    assert!(ProjectHierarchy::new(declared, None).expect_err("dangling parent refused").to_string().contains("not declared"));
    assert!(ProjectHierarchy::new(BTreeMap::new(), Some("missing".into()))
        .expect_err("fleet must exist")
        .to_string()
        .contains("not declared"));
}

// #2718: racing local reparent operations cannot jointly admit a cycle. The
// namespace validation repeats under each embedded store's lock/transaction.
#[tokio::test]
async fn concurrent_reparenting_preserves_acyclicity() {
    for backend in
        [ResourceBackend::InMemory(InMemoryBackend::default()), ResourceBackend::Sqlite(SqliteBackend::open_in_memory().expect("sqlite"))]
    {
        let projects = backend.definitions::<Project>("flotilla");
        let a = InputMeta::builder().name("a".into()).build();
        let b = InputMeta::builder().name("b".into()).build();
        projects.apply(&a, &project(None)).await.expect("a");
        projects.apply(&b, &project(None)).await.expect("b");
        let a_spec = project(Some("b"));
        let b_spec = project(Some("a"));
        let (a_result, b_result) = tokio::join!(projects.apply(&a, &a_spec), projects.apply(&b, &b_spec));
        assert_ne!(a_result.is_ok(), b_result.is_ok(), "exactly one reparent must be refused");
        ProjectHierarchy::load(&backend, "flotilla").await.expect("acyclic after race");
    }
}

// #2718: designation admission and parenting the proposed fleet serialize
// across kinds. Whichever wins, the resulting fleet cannot have a parent.
#[tokio::test]
async fn designation_and_parent_writes_share_admission() {
    for backend in
        [ResourceBackend::InMemory(InMemoryBackend::default()), ResourceBackend::Sqlite(SqliteBackend::open_in_memory().expect("sqlite"))]
    {
        let projects = backend.definitions::<Project>("flotilla");
        let fleets = backend.definitions::<FleetDesignation>("flotilla");
        let a = InputMeta::builder().name("a".into()).build();
        let b = InputMeta::builder().name("b".into()).build();
        let fleet = InputMeta::builder().name("fleet".into()).build();
        projects.apply(&a, &project(None)).await.expect("a");
        projects.apply(&b, &project(None)).await.expect("b");
        let a_spec = project(Some("b"));
        let fleet_spec = FleetDesignationSpec { project: "a".into(), image_cache: None, image_gc: None };
        let (parent, designation) = tokio::join!(projects.apply(&a, &a_spec), fleets.apply(&fleet, &fleet_spec));
        assert_ne!(parent.is_ok(), designation.is_ok(), "one incompatible write must be refused");
        ProjectHierarchy::load(&backend, "flotilla").await.expect("valid hierarchy after race");
    }
}

// Federated admission is host-local: concurrent edits may form a merged cycle.
// Strict loads surface it, while inspection and unrelated deletion remain usable;
// reparenting one member repairs the graph. Exercise both embedded stores.
#[tokio::test]
async fn federated_invalid_graph_can_be_inspected_and_repaired() {
    for target in [ResourceBackend::InMemory(InMemoryBackend::default()), ResourceBackend::Sqlite(SqliteBackend::open_in_memory().unwrap())]
    {
        let source = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("source"));
        let target = target.with_local_root(NodeId::new("target"));
        let a = InputMeta::builder().name("a".into()).build();
        let b = InputMeta::builder().name("b".into()).build();
        source.definitions::<Project>("test").apply(&a, &project(None)).await.unwrap();
        target.definitions::<Project>("test").apply(&b, &project(None)).await.unwrap();
        async fn replicate(from: &ResourceBackend, to: &ResourceBackend, root: &str) {
            to.replica_writer::<Project>(NodeId::new(root), "test")
                .replace(&from.using::<Project>("test").list().await.unwrap(), chrono::Utc::now())
                .await
                .unwrap();
        }
        replicate(&source, &target, "source").await;
        replicate(&target, &source, "target").await;
        target.definitions::<Project>("test").apply(&InputMeta::builder().name("unrelated".into()).build(), &project(None)).await.unwrap();
        source.definitions::<Project>("test").apply(&a, &project(Some("b"))).await.unwrap();
        target.definitions::<Project>("test").apply(&b, &project(Some("a"))).await.unwrap();
        replicate(&source, &target, "source").await;
        assert!(ProjectHierarchy::load(&target, "test").await.unwrap_err().to_string().contains("cycle"));
        let inspection = ProjectHierarchy::load_for_inspection(&target, "test").await.unwrap();
        assert_eq!(inspection.parent("a").unwrap(), Some("b"));
        assert_eq!(inspection.descendants("a").unwrap(), ["b"]);
        target.definitions::<Project>("test").delete("unrelated").await.expect("unrelated deletion remains available");
        target.definitions::<Project>("test").apply(&b, &project(None)).await.expect("repair cycle");
        assert_eq!(ProjectHierarchy::load(&target, "test").await.unwrap().ancestors("a").unwrap(), ["b"]);
        // A remote disappearance leaves a dangling local child. It can be deleted
        // without traversing the missing ancestor, restoring a valid graph.
        source.definitions::<Project>("test").apply(&a, &project(None)).await.unwrap();
        replicate(&source, &target, "source").await;
        source.definitions::<Project>("test").delete("a").await.unwrap();
        target.definitions::<Project>("test").apply(&b, &project(Some("a"))).await.unwrap();
        replicate(&source, &target, "source").await;
        assert!(ProjectHierarchy::load(&target, "test").await.unwrap_err().to_string().contains("not declared"));
        target.definitions::<Project>("test").delete("b").await.expect("delete dangling child");
        ProjectHierarchy::load(&target, "test").await.expect("repaired graph");
    }
}

// Designation can be changed to another parentless Project or removed. Implicit
// edges follow the new designation, and removal returns to the bootstrap forest.
#[tokio::test]
async fn designation_reassignment_and_deletion() {
    for backend in
        [ResourceBackend::InMemory(InMemoryBackend::default()), ResourceBackend::Sqlite(SqliteBackend::open_in_memory().unwrap())]
    {
        let projects = backend.definitions::<Project>("test");
        for name in ["old", "new", "child"] {
            projects.apply(&InputMeta::builder().name(name.into()).build(), &project(None)).await.unwrap();
        }
        let fleets = backend.definitions::<FleetDesignation>("test");
        let meta = InputMeta::builder().name("fleet".into()).build();
        for root in ["old", "new"] {
            fleets.apply(&meta, &FleetDesignationSpec { project: root.into(), image_cache: None, image_gc: None }).await.unwrap();
            assert_eq!(ProjectHierarchy::load(&backend, "test").await.unwrap().ancestors("child").unwrap(), [root]);
            assert!(projects.delete(root).await.is_err());
        }
        fleets.delete("fleet").await.unwrap();
        assert!(ProjectHierarchy::load(&backend, "test").await.unwrap().ancestors("child").unwrap().is_empty());
        projects.delete("new").await.expect("former fleet is deletable");
    }
}

// Federation can leave independent dangling edges. Each valid reparent repair
// must be admitted without requiring every other broken chain to be fixed first.
#[tokio::test]
async fn apply_repairs_independent_invalid_edges_incrementally() {
    for backend in
        [ResourceBackend::InMemory(InMemoryBackend::default()), ResourceBackend::Sqlite(SqliteBackend::open_in_memory().unwrap())]
    {
        let source = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("source"));
        for name in ["parent-a", "parent-b"] {
            source.definitions::<Project>("test").apply(&InputMeta::builder().name(name.into()).build(), &project(None)).await.unwrap();
        }
        for (name, parent) in [("a", "parent-a"), ("b", "parent-b")] {
            source
                .definitions::<Project>("test")
                .apply(&InputMeta::builder().name(name.into()).build(), &project(Some(parent)))
                .await
                .unwrap();
        }
        // The replica snapshot arrives before either parent declaration.
        let mut incoming = source.using::<Project>("test").list().await.unwrap();
        incoming.items.retain(|object| object.metadata.name == "a" || object.metadata.name == "b");
        backend.replica_writer::<Project>(NodeId::new("source"), "test").replace(&incoming, chrono::Utc::now()).await.unwrap();
        let projects = backend.definitions::<Project>("test");
        let a = InputMeta::builder().name("a".into()).build();
        let b = InputMeta::builder().name("b".into()).build();
        projects.apply(&a, &project(None)).await.expect("first independent repair");
        let inspection = ProjectHierarchy::load_for_inspection(&backend, "test").await.unwrap();
        assert!(inspection.ancestors("a").unwrap().is_empty());
        assert!(inspection.ancestors("b").is_err());
        assert!(projects.apply(&a, &project(Some("missing"))).await.is_err(), "new dangling edge is refused");
        assert!(projects.apply(&a, &project(Some("b"))).await.is_err(), "written ancestry must be valid");
        projects.apply(&b, &project(None)).await.expect("second independent repair");
        ProjectHierarchy::load(&backend, "test").await.expect("repaired namespace");
    }
}

// A designation may arrive before its Project during bootstrap. Inspection
// preserves it and emits a warning identifying the namespace and missing root.
#[tokio::test]
async fn inspection_warns_about_undeclared_fleet_project() {
    use std::{
        io::Write,
        sync::{Arc, Mutex},
    };

    use tracing::instrument::WithSubscriber;
    #[derive(Clone)]
    struct Capture(Arc<Mutex<Vec<u8>>>);
    impl Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let source = ResourceBackend::InMemory(InMemoryBackend::default());
    source.definitions::<Project>("bootstrap").apply(&InputMeta::builder().name("root".into()).build(), &project(None)).await.unwrap();
    source
        .definitions::<FleetDesignation>("bootstrap")
        .apply(&InputMeta::builder().name("fleet".into()).build(), &FleetDesignationSpec {
            project: "root".into(),
            image_cache: None,
            image_gc: None,
        })
        .await
        .unwrap();
    let target = ResourceBackend::InMemory(InMemoryBackend::default());
    target
        .replica_writer::<FleetDesignation>(NodeId::new("source"), "bootstrap")
        .replace(&source.using::<FleetDesignation>("bootstrap").list().await.unwrap(), chrono::Utc::now())
        .await
        .unwrap();
    let captured = Capture(Arc::new(Mutex::new(Vec::new())));
    let writer = captured.clone();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_max_level(tracing::Level::WARN)
        .with_writer(move || writer.clone())
        .finish();
    let hierarchy = ProjectHierarchy::load_for_inspection(&target, "bootstrap").with_subscriber(subscriber).await.unwrap();
    assert_eq!(hierarchy.fleet(), Some("root"));
    let output = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
    assert!(output.contains("WARN") && output.contains("FleetDesignation references an undeclared Project"), "{output}");
    assert!(output.contains("bootstrap") && output.contains("root"), "{output}");
}
