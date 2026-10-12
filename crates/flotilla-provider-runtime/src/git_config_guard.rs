use std::{
    path::{Path, PathBuf},
    process::Command,
};

#[cfg(test)]
use crate::CommandRunner;

/// Check host-owned repository configuration before a local Git process can
/// interpret it. A contained worktree can write refs in the shared gitdir, so
/// the host must treat config there as untrusted even with read-only overlays.
pub(crate) fn guard_host_git_config(cmd: &str, args: &[&str], cwd: &Path) -> Result<(), String> {
    if cmd != "git" {
        return Ok(());
    }
    // ProcessCommandRunner launches a host child without replacing its process
    // environment, so this is the same GIT_DIR that the child Git will see.
    let (directory, explicit_git_dir) = git_command_location(args, cwd, std::env::var_os("GIT_DIR").map(PathBuf::from));
    let git_entry = if let Some(path) = explicit_git_dir {
        if path.is_absolute() {
            path
        } else {
            directory.join(path)
        }
    } else if let Some(path) = directory.ancestors().find_map(discovered_git_entry) {
        path
    } else {
        return Ok(());
    };
    // Let Git report a missing explicit gitdir. Do not fall back to an ancestor
    // or turn an absent checkout into a host-config inspection failure.
    if !git_entry.exists() {
        return Ok(());
    }
    let admin_dir = if git_entry.is_dir() {
        git_entry
    } else {
        let pointer = std::fs::read_to_string(&git_entry).map_err(|error| format!("read {}: {error}", git_entry.display()))?;
        let target = pointer.trim().strip_prefix("gitdir: ").ok_or_else(|| format!("invalid gitdir pointer at {}", git_entry.display()))?;
        if Path::new(target).is_absolute() {
            PathBuf::from(target)
        } else {
            git_entry.parent().expect(".git has a parent").join(target)
        }
    };
    let common_dir = match std::fs::read_to_string(admin_dir.join("commondir")) {
        Ok(relative) => admin_dir.join(relative.trim()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => admin_dir.clone(),
        Err(error) => return Err(format!("read {}: {error}", admin_dir.join("commondir").display())),
    };
    let common_dir =
        common_dir.canonicalize().map_err(|error| format!("resolve shared Git directory {}: {error}", common_dir.display()))?;
    for config in [common_dir.join("config"), admin_dir.join("config.worktree")] {
        if !config.exists() {
            continue;
        }
        let output = Command::new("git")
            .args(["config", "--no-includes", "--file"])
            .arg(&config)
            .args(["--name-only", "--list", "--null"])
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env_remove("GIT_CONFIG_PARAMETERS")
            .env_remove("GIT_CONFIG_COUNT")
            .output()
            .map_err(|error| format!("inspect {}: {error}", config.display()))?;
        if !output.status.success() {
            return Err(format!("cannot inspect Git configuration {}: {}", config.display(), String::from_utf8_lossy(&output.stderr)));
        }
        for key in output.stdout.split(|byte| *byte == 0).filter(|key| !key.is_empty()) {
            let key = String::from_utf8_lossy(key).to_ascii_lowercase();
            // An explicit absolute default hooks path is harmless; hosts have
            // used it historically. Relative paths can point into a checkout.
            if key == "core.hookspath" && default_hooks_path(&config, &common_dir)? {
                continue;
            }
            if !allowed_shared_config_key(&key) {
                tracing::error!(config = %config.display(), %key, "unsafe shared Git configuration; refusing Git operation");
                return Err(format!(
                    "unsafe shared Git configuration {}: {key}; remove this key from the host-owned clone config before retrying",
                    config.display()
                ));
            }
        }
    }
    Ok(())
}

/// Git discovery checks each directory level in turn: a `.git` entry first,
/// then the directory itself as a bare repository, before moving up. A bare
/// repository nested inside a checkout must resolve to its own config.
fn discovered_git_entry(directory: &Path) -> Option<PathBuf> {
    let dot_git = directory.join(".git");
    if dot_git.exists() {
        return Some(dot_git);
    }
    let bare = ["HEAD", "config", "objects", "refs"].iter().all(|entry| directory.join(entry).exists());
    bare.then(|| directory.to_path_buf())
}

fn git_command_location(args: &[&str], cwd: &Path, inherited_git_dir: Option<PathBuf>) -> (PathBuf, Option<PathBuf>) {
    let mut directory = cwd.to_path_buf();
    let mut explicit_git_dir = inherited_git_dir;
    let mut index = 0;
    while index < args.len() {
        match args[index] {
            "-C" if index + 1 < args.len() => {
                let path = Path::new(args[index + 1]);
                directory = if path.is_absolute() { path.to_path_buf() } else { directory.join(path) };
                index += 2;
            }
            "--git-dir" if index + 1 < args.len() => {
                explicit_git_dir = Some(PathBuf::from(args[index + 1]));
                index += 2;
            }
            "-c" | "--config-env" | "--work-tree" | "--namespace" if index + 1 < args.len() => index += 2,
            option if option.starts_with("-C") && option.len() > 2 => {
                let path = Path::new(&option[2..]);
                directory = if path.is_absolute() { path.to_path_buf() } else { directory.join(path) };
                index += 1;
            }
            option if option.starts_with("--git-dir=") => {
                explicit_git_dir = Some(PathBuf::from(&option[10..]));
                index += 1;
            }
            option if option.starts_with('-') => index += 1,
            _ => break, // everything after the subcommand belongs to that command
        }
    }
    (directory, explicit_git_dir)
}

fn default_hooks_path(config: &Path, common_dir: &Path) -> Result<bool, String> {
    let output = Command::new("git")
        .args(["config", "--no-includes", "--file"])
        .arg(config)
        .args(["--get", "core.hooksPath"])
        .output()
        .map_err(|error| format!("inspect hooks path in {}: {error}", config.display()))?;
    if !output.status.success() {
        return Ok(false);
    }
    let value = String::from_utf8_lossy(&output.stdout);
    let path = value.trim();
    // A symlinked spelling is rejected even if it resolves to the default.
    Ok(Path::new(path) == common_dir.join("hooks"))
}

fn allowed_shared_config_key(key: &str) -> bool {
    matches!(
        key,
        "core.bare"
            | "core.repositoryformatversion"
            | "core.filemode"
            | "core.logallrefupdates"
            | "core.ignorecase"
            | "core.precomposeunicode"
            | "core.worktree"
            | "core.autocrlf"
            | "core.symlinks"
            | "core.sparsecheckout"
            | "core.sparsecheckoutcone"
            | "core.untrackedcache"
            | "gc.auto"
            | "gc.autodetach"
            | "gc.pruneexpire"
            // Non-executable local-clone tracking and portable worktree metadata (#3090).
            | "push.default"
            | "worktree.userelativepaths"
            | "extensions.relativeworktrees"
            | "flotilla.createdforconvoy"
            | "pull.rebase"
            | "pull.ff"
            | "init.defaultbranch"
            | "extensions.objectformat"
            | "user.name"
            | "user.email"
            | "worktrunk.default-branch"
            | "worktrunk.history"
    ) || key.starts_with("worktrunk.hints.")
        || ["url", "fetch", "pushurl", "push", "mirror", "prune", "tagopt"]
        .iter()
        .any(|suffix| key.starts_with("remote.") && key.ends_with(&format!(".{suffix}")))
        || ["remote", "merge", "rebase", "pushremote", "description"]
            .iter()
            .any(|suffix| key.starts_with("branch.") && key.ends_with(&format!(".{suffix}")))
        // Flotilla's own issue links: `branch.<branch>.flotilla.issues.<provider>`.
        || key.starts_with("branch.") && key.rsplit_once('.').is_some_and(|(prefix, _provider)| prefix.ends_with(".flotilla.issues"))
}

pub(crate) async fn guard_host_git_config_async(cmd: &str, args: &[&str], cwd: &Path) -> Result<(), String> {
    if cmd != "git" {
        return Ok(());
    }
    let cmd = cmd.to_string();
    let args = args.iter().map(|arg| (*arg).to_string()).collect::<Vec<_>>();
    let cwd = cwd.to_path_buf();
    crate::probe::blocking("shared Git config", crate::probe::PROBE_TIMEOUT, move || {
        let args = args.iter().map(String::as_str).collect::<Vec<_>>();
        guard_host_git_config(&cmd, &args, &cwd)
    })
    .await
}

#[cfg(test)]
mod git_config_guard_tests {
    use super::*;

    fn git(repo: &Path, args: &[&str]) {
        let output = Command::new("git").args(args).current_dir(repo).output().expect("run Git fixture command");
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    }

    #[test]
    fn clean_repo_and_explicit_default_hooks_path_are_allowed() {
        let repo = tempfile::tempdir().expect("temporary repository");
        git(repo.path(), &["init", "-q"]);
        git(repo.path(), &["config", "remote.origin.url", "https://example.com/repo.git"]);
        git(repo.path(), &["config", "branch.main.remote", "origin"]);
        git(repo.path(), &["config", "core.autocrlf", "false"]);
        guard_host_git_config("git", &["status"], repo.path()).expect("clean repository");
        // Keys flotilla and worktrunk write into the host clone as ordinary state.
        git(repo.path(), &["config", "branch.fix/a.b.flotilla.issues.github", "42"]);
        git(repo.path(), &["config", "worktrunk.history", "fix/a.b"]);
        git(repo.path(), &["config", "worktrunk.hints.worktree-path", "true"]);
        guard_host_git_config("git", &["status"], repo.path()).expect("flotilla issue links and worktrunk state");
        // Canonical spelling: macOS temp dirs sit behind the `/var` symlink, which the guard rejects.
        let hooks = repo.path().canonicalize().expect("canonical repository path").join(".git/hooks");
        git(repo.path(), &["config", "core.hooksPath", hooks.to_str().expect("UTF-8 path")]);
        guard_host_git_config("git", &["status"], repo.path()).expect("host's default hooks path");
    }

    #[test]
    fn bare_repository_config_is_checked() {
        let repo = tempfile::tempdir().expect("temporary bare repository");
        git(repo.path(), &["init", "--bare", "-q"]);
        git(repo.path(), &["config", "core.fsmonitor", "true"]);
        let error = guard_host_git_config("git", &["show-ref"], repo.path()).expect_err("bare config drift");
        assert!(error.contains("core.fsmonitor"));
    }

    #[test]
    fn bare_repository_nested_in_a_checkout_is_inspected_itself() {
        let outer = tempfile::tempdir().expect("temporary checkout");
        git(outer.path(), &["init", "-q"]);
        let bare = outer.path().join("nested.git");
        git(outer.path(), &["init", "--bare", "-q", bare.to_str().expect("UTF-8 path")]);
        git(&bare, &["config", "core.fsmonitor", "true"]);
        let error = guard_host_git_config("git", &["show-ref"], &bare).expect_err("nested bare config drift");
        assert!(error.contains("core.fsmonitor"), "{error}");
        assert!(error.contains("nested.git"), "{error}");
    }

    // A missing explicit gitdir must fail in Git without inspecting or executing
    // an ancestor's unsafe config. Normal discovery still rejects that config.
    // This runner-boundary scenario complements the generated root-shape coverage.
    #[tokio::test]
    async fn missing_explicit_git_dir_does_not_inspect_ancestor_config() {
        use crate::{ChannelLabel, ProcessCommandRunner};

        let outer = tempfile::tempdir().expect("temporary checkout");
        git(outer.path(), &["init", "-q"]);
        let marker = outer.path().join("fsmonitor-ran");
        git(outer.path(), &["config", "core.fsmonitor", &format!("touch {}", marker.display())]);
        let child = outer.path().join("not-a-checkout");
        std::fs::create_dir(&child).expect("create non-repository child");
        let child = child.to_str().expect("UTF-8 child path");
        let error = guard_host_git_config("git", &["-C", child, "status"], Path::new("/"))
            .expect_err("normal discovery must reject unsafe ancestor config");
        assert!(error.contains("core.fsmonitor"), "{error}");

        let output = ProcessCommandRunner
            .run_output("git", &["-C", child, "--git-dir=.git", "status"], Path::new("/"), &ChannelLabel::Default)
            .await
            .expect("missing explicit gitdir must be reported by Git, not the config guard");
        assert!(!output.success());
        assert!(output.stderr.contains("not a git repository"), "{}", output.stderr);
        assert!(!marker.exists(), "ancestor fsmonitor must never run");
    }

    #[test]
    fn only_leading_global_options_change_git_config_resolution() {
        let cwd = Path::new("/safe/repo");
        let (directory, git_dir) = git_command_location(&["diff", "-C", "/untrusted", "--git-dir=/untrusted/.git"], cwd, None);
        assert_eq!(directory, cwd);
        assert!(git_dir.is_none());
        let (directory, git_dir) = git_command_location(&["-C", "/other", "--git-dir=.git", "status"], cwd, None);
        assert_eq!(directory, Path::new("/other"));
        assert_eq!(git_dir, Some(PathBuf::from(".git")));
    }

    #[test]
    fn reject_other_executable_keys_and_writable_worktree_config_extension() {
        let repo = tempfile::tempdir().expect("temporary repository");
        git(repo.path(), &["init", "-q"]);
        for key in ["alias.exploit", "core.editor", "diff.foo.textconv", "merge.foo.driver", "extensions.worktreeConfig"] {
            let value = if key == "extensions.worktreeConfig" { "true" } else { "echo unsafe" };
            git(repo.path(), &["config", key, value]);
            let error = guard_host_git_config("git", &["status"], repo.path()).expect_err("unexpected key must be rejected");
            assert!(error.to_ascii_lowercase().contains(&key.to_ascii_lowercase()), "{error}");
            git(repo.path(), &["config", "--unset", key]);
        }
    }

    #[test]
    fn explicit_git_dir_and_worktree_config_are_inspected() {
        let repo = tempfile::tempdir().expect("temporary repository");
        git(repo.path(), &["init", "-q"]);
        git(repo.path(), &["config", "user.name", "Tester"]);
        git(repo.path(), &["config", "user.email", "tester@example.com"]);
        git(repo.path(), &["commit", "--allow-empty", "-qm", "initial"]);
        let worktree = repo.path().join("work");
        git(repo.path(), &["worktree", "add", "-qb", "work", worktree.to_str().expect("UTF-8 path")]);
        let admin = std::fs::read_to_string(worktree.join(".git")).expect("worktree pointer");
        let admin = PathBuf::from(admin.trim().strip_prefix("gitdir: ").expect("gitdir pointer"));
        std::fs::write(admin.join("config.worktree"), "[core]\n\tfsmonitor = true\n").expect("inject worktree config");
        std::fs::write(worktree.join(".git"), "gitdir: ../.git/worktrees/work\n").expect("relative worktree pointer");
        let error = guard_host_git_config("git", &["status"], &worktree).expect_err("worktree config drift");
        assert!(error.contains("core.fsmonitor"));
        let git_dir = repo.path().join(".git");
        git(repo.path(), &["config", "core.fsmonitor", "true"]);
        let error = guard_host_git_config("git", &["--git-dir", git_dir.to_str().expect("UTF-8 path"), "status"], Path::new("/"))
            .expect_err("explicit git dir drift");
        assert!(error.contains("core.fsmonitor"));
    }

    #[tokio::test]
    async fn injected_fsmonitor_blocks_host_git_before_it_runs() {
        let repo = tempfile::tempdir().expect("temporary repository");
        let marker = repo.path().join("fsmonitor-ran");
        git(repo.path(), &["init", "-q"]);
        let config = Command::new("git")
            .args(["config", "core.fsmonitor", &format!("touch {}", marker.display())])
            .current_dir(repo.path())
            .output()
            .expect("inject config");
        assert!(config.status.success());

        let runner = crate::ProcessCommandRunner;
        let error =
            runner.run("git", &["status"], repo.path(), &crate::ChannelLabel::Default).await.expect_err("unsafe key must block Git");
        assert!(error.contains("core.fsmonitor"), "{error}");
        assert!(!marker.exists());
    }
}
