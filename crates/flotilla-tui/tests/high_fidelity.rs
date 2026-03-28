#[path = "support/high_fidelity.rs"]
mod high_fidelity;

use std::time::Duration;

use high_fidelity::HighFidelityHarness;

#[tokio::test]
async fn remote_checkout_removal_surfaces_progress_before_completion() {
    let mut harness = HighFidelityHarness::remote_checkout_removal().await.expect("build remote checkout removal harness");

    harness.wait_for_remote_checkout("feat-remote", Duration::from_secs(5)).await.expect("remote checkout should appear in leader app");
    harness.select_checkout("feat-remote").expect("select remote checkout");

    harness.press_remove_shortcut().await.expect("open delete confirmation");
    harness.wait_for_delete_confirm_loaded(Duration::from_secs(5)).await.expect("delete confirmation should load safety info");

    let confirm_output = harness.render_to_string();
    assert!(confirm_output.contains("Branch: feat-remote"), "expected loaded delete confirmation, got:\n{confirm_output}");

    harness.confirm_delete().await.expect("confirm checkout removal");
    harness.wait_for_remove_started(Duration::from_secs(5)).await.expect("remote checkout removal should start");
    harness
        .wait_for_progress_text("Remove checkout for branch feat-remote", Duration::from_secs(5))
        .await
        .expect("remote checkout removal progress should be visible before completion");

    let progress_output = harness.render_to_string();
    assert!(
        progress_output.contains("Remove checkout for branch feat-remote"),
        "expected in-flight progress in render, got:\n{progress_output}"
    );

    harness.release_remove();
    harness
        .wait_for_checkout_removed("feat-remote", Duration::from_secs(5))
        .await
        .expect("remote checkout should disappear after completion");

    let final_output = harness.render_to_string();
    assert!(
        !final_output.contains("Remove checkout for branch feat-remote"),
        "in-flight progress should clear after completion:\n{final_output}"
    );
}
