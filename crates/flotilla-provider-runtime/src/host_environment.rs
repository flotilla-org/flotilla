//! The tools a host flotilla reaches without an agent of its own declares.
//!
//! No login profile is read there. The host's tools are declared as supplies
//! (directories and environment variables, never scripts). The preparer in
//! [`crate::execution_context`] places them in every prepared environment,
//! after flotilla's generation layers and before the fixed system tail,
//! [`SYSTEM_PATH_TAIL`]. A leading `~` in a supply path or variable means the
//! target user's home.

use std::collections::BTreeSet;

use flotilla_protocol::arg::{shell_quote, validate_env_key};

use crate::execution_context::AccountVariables;

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

    /// Supplies describe installations only: a variable a registered family
    /// keeps its account state in is a configuration error for the host.
    pub fn check_account_state(&self, accounts: &AccountVariables) -> Result<(), String> {
        for supply in &self.supplies {
            if let Some((name, family)) = supply.env.iter().find_map(|(name, _)| accounts.owner(name).map(|family| (name, family))) {
                return Err(format!("supply `{}` sets {name}, which carries `{family}` account state", supply.name));
            }
        }
        Ok(())
    }

    /// Shell assignments that establish the composed environment for one
    /// command run on the host, expanding `~` there against its `$HOME`. The
    /// controller-side expansions use the `HOME` discovered by running `env`
    /// through this same prelude over SSH, so both name the same login home.
    /// Variable names are written unquoted: [`Self::new`] admits only valid
    /// shell identifiers, and it is the only constructor with supplies.
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

/// One shell word for `value`, expanding a leading `~` against the remote `$HOME`.
fn remote_word(value: &str) -> String {
    match value.strip_prefix('~') {
        Some("") => "\"$HOME\"".to_string(),
        Some(rest) if rest.starts_with('/') => format!("\"$HOME\"{}", shell_quote(rest)),
        _ => shell_quote(value),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
