use flotilla_provisioning_testkit::Ending;
use flotilla_resources::{CheckoutPhase, ClonePhase, EnvironmentPhase};

// Every own-reconciler shape follows the same engine sequence while old phase remains stale.
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
            2 => {
                super::environment_reconciler::assert_environment_lifecycle(
                    [
                        EnvironmentPhase::Pending,
                        EnvironmentPhase::Provisioning,
                        EnvironmentPhase::Ready,
                        EnvironmentPhase::Failed,
                        EnvironmentPhase::Lost,
                    ][phase],
                    ending,
                )
                .await
            }
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
