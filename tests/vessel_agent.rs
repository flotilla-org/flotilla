//! Real process coverage for the resident skeleton's startup contract.

#[cfg(unix)]
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;

fn command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_flotilla"));
    command
        .kill_on_drop(true)
        .env_remove("FLOTILLA_DAEMON")
        .env_remove("FLOTILLA_ENVIRONMENT_ID")
        .env_remove("FLOTILLA_DAEMON_SOCKET")
        .env_remove("RUST_LOG");
    command
}

// ADR 0069: help works before configuration; invalid startup inputs fail promptly.
// Finite cases cover absent identity and malformed logging, through the real entry point.
#[tokio::test]
async fn startup_help_and_errors_do_not_require_a_daemon() {
    let help = tokio::time::timeout(Duration::from_secs(10), command().args(["vessel-agent", "--help"]).output())
        .await
        .expect("help deadline")
        .expect("help process");
    assert!(help.status.success());
    assert!(String::from_utf8_lossy(&help.stdout).contains("FLOTILLA_ENVIRONMENT_ID"));
    for (configured, expected) in [(false, "FLOTILLA_ENVIRONMENT_ID is required"), (true, "invalid RUST_LOG")] {
        let mut child = command();
        child.arg("vessel-agent");
        if configured {
            let socket = if cfg!(windows) { "C:/unused.sock" } else { "/unused.sock" };
            child.env("FLOTILLA_ENVIRONMENT_ID", "env-test").env("FLOTILLA_DAEMON_SOCKET", socket).env("RUST_LOG", "[invalid");
        }
        let output =
            tokio::time::timeout(Duration::from_secs(10), child.output()).await.expect("startup deadline").expect("startup process");
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains(expected), "{output:?}");
        assert!(output.stdout.is_empty());
    }
}

// The skeleton must log to stderr and exit cleanly for either supervisor shutdown signal.
// Real children isolate global logging and signal handlers; no socket or daemon is needed.
#[cfg(unix)]
#[tokio::test]
async fn startup_logs_and_shutdown_signals_work_in_the_resident_process() {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};

    for (signal, filter) in [(libc::SIGINT, None), (libc::SIGTERM, Some("info"))] {
        let mut launch = command();
        launch.arg("vessel-agent").env("FLOTILLA_ENVIRONMENT_ID", "env-test").env("FLOTILLA_DAEMON_SOCKET", "/unused.sock");
        if let Some(filter) = filter {
            launch.env("RUST_LOG", filter);
        }
        let mut child = launch.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().expect("resident agent");
        let mut stderr = BufReader::new(child.stderr.take().expect("stderr pipe"));
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(10), stderr.read_line(&mut line))
            .await
            .expect("startup log deadline")
            .expect("startup log");
        assert!(line.contains("vessel agent started") && line.contains("env-test"), "{line}");
        // SAFETY: this is our live child process, and the signal is one of the two constants above.
        assert_eq!(unsafe { libc::kill(child.id().expect("live child") as i32, signal) }, 0);
        let output =
            tokio::time::timeout(Duration::from_secs(10), child.wait_with_output()).await.expect("shutdown deadline").expect("shutdown");
        assert!(output.status.success(), "{output:?}");
        assert!(output.stdout.is_empty());
        let mut rest = String::new();
        tokio::time::timeout(Duration::from_secs(10), stderr.read_to_string(&mut rest)).await.expect("stderr deadline").expect("stderr");
        assert!(rest.contains("vessel agent stopped"), "{rest}");
    }
}

// A vessel build must refuse the dashboard with an actionable message and normal CLI failure status.
#[cfg(not(feature = "tui"))]
#[tokio::test]
async fn vessel_build_dashboard_refusal_explains_the_required_feature() {
    let output = tokio::time::timeout(Duration::from_secs(10), command().output()).await.expect("refusal deadline").expect("CLI");
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("operator build with the tui feature"), "{stderr}");
}
