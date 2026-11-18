use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use flotilla_config::ConfigStore;
use flotilla_protocol::HostName;
use flotilla_resources::Convoy;
use flotilla_store::ResourceBackend;
use flotilla_store_memory::engine_home_backend;

use super::support::{convoy_engine, create_identity_convoy};
use crate::in_process::{InProcessDaemon, CONVOY_ATTACH_WAIT};
use crate::testkits::discovery::fake_discovery;

async fn fixture() -> (Arc<InProcessDaemon>, ResourceBackend, tempfile::TempDir) {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"start-attach-test\"\n").unwrap();
    let backend = engine_home_backend();
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::new("local"),
        backend.clone(),
        Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
    )
    .await;
    (daemon, backend, temp)
}

// A healthy convoy that has no session after the bound returns a successful
// still-starting notice. A subsequently recorded failure remains an error.
#[tokio::test(start_paused = true)]
async fn slow_start_attach_does_not_fail() {
    use super::support::test_engine;
    use flotilla_resources::vessel_records::VesselStep;
    let (daemon, backend, _temp) = fixture().await;
    let slow = create_identity_convoy(&daemon, "convoy-slow", "coder", "flotilla").await;
    let start = tokio::time::Instant::now();
    let result = daemon.wait_for_convoy_attach("flotilla", &slow, "coder@flotilla").await;
    assert!(result.is_ok(), "healthy slow provisioning failed: {result:?}");
    assert!(start.elapsed() >= CONVOY_ATTACH_WAIT);

    let (_, notice) = result.unwrap();
    assert!(notice.as_deref().unwrap().contains("still starting"));
    assert!(notice.as_deref().unwrap().contains("environment"));
    assert!(notice.unwrap().contains("flotilla attach coder@flotilla"));

    let convoys = backend.using::<Convoy>("flotilla");
    let convoy = convoys.get(&slow).await.unwrap();
    convoy_engine(&daemon).fail(&convoy, "operator", "placement refused").await.unwrap();
    let error = daemon.wait_for_convoy_attach("flotilla", &slow, "coder@flotilla").await.unwrap_err();
    assert!(error.contains("placement refused"), "{error}");
    // A failed work item is an error with its own message: the next launch's vessel fails, which
    // fails its work, and so its convoy.
    let next = create_identity_convoy(&daemon, "convoy-slow", "coder", "flotilla").await;
    let engine = test_engine(&daemon);
    let vessel = engine.vessels.request(&next, "work", "local");
    engine.vessels.step(&vessel, VesselStep::Failed { cause: "checkout failed".into(), retryable: false });
    let error = daemon.wait_for_convoy_attach("flotilla", &next, "coder@flotilla").await.unwrap_err();
    assert!(error.contains("checkout failed"), "{error}");
}

// Session creation and failure/deletion can interleave with the wait. Generate
// delays on both sides of the old 60-second boundary, including exactly 60.
#[hegel::test]
fn start_attach_lifecycle(tc: hegel::TestCase) {
    use super::support::{create_running_session, create_test_environment, test_engine, test_meta};
    use flotilla_resources::{Host, HostSpec};
    use hegel::generators as gs;

    let boundary = CONVOY_ATTACH_WAIT.as_secs();
    let delay = tc.draw(gs::integers::<u64>().min_value(boundary - 1).max_value(boundary + 1));
    // A convoy ends on the engine and is never deleted, so the wait sees a session or a failure.
    let outcome = tc.draw(gs::integers::<u8>().min_value(0).max_value(1));
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().start_paused(true).build().unwrap();
    runtime.block_on(async {
        let (daemon, backend, _temp) = fixture().await;
        let slow = create_identity_convoy(&daemon, "convoy-slow", "coder", "flotilla").await;
        let host = daemon.local_host_id().unwrap().to_string();
        backend.using::<Host>("flotilla").create(&test_meta(&host), &HostSpec::default()).await.unwrap();
        let engine = test_engine(&daemon);
        let env = create_test_environment(&engine.environments, "start-env", &host);
        let actor = async {
            tokio::time::sleep(Duration::from_secs(delay)).await;
            let convoys = backend.using::<Convoy>("flotilla");
            if outcome == 0 {
                create_running_session(&engine, &env, "terminal-slow", &slow, "coder").await;
            } else {
                let convoy = convoys.get(&slow).await.unwrap();
                convoy_engine(&daemon).fail(&convoy, "operator", "placement refused").await.unwrap();
            }
        };
        let (result, ()) = tokio::join!(daemon.wait_for_convoy_attach("flotilla", &slow, "coder@flotilla"), actor);
        if delay > boundary {
            // The wait successfully yields before the later terminal/failure event.
            assert!(result.as_ref().unwrap().1.as_deref().unwrap().contains("still starting"));
        } else if outcome == 0 {
            // At the exact deadline either an attach plan or the successful notice is valid.
            assert!(result.is_ok(), "{result:?}");
            if delay < boundary {
                assert!(result.unwrap().0.is_some());
            }
        } else {
            // A failure published before the deadline must remain an error.
            // At exactly 60 seconds the timeout may legitimately be observed first.
            if delay < boundary {
                assert!(result.is_err(), "{result:?}");
            }
        }
        // After the event, the same daemon resolution/wait seams must reflect reality.
        if outcome == 0 {
            assert!(daemon.resolve_attach_command_internal(&slow).await.is_ok());
        } else {
            assert!(daemon.wait_for_convoy_attach("flotilla", &slow, "coder@flotilla").await.unwrap_err().contains("placement refused"));
        }
    });
}

// The fallback reports current convoy and vessel evidence, preserving detailed
// provisioning messages and rejecting a failed vessel before work rollup.
#[tokio::test]
async fn start_attach_stage_uses_vessel_status() {
    use super::support::test_engine;
    use flotilla_resources::vessel_records::VesselStep;
    let (daemon, backend, _temp) = fixture().await;
    let engine = test_engine(&daemon);
    let slow = create_identity_convoy(&daemon, "convoy-slow", "coder", "flotilla").await;
    let convoy = backend.using::<Convoy>("flotilla").get(&slow).await.unwrap();
    // Unrelated vessel failures must not contaminate this convoy's start.
    let unrelated = engine.vessels.request("other", "work", "local");
    engine.vessels.step(&unrelated, VesselStep::Failed { cause: "other refused".into(), retryable: false });
    let launch = engine.vessels.request(&slow, "work", "local");
    let relaunch = || engine.vessels.request(&slow, "work", "local");
    // The engine has no Pending reading: a launch is requested in progress. Interrupted is read
    // from the crew's terminals (vessel_records' tests), which this identity convoy has none of.
    // An ended launch reads its reason, so Lost shows the reason written.
    let steps: [(Option<VesselStep>, &str); 5] = [
        (None, "environment and checkouts"),
        (Some(VesselStep::Failed { cause: "waiting for checkout work".into(), retryable: true }), "waiting for checkout work"),
        (Some(VesselStep::Realised { environment: "slow-env".into() }), "terminal session"),
        (Some(VesselStep::Lost { reason: "environment disappeared".into() }), "environment disappeared"),
        (Some(VesselStep::Failed { cause: "placement refused".into(), retryable: false }), "placement refused"),
    ];
    let mut failed = false;
    for (step, expected) in steps {
        if let Some(step) = step {
            failed = matches!(step, VesselStep::Failed { retryable: false, .. });
            // The lost launch is replaced before the next one fails.
            let current = if failed { relaunch() } else { launch.clone() };
            engine.vessels.step(&current, step);
        }
        if failed {
            // A failed vessel fails its work and so its convoy, and an ended convoy's vessels are
            // listed no more: the start reads the failure, with the vessel's reason, from the convoy.
            let status = backend.using::<Convoy>("flotilla").get(&slow).await.unwrap().status.unwrap();
            assert_eq!(status.phase, flotilla_resources::ConvoyPhase::Failed);
            assert!(status.work["work"].message.as_deref().is_some_and(|message| message.contains(expected)), "{status:?}");
            continue;
        }
        let stage = daemon.convoy_start_stage("flotilla", &convoy).await.unwrap();
        // An admitted convoy whose work is ready to start reads Active.
        assert!(stage.starts_with("active;") && stage.contains("vessel work") && stage.contains(expected), "{stage}");
    }
}

// Attach resolves a repository's project to the one a declaration owns. A manifest that took the
// project over leaves it no bootstrap repository, and it still wins over a project generated for
// the whole repository beside it.
#[tokio::test]
async fn attach_context_is_the_project_a_manifest_declares() {
    use flotilla_resources::definitions::{stamp, DefinitionOrigin};
    use flotilla_resources::{
        definition_provenance::WHOLE_REPOSITORY_PROJECT_MANAGER, InputMeta, Project, ProjectRepositorySpec, ProjectSpec, Repository,
        RepositorySpec, MANAGED_BY_LABEL,
    };

    let (daemon, backend, _temp) = fixture().await;
    let repository = RepositorySpec::remote("https://github.com/example/app").expect("repository");
    let key = repository.key();
    flotilla_store::ensure_repository(&backend.using::<Repository>("flotilla"), &key, &repository).await.expect("repository");
    let spec = |name: &str| {
        ProjectSpec::builder()
            .display_name(name.to_owned())
            .repositories(vec![ProjectRepositorySpec::builder().repo(key.clone()).build()])
            .build()
    };
    let root = flotilla_resources::manifest_roots::RootDeclaration {
        host: "local".into(),
        path: "/ops".into(),
        source: "ops".into(),
        binding: None,
        ops: false,
    };
    daemon.manifest_roots().declare("flotilla", "ops", root).await.expect("the manifest root");
    let (labels, annotations) = stamp(&DefinitionOrigin::Manifest {
        root: "ops".into(),
        source: Some("ops".into()),
        path: Some("projects/app.yaml".into()),
        revision: "first".into(),
        charter: None,
        materialized_project: None,
    });
    let projects = backend.definitions::<Project>("flotilla");
    projects
        .create(&InputMeta::builder().name("app".into()).labels(labels).annotations(annotations).build(), &spec("app"))
        .await
        .expect("the manifest's project");
    let generated = BTreeMap::from([(MANAGED_BY_LABEL.to_string(), WHOLE_REPOSITORY_PROJECT_MANAGER.to_string())]);
    projects
        .create(&InputMeta::builder().name("app-repo".into()).labels(generated).build(), &spec("app-repo"))
        .await
        .expect("the generated project");
    let context =
        daemon.attach_resolver().attach_project_context(Some(&flotilla_protocol::RepoSelector::Repository(key))).await.expect("context");
    assert_eq!(context.as_deref(), Some("app"));
}
