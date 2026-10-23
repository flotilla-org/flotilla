//! The pushed executable parses help and refuses host-only surfaces without a daemon.
use std::process::Command;

#[test]
fn help_hooks_and_host_surface_refusal_are_standalone() {
    let binary = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join(format!("flotilla-vessel{}", std::env::consts::EXE_SUFFIX));
    for args in [vec!["--help"], vec!["hook", "--help"], vec!["environment", "shell", "--help"]] {
        let result = Command::new(&binary).args(args).output().unwrap();
        assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
    }
    let result = Command::new(&binary).output().unwrap();
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("host CLI"));
    let result = Command::new(&binary).args(["vessel-agent"]).output().unwrap();
    assert!(!result.status.success(), "drydock owns the sole agent");
}
