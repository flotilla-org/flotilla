use std::path::{Path, PathBuf};

pub const UNIT_NAME: &str = "flotillad.service";
const MANAGED_MARKER: &str = "# managed by fleet-install";

#[cfg(target_os = "linux")]
fn default_user_unit() -> Result<PathBuf, String> {
    let config_home = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(path) => PathBuf::from(path),
        None => {
            let home = std::env::var_os("HOME").ok_or_else(|| "HOME is not set; cannot locate the flotillad systemd unit".to_string())?;
            PathBuf::from(home).join(".config")
        }
    };
    Ok(config_home.join("systemd/user").join(UNIT_NAME))
}

#[cfg(any(test, target_os = "linux"))]
fn systemd_path(path: &Path) -> Result<String, String> {
    let home = std::env::var_os("HOME").ok_or_else(|| "HOME is not set; cannot validate the flotillad systemd unit".to_string())?;
    let home = PathBuf::from(home);
    let rendered = if path == home {
        "%h".to_string()
    } else if let Ok(relative) = path.strip_prefix(&home) {
        format!("%h/{}", relative.display())
    } else {
        path.display().to_string()
    };
    Ok(rendered.replace('\\', "\\\\").replace('"', "\\\"").replace('%', "%%").replacen("%%h", "%h", 1))
}

#[cfg(any(test, target_os = "linux"))]
fn unit_has_daemon_identity(contents: &str, socket_path: &Path, config_dir: &Path, state_dir: &Path) -> Result<bool, String> {
    if !contents.lines().any(|line| line.trim() == MANAGED_MARKER) {
        return Ok(false);
    }
    let identity = format!(
        " --config-dir=\"{}\" --state-dir=\"{}\" --socket=\"{}\"",
        systemd_path(config_dir)?,
        systemd_path(state_dir)?,
        systemd_path(socket_path)?
    );
    Ok(contents.lines().any(|line| line.starts_with("ExecStart=") && line.ends_with(&identity)))
}

#[cfg(target_os = "linux")]
fn systemctl(args: &[&str]) -> Result<std::process::Output, String> {
    std::process::Command::new("systemctl")
        .arg("--user")
        .args(args)
        .output()
        .map_err(|error| format!("could not run systemctl --user {}: {error}", args.join(" ")))
}

/// Whether the enabled, fleet-installed systemd user unit owns startup for
/// this exact daemon identity.
pub fn unit_manages_daemon(socket_path: &Path, config_dir: &Path, state_dir: &Path) -> Result<bool, String> {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (socket_path, config_dir, state_dir);
        Ok(false)
    }

    #[cfg(target_os = "linux")]
    {
        let unit = default_user_unit()?;
        let contents = match std::fs::read_to_string(&unit) {
            Ok(contents) => contents,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(format!("could not read {}: {error}", unit.display())),
        };
        if !unit_has_daemon_identity(&contents, socket_path, config_dir, state_dir)? {
            return Ok(false);
        }
        let output = systemctl(&["is-enabled", UNIT_NAME])?;
        let state = String::from_utf8_lossy(&output.stdout);
        match state.trim() {
            "enabled" => Ok(true),
            "disabled" => Ok(false),
            other => Err(format!(
                "cannot determine whether the installed {UNIT_NAME} systemd user unit is enabled (state {other:?}): {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )),
        }
    }
}

fn run_unit_action(action: &str) -> Result<(), String> {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = action;
        Err("the flotillad systemd user unit is only available on Linux".to_string())
    }

    #[cfg(target_os = "linux")]
    {
        if !default_user_unit()?.is_file() {
            return Err(format!("the {UNIT_NAME} systemd user unit is not installed"));
        }
        let output = systemctl(&[action, UNIT_NAME])?;
        if output.status.success() {
            Ok(())
        } else {
            Err(format!("could not {action} {UNIT_NAME}: {}", String::from_utf8_lossy(&output.stderr).trim()))
        }
    }
}

pub fn start_unit() -> Result<(), String> {
    run_unit_action("start")
}

pub fn stop_unit() -> Result<(), String> {
    run_unit_action("stop")
}

pub fn set_unit_enabled(enabled: bool) -> Result<(), String> {
    run_unit_action(if enabled { "enable" } else { "disable" })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fleet_marker_and_exact_identity_are_required() {
        let home = PathBuf::from(std::env::var_os("HOME").expect("HOME should be set for tests"));
        let config = home.join(".config/flotilla");
        let state = home.join(".local/state/flotilla");
        let socket = config.join("run/flotilla.sock");
        let unit = format!(
            "{MANAGED_MARKER}\n[Service]\nExecStart=\"%h/.local/opt/flotilla-fleet/current/bin/flotillad\" --config-dir=\"%h/.config/flotilla\" --state-dir=\"%h/.local/state/flotilla\" --socket=\"%h/.config/flotilla/run/flotilla.sock\"\n"
        );

        assert!(unit_has_daemon_identity(&unit, &socket, &config, &state).expect("valid unit"));
        assert!(!unit_has_daemon_identity(&unit.replace(MANAGED_MARKER, "# local unit"), &socket, &config, &state).expect("local unit"));
        assert!(!unit_has_daemon_identity(&unit, &socket, &config, &state.join("other")).expect("different identity"));
    }
}
