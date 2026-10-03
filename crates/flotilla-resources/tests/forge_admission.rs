use std::collections::BTreeSet;

use flotilla_protocol::NodeId;
use flotilla_resources::{
    apply_manifest_resource_document, apply_resource_document, Forge, ForgeKind, ForgeSpec, InMemoryBackend, InputMeta, ResourceBackend,
    SqliteBackend,
};
use hegel::generators as gs;

fn declaration(id: &str, host: &str, path: &str) -> ForgeSpec {
    ForgeSpec::builder()
        .forge_id(id.into())
        .kind(ForgeKind::Forgejo)
        .hosts(BTreeSet::from([host.into()]))
        .https_url(format!("https://{host}{path}"))
        .git_ssh_host(host.into())
        .build()
}

// #2159: overlap means a shared issue-service URL, including canonical hosts,
// declared aliases and SSH aliases, with exact installation path boundaries.
#[hegel::test]
fn overlap_matches_shared_issue_services(tc: hegel::TestCase) {
    // Cross empty, trailing slash, sibling, textual-prefix and nested paths;
    // draw each host source separately and vary hostname case.
    let paths = ["", "/", "/forge", "/forge/", "/forgejo", "/forge/nested", "/other"];
    let left_path = paths[tc.draw(gs::integers::<usize>().min_value(0).max_value(paths.len() - 1))];
    let right_path = paths[tc.draw(gs::integers::<usize>().min_value(0).max_value(paths.len() - 1))];
    let shared = tc.draw(gs::booleans());
    let source = tc.draw(gs::integers::<usize>().min_value(0).max_value(2));
    let mut left = declaration("left", "left.example", left_path);
    let right = declaration("right", "RIGHT.example", right_path);
    if shared {
        match source {
            0 => left.https_url = format!("https://right.example{left_path}"),
            1 => {
                left.hosts.insert("right.example".into());
            }
            _ => left.git_ssh_host = "right.example".into(),
        }
    }
    let expected = shared && left_path.trim_end_matches('/') == right_path.trim_end_matches('/');
    assert_eq!(left.overlaps_issue_service(&right), expected);
    assert_eq!(right.overlaps_issue_service(&left), expected, "symmetric ownership");
}

async fn admission_contract(backend: ResourceBackend) {
    let resolver = backend.using::<Forge>("dev");
    let first = declaration("first", "forge.example", "/forge");
    let meta = InputMeta::builder().name("first".into()).build();
    let created = resolver.create(&meta, &first).await.expect("first installation");
    let mut duplicate = declaration("duplicate", "other.example", "/forge/");
    duplicate.hosts.insert("FORGE.example".into());
    let duplicate_meta = InputMeta::builder().name("duplicate".into()).build();
    // Refusal must happen on writes, leaving the original declaration usable.
    assert!(resolver.create(&duplicate_meta, &duplicate).await.expect_err("alias overlap refused").to_string().contains("first"));
    assert_eq!(resolver.list().await.expect("list").items.len(), 1);
    resolver.update(&meta, &created.metadata.resource_version, &first).await.expect("self reapply permitted");
    for (id, path) in [("sibling", "/other"), ("textual", "/forgejo"), ("nested", "/forge/nested")] {
        let spec = declaration(id, "forge.example", path);
        let meta = InputMeta::builder().name(id.into()).build();
        let object = resolver.create(&meta, &spec).await.expect("distinct installation");
        let mut colliding = spec;
        colliding.https_url = "https://forge.example/forge".into();
        assert!(resolver.update(&meta, &object.metadata.resource_version, &colliding).await.is_err(), "update cannot introduce overlap");
        assert_eq!(resolver.get(id).await.expect("unchanged object").spec.https_url, object.spec.https_url);
    }
    // Operator and manifest admission use the same authoritative refusal.
    let document = serde_json::json!({"apiVersion":"flotilla.work/v1", "kind":"Forge", "metadata":{"name":"duplicate"}, "spec": duplicate});
    assert!(apply_resource_document(&backend, "dev", document.clone()).await.is_err());
    assert!(apply_manifest_resource_document(&backend, "dev", document).await.is_err());
    // Namespace isolation: the same ownership is valid in another namespace.
    backend.using::<Forge>("other").create(&duplicate_meta, &duplicate).await.expect("independent namespace");
    // The built-in GitHub installation is a fallback: one explicit owner wins.
    let mut github = declaration("public", "github.com", "");
    github.kind = ForgeKind::Github;
    let github_meta = InputMeta::builder().name("public".into()).build();
    resolver.create(&github_meta, &github).await.expect("explicit builtin override");
    let mut second = github;
    second.forge_id = "second-public".into();
    assert!(resolver.create(&InputMeta::builder().name("second-public".into()).build(), &second).await.is_err());
    // Admission also considers already-visible replicated definitions.
    let remote = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("remote-root"));
    let remote_spec = declaration("remote", "replica.example", "/installation");
    remote
        .definitions::<Forge>("dev")
        .create(&InputMeta::builder().name("remote".into()).build(), &remote_spec)
        .await
        .expect("remote forge");
    let listed = remote.using::<Forge>("dev").list().await.expect("remote list");
    backend.replica_writer::<Forge>(NodeId::new("remote-root"), "dev").replace(&listed, chrono::Utc::now()).await.expect("replicate forge");
    let mut collision = remote_spec;
    collision.forge_id = "local-collision".into();
    assert!(backend
        .definitions::<Forge>("dev")
        .create(&InputMeta::builder().name("local-collision".into()).build(), &collision)
        .await
        .is_err());
    // Resolved tombstones, including legacy deletion markers, are excluded by
    // the merged definitions read and must not retain issue-service ownership.
    let retiring = declaration("retiring", "retired.example", "/installation");
    remote
        .definitions::<Forge>("dev")
        .create(&InputMeta::builder().name("retiring".into()).build(), &retiring)
        .await
        .expect("retiring forge");
    remote.definitions::<Forge>("dev").delete("retiring").await.expect("causal deletion");
    let mut tombstones = remote.using::<Forge>("dev").list().await.expect("remote tombstones");
    let retiring_record = tombstones.items.iter_mut().find(|object| object.metadata.name == "retiring").expect("retiring record");
    retiring_record.metadata.merge = None; // Previous-generation deletion metadata is synthesised on read.
    backend
        .replica_writer::<Forge>(NodeId::new("remote-root"), "dev")
        .replace(&tombstones, chrono::Utc::now())
        .await
        .expect("replicate deletion");
    assert!(backend.definitions::<Forge>("dev").list().await.expect("definitions").iter().all(|object| object.metadata.name != "retiring"));
    let mut replacement = retiring;
    replacement.forge_id = "replacement".into();
    backend
        .definitions::<Forge>("dev")
        .create(&InputMeta::builder().name("replacement".into()).build(), &replacement)
        .await
        .expect("tombstone releases ownership");
    // Two concurrent local admissions cannot both acquire the same service.
    let a = declaration("race-a", "race.example", "");
    let b = declaration("race-b", "race.example", "");
    let ma = InputMeta::builder().name("race-a".into()).build();
    let mb = InputMeta::builder().name("race-b".into()).build();
    let (a, b) = tokio::join!(resolver.create(&ma, &a), resolver.create(&mb, &b));
    assert_ne!(a.is_ok(), b.is_ok(), "one atomic admission wins");
}

// Run one behavioral contract against each authoritative store.
#[tokio::test]
async fn authoritative_admission_contract() {
    admission_contract(ResourceBackend::InMemory(InMemoryBackend::default())).await;
    admission_contract(ResourceBackend::Sqlite(SqliteBackend::open_in_memory().expect("sqlite"))).await;
}
