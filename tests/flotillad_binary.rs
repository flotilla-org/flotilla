use std::process::Command;

#[test]
fn installed_package_exposes_flotillad_binary() {
    let flotillad = env!("CARGO_BIN_EXE_flotillad");
    let status = Command::new(flotillad).arg("--help").status().expect("flotillad help should run");

    assert!(status.success(), "flotillad --help should succeed");
}

#[test]
fn binaries_report_their_wire_generation_and_protocol_version() {
    for binary in [env!("CARGO_BIN_EXE_flotilla"), env!("CARGO_BIN_EXE_flotillad")] {
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
    flotilla_core::build_info::initialize_build_id(env!("FLOTILLA_BUILD_ID"));
    assert_eq!(flotilla_core::build_info::build_id(), env!("FLOTILLA_BUILD_ID"));
    assert_eq!(flotilla_client::build_id(), env!("FLOTILLA_BUILD_ID"));
}
