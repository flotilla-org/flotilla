use flotilla_protocol::LifecycleReading;
use flotilla_provisioning_testkit::{Ending, EnvironmentFixture};
use flotilla_resources::{environment_records::EnvironmentRecords, CheckoutPhase, ClonePhase, EnvironmentSpec, HostDirectEnvironmentSpec};

// Every own-reconciler shape follows the same engine sequence while old phase remains stale; an
// Environment has no old status, and its readers see the engine's reading through its port.
// Draw all kinds, every old phase, success/loss/release and empty/Unicode refusal causes.
#[hegel::test]
fn lifecycle_phases_and_endings_override_old_status(tc: hegel::TestCase) {
    use hegel::generators as gs;
    let kind = tc.draw(gs::integers::<usize>().min_value(0).max_value(3));
    let phase = tc.draw(gs::integers::<usize>().min_value(0).max_value(4));
    let ending = match tc.draw(gs::integers::<usize>().min_value(0).max_value(4)) {
        0 => Ending::Ready("/work"),
        1 => Ending::Lost("/work"),
        2 => Ending::Released,
        3 => Ending::GaveUp(""),
        _ => Ending::GaveUp("失敗"),
    };
    tokio::runtime::Runtime::new().expect("runtime").block_on(async {
        match kind {
            0 => {
                super::clone_reconciler::assert_clone_lifecycle(
                    [ClonePhase::Pending, ClonePhase::Cloning, ClonePhase::Ready, ClonePhase::Failed][phase % 4],
                    ending,
                )
                .await
            }
            1 => {
                super::checkout_reconciler::assert_checkout_lifecycle(
                    [CheckoutPhase::Pending, CheckoutPhase::Ready, CheckoutPhase::Failed, CheckoutPhase::Gone][phase % 4],
                    ending,
                )
                .await
            }
            2 => assert_environment_lifecycle(ending),
            _ => {
                super::image_build::assert_image_lifecycle(
                    [
                        flotilla_resources::ImageBuildPhase::Queued,
                        flotilla_resources::ImageBuildPhase::Building,
                        flotilla_resources::ImageBuildPhase::Built,
                        flotilla_resources::ImageBuildPhase::Failed,
                    ][phase % 4],
                    ending,
                )
                .await
            }
        }
    });
}

// An Environment's readers see the engine's sequence through the environment port: in progress,
// retrying after a retryable failure, then the ending; a read of a broken store is an error.
fn assert_environment_lifecycle(ending: Ending<'_>) {
    let fixture = EnvironmentFixture::default();
    let spec = EnvironmentSpec {
        host_direct: Some(HostDirectEnvironmentSpec { host_ref: "host".into(), repo_default_dir: "/work".into() }),
        ..Default::default()
    };
    let reading = || fixture.environments.get("env", None).expect("read").expect("held").reading;
    fixture.request("env", &spec, None);
    assert_eq!(reading(), LifecycleReading::InProgress);
    fixture.retry("env", "retry");
    assert!(matches!(reading(), LifecycleReading::Retrying { failures: 1, ref cause, .. } if cause.contains("retry")), "{:?}", reading());
    fixture.finish("env", ending);
    match ending {
        Ending::Ready(identity) => assert_eq!(reading(), LifecycleReading::Ready { identity: identity.into() }),
        Ending::Lost(_) => assert_eq!(reading(), LifecycleReading::Lost { reason: None }),
        Ending::Released => assert_eq!(reading(), LifecycleReading::Released),
        Ending::Failed(reason) => assert_eq!(reading(), LifecycleReading::Failed { reason: reason.into() }),
        Ending::GaveUp(cause) => {
            assert!(matches!(reading(), LifecycleReading::GaveUp { cause: ref read } if read.contains(cause)), "{:?}", reading())
        }
    }
    fixture.poison();
    assert!(fixture.environments.get("env", None).expect_err("read error").contains("poisoned"));
}
