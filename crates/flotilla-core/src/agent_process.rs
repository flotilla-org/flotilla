//! Process-exit receipts for agents hosted inside a persistent terminal shell.

use sha2::{Digest, Sha256};

/// Each launch gets a distinct receipt, so an earlier process cannot mark a
/// replacement process as exited. Hashing also keeps the path shell-safe.
pub fn exit_receipt(crew_id: &str) -> String {
    format!(".flotilla/agent-exits/{:x}", Sha256::digest(crew_id.as_bytes()))
}

/// The parent shell records exit even when the agent cannot run its own hooks.
/// The terminal and checkout remain available for explicit or automatic resume.
pub fn monitored_command(command: &str, crew_id: &str) -> String {
    let receipt = exit_receipt(crew_id);
    format!("(\n{command}\n)\nflotilla_agent_exit_code=$?\nmkdir -p .flotilla/agent-exits\nprintf '%s\\n' \"$flotilla_agent_exit_code\" > {receipt}")
}

#[cfg(test)]
mod tests {
    use super::*;

    // The real shell is the process boundary: exit receipts must work for both
    // successful and failed child processes, without an agent-provided hook.
    #[test]
    fn parent_records_agent_exit_and_keeps_launches_independent() {
        let cwd = tempfile::tempdir().expect("checkout");
        for code in [0, 1, 42, 137] {
            let crew = format!("crew-{code}");
            let result = std::process::Command::new("sh")
                .arg("-c")
                .arg(monitored_command(&format!("exit {code}"), &crew))
                .current_dir(cwd.path())
                .status()
                .expect("shell");
            assert!(result.success(), "the parent remains usable after child failure");
            assert_eq!(std::fs::read_to_string(cwd.path().join(exit_receipt(&crew))).expect("receipt").trim(), code.to_string());
            assert!(!cwd.path().join(exit_receipt("next launch")).exists(), "an old launch cannot mark its replacement exited");
        }
    }
}
