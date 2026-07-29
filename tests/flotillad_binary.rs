use std::process::Command;

#[test]
fn installed_package_exposes_flotillad_binary() {
    let flotillad = env!("CARGO_BIN_EXE_flotillad");
    let status = Command::new(flotillad).arg("--help").status().expect("flotillad help should run");

    assert!(status.success(), "flotillad --help should succeed");
}

#[test]
fn binaries_report_their_wire_generation() {
    for binary in [env!("CARGO_BIN_EXE_flotilla"), env!("CARGO_BIN_EXE_flotillad")] {
        let output = Command::new(binary).arg("--version").output().expect("binary version should run");

        assert!(output.status.success(), "{} --version should succeed", binary);
        let stdout = String::from_utf8(output.stdout).expect("version output should be UTF-8");
        assert!(
            stdout.contains(&format!("wire={}", flotilla_client::BUILD_ID)),
            "{} should report its wire generation, got {stdout:?}",
            binary
        );
    }
}
