use std::process::Command;

#[test]
fn installed_package_exposes_flotillad_binary() {
    let flotillad = env!("CARGO_BIN_EXE_flotillad");
    let status = Command::new(flotillad).arg("--help").status().expect("flotillad help should run");

    assert!(status.success(), "flotillad --help should succeed");
}
