use std::path::Path;
#[cfg(target_os = "macos")]
use std::path::PathBuf;

pub const AGENT_LABEL: &str = "work.flotilla.flotillad";

#[cfg(target_os = "macos")]
fn default_agent_plist() -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME").ok_or_else(|| "HOME is not set; cannot locate the flotillad launchd agent".to_string())?;
    Ok(PathBuf::from(home).join("Library/LaunchAgents/work.flotilla.flotillad.plist"))
}

#[cfg(target_os = "macos")]
fn is_default_daemon_identity(socket_path: &Path, config_dir: &Path, state_dir: &Path) -> bool {
    let policy = flotilla_core::path_policy::PathPolicy::from_process_env();
    let default_socket = flotilla_core::path_policy::daemon_socket_path(policy.config_dir.as_path());
    socket_path == default_socket && config_dir == policy.config_dir.as_path() && state_dir == policy.state_dir.as_path()
}

#[cfg(any(test, target_os = "macos"))]
fn service_is_disabled(output: &str) -> bool {
    output.lines().any(|line| {
        let Some((service, value)) = line.split_once("=>") else { return false };
        service.trim().trim_matches('"') == AGENT_LABEL && value.trim().trim_end_matches(';') == "true"
    })
}

#[cfg(target_os = "macos")]
fn launchctl(args: &[&str]) -> Result<std::process::Output, String> {
    std::process::Command::new("/bin/launchctl")
        .args(args)
        .output()
        .map_err(|error| format!("could not run launchctl {}: {error}", args.join(" ")))
}

#[cfg(target_os = "macos")]
fn service_target() -> String {
    // SAFETY: getuid has no preconditions and does not mutate process state.
    format!("gui/{}/{}", unsafe { libc::getuid() }, AGENT_LABEL)
}

#[cfg(target_os = "macos")]
fn service_domain() -> String {
    // SAFETY: getuid has no preconditions and does not mutate process state.
    format!("gui/{}", unsafe { libc::getuid() })
}

/// Whether the fleet launchd agent owns startup for this daemon identity.
///
/// An explicitly disabled installed agent is the durable dev-mode marker and
/// returns client spawn authority. Any failure to inspect an installed agent
/// is reported instead of risking a competing direct daemon process.
pub fn agent_manages_daemon(socket_path: &Path, config_dir: &Path, state_dir: &Path) -> Result<bool, String> {
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (socket_path, config_dir, state_dir);
        Ok(false)
    }

    #[cfg(target_os = "macos")]
    {
        if !is_default_daemon_identity(socket_path, config_dir, state_dir) || !default_agent_plist()?.is_file() {
            return Ok(false);
        }
        let domain = service_domain();
        let output = launchctl(&["print-disabled", &domain])?;
        if !output.status.success() {
            return Err(format!(
                "cannot determine whether the installed {AGENT_LABEL} launchd agent is disabled: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        Ok(!service_is_disabled(&String::from_utf8_lossy(&output.stdout)))
    }
}

/// Load the installed agent into this user's launchd domain. Dev mode unloads
/// the job, so re-enabling it requires bootstrap before kickstart.
pub fn bootstrap_agent() -> Result<(), String> {
    #[cfg(not(target_os = "macos"))]
    {
        Err("the flotillad launchd agent is only available on macOS".to_string())
    }

    #[cfg(target_os = "macos")]
    {
        let plist = default_agent_plist()?;
        if !plist.is_file() {
            return Err(format!("the {AGENT_LABEL} launchd agent is not installed"));
        }
        let domain = service_domain();
        let plist = plist.to_str().ok_or_else(|| format!("the {AGENT_LABEL} launchd agent path is not valid UTF-8"))?;
        let output = launchctl(&["bootstrap", &domain, plist])?;
        if output.status.success() {
            Ok(())
        } else {
            Err(format!("could not bootstrap {AGENT_LABEL}: {}", String::from_utf8_lossy(&output.stderr).trim()))
        }
    }
}

/// Ask launchd to start the fleet daemon if it is not already running.
/// Deliberately omits `-k`: reconnecting clients must never restart a daemon
/// which launchd has already brought up.
pub fn kickstart_agent() -> Result<(), String> {
    #[cfg(not(target_os = "macos"))]
    {
        Err("the flotillad launchd agent is only available on macOS".to_string())
    }

    #[cfg(target_os = "macos")]
    {
        let target = service_target();
        let output = launchctl(&["kickstart", &target])?;
        if output.status.success() {
            Ok(())
        } else {
            Err(format!("launchd owns flotillad but could not start {AGENT_LABEL}: {}", String::from_utf8_lossy(&output.stderr).trim()))
        }
    }
}

pub fn set_agent_enabled(enabled: bool) -> Result<(), String> {
    #[cfg(not(target_os = "macos"))]
    {
        let _ = enabled;
        Err("flotillad dev mode is only available on macOS hosts with the fleet launchd agent".to_string())
    }

    #[cfg(target_os = "macos")]
    {
        if !default_agent_plist()?.is_file() {
            return Err(format!("the {AGENT_LABEL} launchd agent is not installed"));
        }
        let action = if enabled { "enable" } else { "disable" };
        let target = service_target();
        let output = launchctl(&[action, &target])?;
        if output.status.success() {
            Ok(())
        } else {
            Err(format!("could not {action} {AGENT_LABEL}: {}", String::from_utf8_lossy(&output.stderr).trim()))
        }
    }
}

/// Unload the agent and stop its process. A missing/unloaded job is already in
/// the desired state, so launchctl's non-zero result is intentionally ignored.
pub fn bootout_agent() -> Result<(), String> {
    #[cfg(not(target_os = "macos"))]
    {
        Err("the flotillad launchd agent is only available on macOS".to_string())
    }

    #[cfg(target_os = "macos")]
    {
        let target = service_target();
        launchctl(&["bootout", &target]).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_state_is_keyed_by_exact_agent_label() {
        let output = r#"
disabled services = {
    "work.flotilla.flotillad-helper" => true
    "work.flotilla.flotillad" => true
}
"#;
        assert!(service_is_disabled(output));
        assert!(!service_is_disabled(r#""work.flotilla.flotillad" => false"#));
        assert!(!service_is_disabled("disabled services = {}"));
    }
}
