use std::sync::Arc;
use std::time::Duration;

use flotilla_config::ConfigStore;
use flotilla_protocol::HostName;
use flotilla_resources::{Convoy, ConvoyPhase, ConvoyStatus};
use flotilla_store::ResourceBackend;
use flotilla_store_memory::InMemoryBackend;

use super::support::create_identity_convoy;
use crate::in_process::InProcessDaemon;
use crate::testkits::discovery::fake_discovery;

async fn fixture() -> (Arc<InProcessDaemon>, ResourceBackend, tempfile::TempDir) {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"start-attach-test\"\n").unwrap();
    let backend = ResourceBackend::new(InMemoryBackend::default());
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
    let (daemon, backend, _temp) = fixture().await;
    create_identity_convoy(&backend, "convoy-slow", "coder", Some("flotilla")).await;
    let start = tokio::time::Instant::now();
    let result = daemon.wait_for_convoy_attach("flotilla", "convoy-slow", "coder@flotilla").await;
    assert!(result.is_ok(), "healthy slow provisioning failed: {result:?}");
    assert!(start.elapsed() >= Duration::from_secs(60));

    let (_, notice) = result.unwrap();
    assert!(notice.as_deref().unwrap().contains("still starting"));
    assert!(notice.as_deref().unwrap().contains("environment"));
    assert!(notice.unwrap().contains("flotilla attach coder@flotilla"));

    let convoys = backend.using::<Convoy>("flotilla");
    let convoy = convoys.get("convoy-slow").await.unwrap();
    convoys
        .update_status(
            "convoy-slow",
            &convoy.metadata.resource_version,
            &ConvoyStatus { phase: ConvoyPhase::Failed, message: Some("placement refused".into()), ..Default::default() },
        )
        .await
        .unwrap();
    let error = daemon.wait_for_convoy_attach("flotilla", "convoy-slow", "coder@flotilla").await.unwrap_err();
    assert!(error.contains("placement refused"), "{error}");
    // A failed work item must remain an error even before convoy phase rollup.
    let convoy = convoys.get("convoy-slow").await.unwrap();
    let mut status = ConvoyStatus { phase: ConvoyPhase::Active, ..Default::default() };
    status.work.insert(
        "work".into(),
        flotilla_resources::WorkState::builder().phase(flotilla_resources::WorkPhase::Failed).message("checkout failed".into()).build(),
    );
    convoys.update_status("convoy-slow", &convoy.metadata.resource_version, &status).await.unwrap();
    let error = daemon.wait_for_convoy_attach("flotilla", "convoy-slow", "coder@flotilla").await.unwrap_err();
    assert!(error.contains("checkout failed"), "{error}");
}

// Session creation and failure/deletion can interleave with the wait. Generate
// delays on both sides of the old 60-second boundary, including exactly 60.
#[hegel::test]
fn start_attach_lifecycle(tc: hegel::TestCase) {
    use super::support::{create_running_session, create_test_environment, test_meta};
    use flotilla_resources::{Host, HostSpec};
    use hegel::generators as gs;

    let delay = tc.draw(gs::integers::<u64>().min_value(59).max_value(61));
    let outcome = tc.draw(gs::integers::<u8>().min_value(0).max_value(2));
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().start_paused(true).build().unwrap();
    runtime.block_on(async {
        let (daemon, backend, _temp) = fixture().await;
        create_identity_convoy(&backend, "convoy-slow", "coder", Some("flotilla")).await;
        let host = daemon.local_host_id().unwrap().to_string();
        backend.using::<Host>("flotilla").create(&test_meta(&host), &HostSpec::default()).await.unwrap();
        let env = create_test_environment(&daemon, "start-env", &host).await;
        let actor = async {
            tokio::time::sleep(Duration::from_secs(delay)).await;
            let convoys = backend.using::<Convoy>("flotilla");
            match outcome {
                0 => create_running_session(&daemon, &env, "terminal-slow", "convoy-slow", "coder").await,
                1 => {
                    let convoy = convoys.get("convoy-slow").await.unwrap();
                    convoys
                        .update_status(
                            "convoy-slow",
                            &convoy.metadata.resource_version,
                            &ConvoyStatus { phase: ConvoyPhase::Failed, message: Some("placement refused".into()), ..Default::default() },
                        )
                        .await
                        .unwrap();
                }
                _ => {
                    convoys.delete("convoy-slow").await.unwrap();
                }
            }
        };
        let (result, ()) = tokio::join!(daemon.wait_for_convoy_attach("flotilla", "convoy-slow", "coder@flotilla"), actor);
        if delay > 60 {
            // The wait successfully yields before the later terminal/failure event.
            assert!(result.as_ref().unwrap().1.as_deref().unwrap().contains("still starting"));
        } else if outcome == 0 {
            // At the exact deadline either an attach plan or the successful notice is valid.
            assert!(result.is_ok(), "{result:?}");
            if delay < 60 {
                assert!(result.unwrap().0.is_some());
            }
        } else {
            // A failure/deletion published before the deadline must remain an error.
            // At exactly 60 seconds the timeout may legitimately be observed first.
            if delay < 60 {
                assert!(result.is_err(), "{result:?}");
            }
        }
        // After the event, the same public resolution/wait seams must reflect reality.
        match outcome {
            0 => assert!(daemon.resolve_attach_command_internal("convoy-slow").await.is_ok()),
            1 => assert!(daemon
                .wait_for_convoy_attach("flotilla", "convoy-slow", "coder@flotilla")
                .await
                .unwrap_err()
                .contains("placement refused")),
            _ => assert!(daemon.resolve_attach_command_internal("convoy-slow").await.is_err()),
        }
    });
}

// The fallback reports current convoy and vessel evidence, preserving detailed
// provisioning messages and rejecting a failed vessel before work rollup.
#[tokio::test]
async fn start_attach_stage_uses_vessel_status() {
    use super::support::test_meta;
    use flotilla_resources::{InputMeta, Vessel, VesselPhase, VesselSpec, VesselStatus, CONVOY_LABEL};
    use std::collections::BTreeMap;
    let (daemon, backend, _temp) = fixture().await;
    create_identity_convoy(&backend, "convoy-slow", "coder", Some("flotilla")).await;
    let convoy = backend.using::<Convoy>("flotilla").get("convoy-slow").await.unwrap();
    let vessels = backend.using::<Vessel>("flotilla");
    let spec = VesselSpec {
        convoy_ref: "convoy-slow".into(),
        vessel_name: "work".into(),
        placement_policy_ref: "test".into(),
        adopted_checkout_refs: BTreeMap::new(),
    };
    // Unrelated vessel failures must not contaminate this convoy's start.
    let unrelated = vessels.create(&test_meta("other"), &VesselSpec { convoy_ref: "other".into(), ..spec.clone() }).await.unwrap();
    vessels
        .update_status("other", &unrelated.metadata.resource_version, &VesselStatus { phase: VesselPhase::Failed, ..Default::default() })
        .await
        .unwrap();
    let mut vessel = vessels
        .create(
            &InputMeta::builder().name("slow-work".into()).labels(BTreeMap::from([(CONVOY_LABEL.into(), "convoy-slow".into())])).build(),
            &spec,
        )
        .await
        .unwrap();
    for (phase, message, expected) in [
        (VesselPhase::Pending, None, "placement"),
        (VesselPhase::Provisioning, None, "environment and checkouts"),
        (VesselPhase::Provisioning, Some("waiting for checkout work".to_string()), "waiting for checkout work"),
        (VesselPhase::Ready, None, "terminal session"),
        (VesselPhase::Failed, Some("placement refused".to_string()), "placement refused"),
    ] {
        vessel = vessels
            .update_status("slow-work", &vessel.metadata.resource_version, &VesselStatus { phase, message, ..Default::default() })
            .await
            .unwrap();
        let stage = daemon.convoy_start_stage("flotilla", &convoy).await;
        if phase == VesselPhase::Failed {
            assert!(stage.unwrap_err().contains(expected));
        } else {
            let stage = stage.unwrap();
            assert!(stage.contains("Pending") && stage.contains("vessel work") && stage.contains(expected), "{stage}");
        }
    }
}
