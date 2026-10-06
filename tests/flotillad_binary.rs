use std::{collections::BTreeSet, process::Command};

#[test]
fn installed_package_exposes_flotillad_binary() {
    let flotillad = env!("CARGO_BIN_EXE_flotillad");
    let status = Command::new(flotillad).arg("--help").status().expect("flotillad help should run");

    assert!(status.success(), "flotillad --help should succeed");
}

#[test]
fn binaries_report_their_wire_generation_and_protocol_version() {
    // Every Cargo binary target must report the generated identity, so adding an
    // entry point without initialization cannot silently advertise "unknown".
    let binaries = [("flotilla", env!("CARGO_BIN_EXE_flotilla")), ("flotillad", env!("CARGO_BIN_EXE_flotillad"))];
    // Cargo is the real process boundary that discovers explicit and automatic binary targets.
    let output = Command::new(env!("CARGO"))
        .args(["metadata", "--no-deps", "--locked", "--format-version", "1"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("discover Cargo binary targets");
    assert!(output.status.success(), "cargo metadata failed: {}", String::from_utf8_lossy(&output.stderr));
    let metadata: serde_json::Value = serde_json::from_slice(&output.stdout).expect("Cargo metadata JSON");
    let package = metadata["packages"]
        .as_array()
        .expect("metadata packages")
        .iter()
        .find(|package| package["name"] == env!("CARGO_PKG_NAME"))
        .expect("root package");
    let targets: BTreeSet<_> = package["targets"]
        .as_array()
        .expect("package targets")
        .iter()
        .filter(|target| target["kind"].as_array().expect("target kinds").iter().any(|kind| kind == "bin"))
        .map(|target| target["name"].as_str().expect("binary target name"))
        .collect();
    assert_eq!(targets, binaries.iter().map(|(name, _)| *name).collect(), "every binary needs identity coverage");
    for (_, binary) in binaries {
        let output = Command::new(binary).arg("--version").output().expect("binary version should run");

        assert!(output.status.success(), "{} --version should succeed", binary);
        let stdout = String::from_utf8(output.stdout).expect("version output should be UTF-8");
        assert!(
            stdout.contains(&format!("wire={}", env!("FLOTILLA_BUILD_ID"))),
            "{} should report its wire generation, got {stdout:?}",
            binary
        );
        assert!(
            stdout.contains(&format!("proto={}", flotilla_protocol::PROTOCOL_VERSION)),
            "{} should report its peer protocol version, got {stdout:?}",
            binary
        );
    }
}

// Glue: the final executable injects one diagnostic identity shared by client and daemon.
// This integration binary isolates process-wide identity initialization from library tests.
#[test]
fn executable_identity_is_shared_with_libraries() {
    assert_eq!(flotilla_core::build_info::build_id(), "unknown");
    assert!(std::panic::catch_unwind(|| flotilla_core::build_info::initialize_build_id("")).is_err());
    assert_eq!(flotilla_core::build_info::build_id(), "unknown");
    flotilla_core::build_info::initialize_build_id(env!("FLOTILLA_BUILD_ID"));
    assert_eq!(flotilla_core::build_info::build_id(), env!("FLOTILLA_BUILD_ID"));
    assert_eq!(flotilla_client::build_id(), env!("FLOTILLA_BUILD_ID"));
    // Embedders may repeat initialization, but empty or conflicting identities
    // must not replace the process identity.
    flotilla_core::build_info::initialize_build_id(env!("FLOTILLA_BUILD_ID"));
    assert!(std::panic::catch_unwind(|| flotilla_core::build_info::initialize_build_id("")).is_err());
    assert!(std::panic::catch_unwind(|| flotilla_core::build_info::initialize_build_id("conflicting-build")).is_err());
    assert_eq!(flotilla_core::build_info::build_id(), env!("FLOTILLA_BUILD_ID"));
}
