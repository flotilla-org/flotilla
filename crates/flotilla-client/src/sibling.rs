//! Sibling binaries (#4249): a heavy client surface ships as `flotilla-<name>` beside `flotilla`,
//! which hands the command to it. Only the siblings named here run, and only from the running
//! binary's own directory, never from `PATH`.

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    process::Command,
};

/// The variable a handoff sets to the parent's build id; the sibling refuses a different build.
pub const PARENT_BUILD_ENV: &str = "FLOTILLA_PARENT_BUILD";

/// The siblings `flotilla` hands commands to; adding one is a code change. `mcp` (#4201) and
/// `tui` (#4263) join as they land. This is the allowlist the release ships too: the candidate
/// build reads the `Sibling::<Name> => "<command>"` arms of [`Sibling::command`] from this file
/// (`generation_validation.py siblings`), so keep each arm on one line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sibling {
    Table,
}

impl Sibling {
    /// Every sibling, in allowlist order.
    pub const ALL: &[Sibling] = &[Sibling::Table];

    /// The `flotilla` command it serves.
    pub fn command(self) -> &'static str {
        match self {
            Sibling::Table => "table",
        }
    }

    /// Its file name: `flotilla-<command>`, with `.exe` on Windows.
    pub fn file_name(self) -> String {
        format!("flotilla-{}{}", self.command(), std::env::consts::EXE_SUFFIX)
    }
}

/// The running binary, with symlinks resolved, so a sibling is found where the release put it.
pub fn running_binary() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|error| format!("cannot find the running flotilla binary: {error}"))?;
    std::fs::canonicalize(&exe).map_err(|error| format!("cannot resolve the running flotilla binary {}: {error}", exe.display()))
}

/// Where `sibling` is: beside `binary`, or an error naming it, the directory and the fix.
pub fn locate(sibling: Sibling, binary: &Path) -> Result<PathBuf, String> {
    let dir = binary.parent().ok_or_else(|| format!("{} has no directory", binary.display()))?;
    let path = dir.join(sibling.file_name());
    if path.is_file() {
        return Ok(path);
    }
    Err(format!(
        "`flotilla {}` runs {}, which is not in {} beside {}; reinstall the flotilla release, which ships its siblings \
         (in a source checkout: cargo build -p flotilla-{})",
        sibling.command(),
        sibling.file_name(),
        dir.display(),
        binary.display(),
        sibling.command(),
    ))
}

/// Runs `sibling` with `args` in place of this process, telling it this build (`build`). On Unix
/// this execs it, so the terminal, signals and exit status pass straight through, and returns
/// only an error. Elsewhere it spawns and waits for it, returning the status to exit with.
pub fn hand_off(sibling: Sibling, args: Vec<OsString>, build: &str) -> Result<i32, String> {
    let path = locate(sibling, &running_binary()?)?;
    let mut command = Command::new(&path);
    command.args(args).env(PARENT_BUILD_ENV, build);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let error = command.exec();
        Err(format!("cannot run {}: {error}", path.display()))
    }
    #[cfg(not(unix))]
    {
        let status = command.status().map_err(|error| format!("cannot run {}: {error}", path.display()))?;
        Ok(status.code().unwrap_or(1))
    }
}

/// What a sibling run directly or by `flotilla` says about `parent`, the `FLOTILLA_PARENT_BUILD`
/// it was given, against its own `build`: nothing when they match or it ran on its own.
pub fn check_parent_build(name: &str, parent: Option<&str>, build: &str) -> Result<(), String> {
    match parent {
        Some(parent) if parent != build => {
            Err(format!("flotilla is build {parent} but {name} beside it is build {build}; reinstall the flotilla release so they match"))
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sibling_is_found_beside_the_binary_and_a_missing_one_names_the_fix() {
        let dir = tempfile::tempdir().expect("tempdir");
        let binary = dir.path().join("flotilla");
        let missing = locate(Sibling::Table, &binary).unwrap_err();
        assert!(missing.contains(&Sibling::Table.file_name()), "{missing}");
        assert!(missing.contains(&dir.path().display().to_string()), "{missing}");
        assert!(missing.contains("reinstall the flotilla release"), "{missing}");

        let sibling = dir.path().join(Sibling::Table.file_name());
        std::fs::write(&sibling, b"").expect("write sibling");
        assert_eq!(locate(Sibling::Table, &binary).expect("found"), sibling);
    }

    // The release packages the siblings named by the `command` arms in this file's text; every
    // sibling has its arm there, and nothing else in the file reads as one.
    #[test]
    fn the_packaged_allowlist_reads_every_sibling_from_this_file() {
        let arms: Vec<&str> = include_str!("sibling.rs")
            .lines()
            .filter_map(|line| line.trim().strip_prefix("Sibling::")?.split_once(" => \"")?.1.strip_suffix("\","))
            .collect();
        let commands: Vec<&str> = Sibling::ALL.iter().map(|sibling| sibling.command()).collect();
        // `command` matches exhaustively, so a variant missing from `ALL` still has an arm here.
        assert_eq!(arms, commands);
    }

    #[test]
    fn a_sibling_refuses_a_parent_of_another_build() {
        assert!(check_parent_build("flotilla-table", None, "b1").is_ok(), "run on its own");
        assert!(check_parent_build("flotilla-table", Some("b1"), "b1").is_ok());
        let refused = check_parent_build("flotilla-table", Some("b0"), "b1").unwrap_err();
        assert!(refused.contains("b0") && refused.contains("b1"), "{refused}");
    }
}
