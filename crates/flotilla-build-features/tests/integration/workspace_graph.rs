use std::{path::Path, process::Command};

fn check(arguments: &[&str]) {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().and_then(Path::parent).expect("workspace root");
    let python = if cfg!(windows) { "python" } else { "python3" };
    // Process boundary: exercise the real Cargo-metadata checker through its CLI.
    // CI forces Cargo colors; machine-readable tree output must still be uncolored.
    let output = Command::new(python)
        .args(arguments)
        .env("CARGO_TERM_COLOR", "always")
        .current_dir(root)
        .output()
        .expect("run build graph check with Python 3");
    assert!(
        output.status.success(),
        "build graph check failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

#[test]
fn default_build_and_test_features_are_uniform_and_test_dependencies_point_downward() {
    // Glue: #2747 requires stable feature selections and no upward dev-dependencies.
    check(&["ci/build-graph/check.py"]);
}

#[test]
fn build_graph_guard_catches_regressions() {
    // Glue: run the guard's finite graph/feature contracts in existing workspace CI.
    check(&["-m", "unittest", "discover", "-s", "ci/build-graph", "-p", "test_check.py"]);
}
