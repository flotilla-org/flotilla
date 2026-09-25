use std::collections::BTreeSet;

use flotilla_resources::{Forge, ForgeKind, ForgeSpec, InMemoryBackend, InputMeta, RepositoryIdentity, RepositorySpec, ResourceBackend};

fn lab() -> ForgeSpec {
    ForgeSpec::builder()
        .forge_id("flotilla-lab".to_string())
        .kind(ForgeKind::Forgejo)
        .hosts(BTreeSet::from([
            "forgejo.lab.flotilla.work".to_string(),
            "manchego.lab.flotilla.work".to_string(),
            "forgejo-manchego".to_string(),
        ]))
        .https_url("https://forgejo.lab.flotilla.work".to_string())
        .git_ssh_host("manchego.lab.flotilla.work".to_string())
        .build()
}

#[test]
fn equivalent_urls_have_one_forge_relative_repository_key() {
    let forge = lab();
    let forms = [
        "https://forgejo.lab.flotilla.work/robert/ghostty-ops",
        "https://forgejo.lab.flotilla.work/robert/ghostty-ops.git",
        "https://manchego.lab.flotilla.work/robert/ghostty-ops",
        "https://manchego.lab.flotilla.work/robert/ghostty-ops.git",
        "forgejo-manchego:robert/ghostty-ops",
        "forgejo-manchego:robert/ghostty-ops.git",
        "git@manchego.lab.flotilla.work:robert/ghostty-ops.git",
        "ssh://git@forgejo.lab.flotilla.work/robert/ghostty-ops.git",
    ];
    let keys = forms
        .iter()
        .map(|form| {
            let canonical = flotilla_resources::canonicalize_repo_url(form).expect("canonical remote");
            let spec = RepositorySpec::remote(canonical).expect("repository").on_forge(&forge).expect("forge identity");
            assert_eq!(spec.identity(), &RepositoryIdentity::Forge {
                forge_ref: "flotilla-lab".to_string(),
                owner: "robert".to_string(),
                repo_name: "ghostty-ops".to_string(),
            });
            spec.key()
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(keys.len(), 1);
    let clone_keys = forms
        .iter()
        .map(|form| {
            let canonical = flotilla_resources::canonicalize_repo_url(form).expect("canonical remote");
            RepositorySpec::remote(canonical)
                .expect("repository")
                .on_forge(&forge)
                .expect("forge identity")
                .clone_key("host-direct")
                .expect("clone identity")
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(clone_keys.len(), 1);
}

#[tokio::test]
async fn forge_definition_is_visible_from_a_replica() {
    let source_root = flotilla_protocol::NodeId::new("source");
    let source = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(source_root.clone());
    let destination = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(flotilla_protocol::NodeId::new("destination"));
    source
        .definitions::<Forge>("flotilla")
        .create(&InputMeta::builder().name("flotilla-lab".to_string()).build(), &lab())
        .await
        .expect("forge");
    let snapshot = source.using::<Forge>("flotilla").list().await.expect("source forges");
    destination.replica_writer::<Forge>(source_root, "flotilla").replace(&snapshot, chrono::Utc::now()).await.expect("replicate forges");
    let replica = destination.definitions::<Forge>("flotilla").get("flotilla-lab").await.expect("replicated forge");
    assert_eq!(replica.spec, lab());
}

#[tokio::test]
async fn malformed_forge_definition_is_rejected_when_authored() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let forges = backend.definitions::<Forge>("flotilla");
    let wrong_name = forges.create(&InputMeta::builder().name("other".to_string()).build(), &lab()).await;
    assert!(wrong_name.expect_err("forge_id must match resource name").to_string().contains("forge_id"));
    let mut no_hosts = lab();
    no_hosts.hosts.clear();
    let invalid_hosts = forges.create(&InputMeta::builder().name("flotilla-lab".to_string()).build(), &no_hosts).await;
    assert!(invalid_hosts.expect_err("empty aliases must be rejected").to_string().contains("hosts"));
    let mut insecure_front = lab();
    insecure_front.https_url = "http://forgejo.lab.flotilla.work".to_string();
    let invalid_front = forges.create(&InputMeta::builder().name("flotilla-lab".to_string()).build(), &insecure_front).await;
    assert!(invalid_front.expect_err("HTTPS front is required").to_string().contains("HTTPS"));
}
