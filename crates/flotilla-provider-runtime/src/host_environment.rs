//! #3722: the environment flotilla composes for commands on a host it reaches
//! without an agent of its own.
//!
//! No login profile is read there. The host's tools are declared as supplies
//! (directories and environment variables, never scripts), and flotilla builds
//! `PATH` itself, in this order:
//!
//! 1. flotilla's own directories (the agentless CLI and credential wrappers);
//! 2. tools flotilla installs from its generation (#3655);
//! 3. declared supplies, in declaration order;
//! 4. the fixed system tail, [`SYSTEM_PATH_TAIL`].
//!
//! A leading `~` in a supply path or variable means the remote user's home.

use std::collections::BTreeSet;

use flotilla_protocol::arg::{shell_quote, validate_env_key};

/// The system directories every composed `PATH` ends with.
pub const SYSTEM_PATH_TAIL: &[&str] = &["/usr/local/bin", "/usr/bin", "/bin"];

/// Variable-name prefix flotilla reserves for itself on every host.
const FLOTILLA_VARIABLE_PREFIX: &str = "FLOTILLA_";

/// One declared host supply: directories for `PATH` and variables to set.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HostSupply {
    pub name: String,
    pub path: Vec<String>,
    pub env: Vec<(String, String)>,
}

/// A host's declared supplies, validated.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HostEnvironment {
    supplies: Vec<HostSupply>,
}

impl HostEnvironment {
    /// Validate declared supplies. Every refusal is a configuration error for
    /// the host that declares them.
    pub fn new(supplies: Vec<HostSupply>) -> Result<Self, String> {
        let mut names = BTreeSet::new();
        let mut variables = BTreeSet::new();
        for supply in &supplies {
            if supply.name.trim().is_empty() {
                return Err("a supply has an empty name".into());
            }
            if !names.insert(supply.name.as_str()) {
                return Err(format!("supply `{}` is declared more than once", supply.name));
            }
            for entry in &supply.path {
                validate_path_entry(entry).map_err(|reason| format!("supply `{}` path entry {entry:?} {reason}", supply.name))?;
            }
            for (name, _) in &supply.env {
                validate_env_key(name).map_err(|error| format!("supply `{}`: {error}", supply.name))?;
                // `~` expands against `HOME`, so a supply cannot redefine it.
                if matches!(name.as_str(), "PATH" | "HOME") || name.starts_with(FLOTILLA_VARIABLE_PREFIX) {
                    return Err(supply_collision(&supply.name, name));
                }
                if !variables.insert(name.as_str()) {
                    return Err(format!("supply `{}` sets {name}, which another supply already sets", supply.name));
                }
            }
        }
        Ok(Self { supplies })
    }

    pub fn supplies(&self) -> &[HostSupply] {
        &self.supplies
    }

    /// Supply directories in declaration order, then the system tail, with
    /// `~` expanded to `home`.
    pub fn base_path_entries(&self, home: &str) -> Vec<String> {
        let supplied = self.supplies.iter().flat_map(|supply| supply.path.iter().map(|entry| expand_home(entry, home)));
        dedup(supplied.chain(SYSTEM_PATH_TAIL.iter().map(|entry| entry.to_string())))
    }

    /// Supply variables, with `~` expanded to `home`.
    pub fn variables(&self, home: &str) -> Vec<(String, String)> {
        self.supplies.iter().flat_map(|supply| supply.env.iter().map(|(name, value)| (name.clone(), expand_home(value, home)))).collect()
    }

    /// The session `PATH`: `own` first (flotilla's directories, then its
    /// generation installs), then any other directory `inherited` names that
    /// is not a supply or system directory (the credential wrappers flotilla
    /// put there), then supplies and the system tail. Directories a login
    /// profile would have added never get here: nothing reads one.
    pub fn compose_path(&self, home: &str, own: &[String], inherited: Option<&str>) -> String {
        let base = self.base_path_entries(home);
        let inherited = inherited
            .into_iter()
            .flat_map(|path| path.split(':'))
            .filter(|entry| !entry.is_empty() && !base.iter().any(|known| known == entry))
            .map(str::to_string);
        dedup(own.iter().cloned().chain(inherited).chain(base.iter().cloned())).join(":")
    }

    /// Compose a session environment in place: `PATH` as [`Self::compose_path`]
    /// builds it, and each supply variable added. A supply cannot override a
    /// variable flotilla already set for the session.
    pub fn compose_session(&self, home: &str, own: &[String], env: &mut Vec<(String, String)>) -> Result<(), String> {
        for supply in &self.supplies {
            if let Some((name, _)) = supply.env.iter().find(|(name, _)| env.iter().any(|(key, _)| key == name)) {
                return Err(supply_collision(&supply.name, name));
            }
        }
        let inherited = env.iter().rev().find(|(key, _)| key == "PATH").map(|(_, value)| value.clone());
        let path = self.compose_path(home, own, inherited.as_deref());
        env.retain(|(key, _)| key != "PATH");
        env.push(("PATH".to_string(), path));
        env.extend(self.variables(home));
        Ok(())
    }

    /// Shell assignments that establish the composed environment for one
    /// command run on the host, expanding `~` there against its `$HOME`.
    pub fn shell_prelude(&self) -> String {
        let path = self
            .supplies
            .iter()
            .flat_map(|supply| supply.path.iter().map(String::as_str))
            .chain(SYSTEM_PATH_TAIL.iter().copied())
            .map(remote_word)
            .collect::<Vec<_>>()
            .join(":");
        let mut prelude = format!("PATH={path}; export PATH; ");
        for supply in &self.supplies {
            for (name, value) in &supply.env {
                prelude.push_str(&format!("{name}={}; export {name}; ", remote_word(value)));
            }
        }
        prelude
    }
}

/// Shell lines that set `variables` again, for a command whose shell may have
/// read a login profile after its environment was handed over (cleat runs a
/// session command through `$SHELL -lc`). They are POSIX `export` lines, as the
/// session command they prefix already is (`flotilla_agent::process`); a csh
/// login shell cannot run either.
pub fn export_lines(variables: &[(String, String)]) -> String {
    variables.iter().map(|(name, value)| format!("export {name}={}\n", shell_quote(value))).collect()
}

fn supply_collision(supply: &str, variable: &str) -> String {
    format!("supply `{supply}` sets {variable}, which flotilla sets")
}

fn validate_path_entry(entry: &str) -> Result<(), &'static str> {
    if entry.contains(':') {
        return Err("contains `:`");
    }
    if entry == "~" || entry.starts_with("~/") || entry.starts_with('/') {
        Ok(())
    } else {
        Err("is neither absolute nor under `~/`")
    }
}

fn expand_home(value: &str, home: &str) -> String {
    match value.strip_prefix('~') {
        Some("") => home.to_string(),
        Some(rest) if rest.starts_with('/') => format!("{}{rest}", home.trim_end_matches('/')),
        _ => value.to_string(),
    }
}

/// One shell word for `value`, expanding a leading `~` against the remote `$HOME`.
fn remote_word(value: &str) -> String {
    match value.strip_prefix('~') {
        Some("") => "\"$HOME\"".to_string(),
        Some(rest) if rest.starts_with('/') => format!("\"$HOME\"{}", shell_quote(rest)),
        _ => shell_quote(value),
    }
}

fn dedup(entries: impl Iterator<Item = String>) -> Vec<String> {
    let mut seen = BTreeSet::new();
    entries.filter(|entry| seen.insert(entry.clone())).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rust_and_gh() -> HostEnvironment {
        HostEnvironment::new(vec![
            HostSupply {
                name: "rust".into(),
                path: vec!["~/.cargo/bin".into()],
                env: vec![("CARGO_HOME".into(), "~/.cargo".into()), ("RUSTUP_HOME".into(), "~/.rustup".into())],
            },
            HostSupply { name: "gh".into(), path: vec!["~/.local/opt/gh/bin".into()], env: Vec::new() },
        ])
        .expect("valid supplies")
    }

    #[test]
    fn session_path_is_own_then_credentials_then_supplies_then_system() {
        let host = rust_and_gh();
        let inherited = "/run/creds/github:/home/u/.cargo/bin:/home/u/.local/opt/gh/bin:/usr/local/bin:/usr/bin:/bin";
        let path = host.compose_path("/home/u", &["/state/cli".into()], Some(inherited));
        assert_eq!(path, "/state/cli:/run/creds/github:/home/u/.cargo/bin:/home/u/.local/opt/gh/bin:/usr/local/bin:/usr/bin:/bin");
        // Composing again changes nothing.
        assert_eq!(host.compose_path("/home/u", &["/state/cli".into()], Some(&path)), path);
    }

    #[test]
    fn session_environment_sets_supply_variables_and_refuses_to_override_flotilla() {
        let host = rust_and_gh();
        let mut env = vec![("GITHUB_TOKEN_FILE".into(), "/run/token".into())];
        host.compose_session("/home/u/", &[], &mut env).expect("compose");
        assert!(env.contains(&("RUSTUP_HOME".into(), "/home/u/.rustup".into())), "{env:?}");
        assert!(env.contains(&("PATH".into(), "/home/u/.cargo/bin:/home/u/.local/opt/gh/bin:/usr/local/bin:/usr/bin:/bin".into())));

        let mut env = vec![("CARGO_HOME".into(), "/flotilla/cargo".into())];
        let error = host.compose_session("/home/u", &[], &mut env).expect_err("collision");
        assert_eq!(error, "supply `rust` sets CARGO_HOME, which flotilla sets");
        assert_eq!(env, vec![("CARGO_HOME".to_string(), "/flotilla/cargo".to_string())], "a refused composition changes nothing");
    }

    #[test]
    fn declarations_flotilla_owns_or_cannot_place_are_refused() {
        let refused = |supply: HostSupply| HostEnvironment::new(vec![supply]).expect_err("refused");
        let supply = |path: &[&str], env: &[(&str, &str)]| HostSupply {
            name: "tool".into(),
            path: path.iter().map(|entry| entry.to_string()).collect(),
            env: env.iter().map(|(name, value)| (name.to_string(), value.to_string())).collect(),
        };
        assert_eq!(refused(supply(&[], &[("PATH", "/x")])), "supply `tool` sets PATH, which flotilla sets");
        assert_eq!(refused(supply(&[], &[("HOME", "/x")])), "supply `tool` sets HOME, which flotilla sets");
        assert_eq!(
            refused(supply(&[], &[("FLOTILLA_DAEMON_SOCKET", "/x")])),
            "supply `tool` sets FLOTILLA_DAEMON_SOCKET, which flotilla sets"
        );
        assert!(refused(supply(&["bin"], &[])).contains("neither absolute"));
        assert!(refused(supply(&["~other/bin"], &[])).contains("neither absolute"));
        assert!(refused(supply(&["/a:/b"], &[])).contains("`:`"));
        assert!(refused(supply(&[], &[("NOT-A-NAME", "x")])).contains("invalid environment assignment key"));
        assert!(refused(HostSupply { name: " ".into(), ..HostSupply::default() }).contains("empty name"));
        let twice =
            HostEnvironment::new(vec![supply(&[], &[("A", "1")]), HostSupply { name: "other".into(), ..supply(&[], &[("A", "2")]) }]);
        assert_eq!(twice.expect_err("shared variable"), "supply `other` sets A, which another supply already sets");
        assert!(HostEnvironment::new(vec![supply(&[], &[]), supply(&[], &[])]).expect_err("same name").contains("more than once"));
    }

    // The prelude runs in a real shell: supplies expand against that shell's
    // `$HOME`, values with shell syntax stay literal, and no profile is read.
    #[cfg(unix)]
    #[test]
    fn shell_prelude_composes_the_environment_in_a_real_shell() {
        let host = HostEnvironment::new(vec![HostSupply {
            name: "odd".into(),
            path: vec!["~/it's $(here)".into(), "/opt/tool/bin".into()],
            env: vec![("TOOL_HOME".into(), "~".into()), ("TOOL_FLAGS".into(), "$HOME `x` '".into())],
        }])
        .expect("valid supplies");
        let script = format!("{}printf '%s\\n' \"$PATH\" \"$TOOL_HOME\" \"$TOOL_FLAGS\"", host.shell_prelude());
        let output = std::process::Command::new("/bin/sh").args(["-c", &script]).env_clear().env("HOME", "/home/x y").output().expect("sh");
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        assert_eq!(
            String::from_utf8(output.stdout).expect("utf-8"),
            "/home/x y/it's $(here):/opt/tool/bin:/usr/local/bin:/usr/bin:/bin\n/home/x y\n$HOME `x` '\n"
        );
    }
}
