//! Local generation pruning. The fleet installer holds its install lock across
//! selection and deletion; this command never connects to or starts a daemon.
use std::{
    collections::HashSet,
    fs, io,
    path::{Path, PathBuf},
};

use flotilla_core::providers::{ChannelLabel, CommandRunner};

/// Native executable lookup is separate from PID enumeration so tests can
/// inject process lifetimes without spawning processes or relying on /proc.
trait Processes {
    fn executable(&self, pid: i32) -> io::Result<PathBuf>;
    fn exists(&self, pid: i32) -> bool;
}

struct NativeProcesses;

impl Processes for NativeProcesses {
    fn executable(&self, pid: i32) -> io::Result<PathBuf> {
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::ffi::{OsStrExt, OsStringExt};
            let path = fs::read_link(format!("/proc/{pid}/exe"))?;
            let bytes = path.as_os_str().as_bytes();
            Ok(PathBuf::from(std::ffi::OsString::from_vec(bytes.strip_suffix(b" (deleted)").unwrap_or(bytes).to_vec())))
        }
        #[cfg(target_os = "macos")]
        {
            use std::os::unix::ffi::OsStringExt;
            let mut buffer = vec![0_u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
            // SAFETY: the buffer is writable for the supplied length.
            let length = unsafe { libc::proc_pidpath(pid, buffer.as_mut_ptr().cast(), buffer.len() as u32) };
            if length <= 0 {
                return Err(io::Error::last_os_error());
            }
            buffer.truncate(buffer.iter().position(|byte| *byte == 0).unwrap_or(length as usize));
            Ok(PathBuf::from(std::ffi::OsString::from_vec(buffer)))
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let _ = pid;
            Err(io::Error::other("unsupported process platform"))
        }
    }

    fn exists(&self, pid: i32) -> bool {
        #[cfg(unix)]
        {
            // SAFETY: signal zero only checks process existence/permissions.
            unsafe { libc::kill(pid, 0) == 0 || io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH) }
        }
        #[cfg(not(unix))]
        {
            let _ = pid;
            true
        }
    }
}

// CommandOutput has no exit code. Preserve pgrep's 0/1 distinction explicitly;
// neither stderr matching nor treating every failed command as "no processes"
// would fail closed when process enumeration is unavailable.
const ENUMERATE: &str = "pgrep -x flotillad; result=$?; printf '\\npgrep-status:%s\\n' \"$result\"";

async fn protected(root: &Path, runner: &impl CommandRunner, processes: &impl Processes) -> Result<HashSet<PathBuf>, String> {
    let releases = root.join("releases");
    let mut protected = HashSet::new();
    for name in ["current", "previous"] {
        let link = root.join(name);
        match fs::symlink_metadata(&link) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                let target = fs::canonicalize(&link).ok();
                if let Some(target) = target.filter(|target| target.parent() == Some(releases.as_path()) && target.is_dir()) {
                    protected.insert(target);
                } else {
                    let recovery = if name == "previous" { ", or remove the stale previous link" } else { "" };
                    return Err(format!("unsafe or missing {name} target: {}; inspect and restore the link to an installed release{recovery} before retrying", link.display()));
                }
            }
            Ok(_) => return Err(format!("{name} is not a symlink")),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.to_string()),
        }
    }
    let output = runner.run("sh", &["-c", ENUMERATE], Path::new("/"), &ChannelLabel::Command("fleet prune".into())).await?;
    let (pids, status) = output.trim_end().rsplit_once("\npgrep-status:").ok_or("cannot enumerate running flotillad processes")?;
    match status {
        "1" => return Ok(protected),
        "0" if !pids.trim().is_empty() => {}
        "0" => return Err("process discovery returned no executable PIDs".into()),
        _ => return Err("cannot enumerate running flotillad processes".into()),
    }
    for pid in pids.split_whitespace() {
        let pid: i32 = pid.parse().map_err(|_| "invalid process PID")?;
        if pid <= 0 {
            return Err("invalid process PID".into());
        }
        let executable = match processes.executable(pid) {
            Ok(path) => path,
            Err(_) if !processes.exists(pid) => continue,
            Err(error) => return Err(error.to_string()),
        };
        // Like Path.resolve(strict=False), keep a deleted executable's lexical
        // path while resolving existing symlink ancestors.
        let executable = resolve_missing(&executable).map_err(|error| error.to_string())?;
        if let Ok(relative) = executable.strip_prefix(&releases) {
            let mut parts = relative.components();
            if let (Some(generation), Some(_)) = (parts.next(), parts.next()) {
                protected.insert(releases.join(generation));
            }
        }
    }
    Ok(protected)
}

fn resolve_missing(path: &Path) -> io::Result<PathBuf> {
    match fs::canonicalize(path) {
        Ok(path) => Ok(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let parent = path.parent().ok_or(error)?;
            Ok(resolve_missing(parent)?.join(path.file_name().ok_or_else(|| io::Error::other("missing filename"))?))
        }
        Err(error) => Err(error),
    }
}

fn generation_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name.as_bytes()[0].is_ascii_alphanumeric()
        && name.bytes().all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
}

fn candidates(mut generations: Vec<PathBuf>, protected: &HashSet<PathBuf>, keep: usize) -> Vec<PathBuf> {
    generations.sort_by(|a, b| b.file_name().cmp(&a.file_name()));
    generations.into_iter().filter(|path| !protected.contains(path)).skip(keep).collect()
}

fn writable_tree(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let metadata = fs::symlink_metadata(path)?;
        if metadata.is_dir() {
            fs::set_permissions(path, fs::Permissions::from_mode(metadata.permissions().mode() | 0o200))?;
            for entry in fs::read_dir(path)? {
                writable_tree(&entry?.path())?;
            }
        }
    }
    Ok(())
}

pub async fn run(root: &Path, keep: &str, dry_run: bool, runner: &impl CommandRunner) -> Result<(), String> {
    prune(root, keep, dry_run, runner, &NativeProcesses, |name| {
        println!("fleet-install: {} {name}", if dry_run { "would prune" } else { "pruning" });
    })
    .await
    .map_err(|error| format!("fleet-install: pruning failed: {error}"))
}

async fn prune(
    root: &Path,
    keep: &str,
    dry_run: bool,
    runner: &impl CommandRunner,
    processes: &impl Processes,
    mut report: impl FnMut(&str),
) -> Result<(), String> {
    if keep.is_empty() || !keep.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err("FLEET_INSTALL_KEEP_OTHERS must be a non-negative integer".into());
    }
    // Python accepts arbitrary-sized integers. Saturating retains everything
    // rather than rejecting a valid retention value larger than usize::MAX.
    let keep = keep.bytes().fold(0_usize, |n, digit| n.saturating_mul(10).saturating_add((digit - b'0') as usize));
    let absolute =
        if root.is_absolute() { root.to_path_buf() } else { std::env::current_dir().map_err(|error| error.to_string())?.join(root) };
    let root = resolve_missing(&absolute).map_err(|error| error.to_string())?;
    let initial = protected(&root, runner, processes).await?;
    let releases = root.join("releases");
    let entries = match fs::read_dir(&releases) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.to_string()),
    };
    let mut generations = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| error.to_string())?;
        if entry.file_type().map_err(|error| error.to_string())?.is_dir() && generation_name(&entry.file_name().to_string_lossy()) {
            generations.push(entry.path());
        }
    }
    let mut names: Vec<_> =
        generations.iter().map(|path| path.file_name().expect("generation filename").to_string_lossy().into_owned()).collect();
    names.sort_by_key(|name| std::cmp::Reverse(name.len()));
    for release in candidates(generations, &initial, keep) {
        if protected(&root, runner, processes).await?.contains(&release) {
            continue;
        }
        let name = release.file_name().expect("generation filename").to_string_lossy();
        report(&name);
        if dry_run {
            continue;
        }
        writable_tree(&release).and_then(|()| fs::remove_dir_all(&release)).map_err(|error| error.to_string())?;
        for entry in fs::read_dir(&releases).map_err(|error| error.to_string())? {
            let entry = entry.map_err(|error| error.to_string())?;
            let sibling = entry.file_name().to_string_lossy().into_owned();
            let owner = names.iter().find(|name| sibling.starts_with(&format!("{name}.")));
            if owner.map(String::as_str) == Some(name.as_ref()) && !entry.file_type().map_err(|error| error.to_string())?.is_dir() {
                fs::remove_file(entry.path()).map_err(|error| error.to_string())?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{HashMap, VecDeque},
        sync::Mutex,
    };

    use async_trait::async_trait;
    use flotilla_core::providers::CommandOutput;
    use tempfile::TempDir;

    use super::*;

    // Process-boundary double: supplies executable paths and exited/unreadable
    // PIDs. The filesystem adapter is real because symlinks, modes and sidecar
    // deletion are part of the contract, rather than subprocess orchestration.
    #[derive(Default)]
    struct FakeProcesses {
        paths: HashMap<i32, PathBuf>,
        exited: HashSet<i32>,
    }
    impl Processes for FakeProcesses {
        fn executable(&self, pid: i32) -> io::Result<PathBuf> {
            self.paths.get(&pid).cloned().ok_or_else(|| io::Error::other("unreadable executable"))
        }
        fn exists(&self, pid: i32) -> bool {
            !self.exited.contains(&pid)
        }
    }

    // Subprocess-boundary double; verifies enumeration delegation and permits
    // PID enumeration to change between selection and each deletion.
    struct MockRunner {
        responses: Mutex<VecDeque<Result<String, String>>>,
        calls: Mutex<Vec<(String, Vec<String>)>>,
    }
    impl MockRunner {
        fn new(responses: Vec<Result<String, String>>) -> Self {
            Self { responses: Mutex::new(responses.into()), calls: Mutex::new(Vec::new()) }
        }
        fn calls(&self) -> Vec<(String, Vec<String>)> {
            self.calls.lock().expect("calls").clone()
        }
    }
    #[async_trait]
    impl CommandRunner for MockRunner {
        async fn run(&self, command: &str, args: &[&str], _cwd: &Path, _label: &ChannelLabel) -> Result<String, String> {
            self.calls.lock().expect("calls").push((command.into(), args.iter().map(|arg| (*arg).into()).collect()));
            self.responses.lock().expect("responses").pop_front().expect("expected enumeration call")
        }
        async fn run_output(&self, _command: &str, _args: &[&str], _cwd: &Path, _label: &ChannelLabel) -> Result<CommandOutput, String> {
            panic!("pruning uses run")
        }
        async fn exists(&self, _command: &str, _args: &[&str]) -> bool {
            panic!("pruning uses run")
        }
    }

    fn runner(outputs: &[&str]) -> MockRunner {
        MockRunner::new(outputs.iter().map(|output| Ok((*output).into())).collect())
    }

    fn fixture(names: &[&str]) -> TempDir {
        let root = tempfile::tempdir().expect("temporary fleet");
        fs::create_dir(root.path().join("releases")).expect("releases");
        for name in names {
            fs::create_dir_all(root.path().join("releases").join(name).join("bin")).expect("generation");
            fs::write(root.path().join("releases").join(format!("{name}.validator.bak")), "validator").expect("sidecar");
        }
        root
    }

    #[cfg(unix)]
    fn link(root: &Path, name: &str, target: &str) {
        std::os::unix::fs::symlink(target, root.join(name)).expect("link");
    }

    // The native adapter reads the actual executable, rather than argv or a
    // fleet symlink. Inspect this test process without spawning a subprocess.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn native_executable_lookup() {
        let pid = std::process::id() as i32;
        assert!(NativeProcesses.exists(pid));
        assert_eq!(
            fs::canonicalize(NativeProcesses.executable(pid).expect("executable")).expect("canonical executable"),
            fs::canonicalize(std::env::current_exe().expect("current executable")).expect("canonical current executable")
        );
    }

    // Retention is by generation name, excluding every protected generation.
    // Exhaustive generator: every subset of eight protected releases, K from
    // zero through beyond capacity, and empty input. This covers duplicates
    // in protection, newest/oldest running positions and both K boundaries.
    #[test]
    fn retention_property() {
        for count in 0..=8 {
            for mask in 0..(1 << count) {
                for keep in 0..=9 {
                    let releases: Vec<_> = (0..count).map(|i| PathBuf::from(format!("{i:02}"))).collect();
                    let protected: HashSet<_> =
                        releases.iter().enumerate().filter(|(i, _)| mask & (1 << i) != 0).map(|(_, p)| p.clone()).collect();
                    let removed = candidates(releases.clone(), &protected, keep);
                    let mut unprotected_seen = 0;
                    for path in releases.iter().rev() {
                        if protected.contains(path) {
                            assert!(!removed.contains(path));
                        } else {
                            assert_eq!(removed.contains(path), unprotected_seen >= keep);
                            unprotected_seen += 1;
                        }
                    }
                    let retained: Vec<_> = releases.into_iter().filter(|p| !removed.contains(p)).collect();
                    assert!(candidates(retained, &protected, keep).is_empty(), "pruning is idempotent");
                }
            }
        }
    }

    // Current, previous and duplicate running executable paths retain their
    // releases and sidecars in addition to K others, even when read-only.
    #[cfg(unix)]
    #[tokio::test]
    async fn protects_links_processes_and_k_others() {
        use std::os::unix::fs::PermissionsExt;
        for keep in [0, 1, 3, 9] {
            let root = fixture(&["01", "02", "03", "04", "05", "06", "07", "08"]);
            link(root.path(), "current", "releases/01");
            link(root.path(), "previous", "releases/02");
            let processes = FakeProcesses {
                paths: [
                    (10, root.path().join("releases/03/bin/flotillad")),
                    (11, root.path().join("releases/03/bin/flotillad")),
                    (12, root.path().join("releases/08/bin/flotillad")),
                ]
                .into(),
                exited: HashSet::new(),
            };
            let output = "10 11 12\npgrep-status:0\n";
            let commands = runner(&[output, output, output, output, output]);
            let readonly = root.path().join("releases/04");
            fs::set_permissions(&readonly, fs::Permissions::from_mode(0o555)).expect("readonly");
            prune(root.path(), &keep.to_string(), false, &commands, &processes, |_| {}).await.expect("prune");
            for i in 1..=8 {
                let retained = [1, 2, 3, 8].contains(&i) || i >= 8 - keep;
                assert_eq!(root.path().join(format!("releases/{i:02}")).is_dir(), retained);
                assert_eq!(root.path().join(format!("releases/{i:02}.validator.bak")).is_file(), retained);
            }
            assert!(commands.calls().iter().all(|(command, args)| command == "sh" && args == &["-c", ENUMERATE]));
        }
    }

    // Dry-run reports exactly the selected releases without changing modes,
    // directories, sidecars, or directory symlinks. Invalid names are ignored.
    #[cfg(unix)]
    #[tokio::test]
    async fn dry_run_and_generation_names() {
        use std::os::unix::fs::PermissionsExt;
        let root = fixture(&["a", "b", ".invalid"]);
        link(&root.path().join("releases"), "c", "a");
        let release = root.path().join("releases/a");
        fs::set_permissions(&release, fs::Permissions::from_mode(0o555)).expect("readonly");
        let mut report = Vec::new();
        prune(root.path(), "1", true, &runner(&["\npgrep-status:1\n", "\npgrep-status:1\n"]), &FakeProcesses::default(), |name| {
            report.push(name.to_owned())
        })
        .await
        .expect("dry run");
        assert_eq!(report, ["a"]);
        assert_eq!(fs::metadata(&release).expect("metadata").permissions().mode() & 0o777, 0o555);
        assert!(root.path().join("releases/a.validator.bak").exists());
        assert!(root.path().join("releases/c").is_symlink());
        assert!(root.path().join("releases/.invalid").is_dir());
    }

    // Longest-prefix ownership preserves dotted generations' backups,
    // including dangling symlinks, while deleting a prefix generation.
    #[cfg(unix)]
    #[tokio::test]
    async fn dotted_sidecars() {
        let root = fixture(&["a", "a.b", "a.b.c"]);
        link(root.path(), "current", "releases/a.b");
        link(root.path(), "previous", "releases/a.b.c");
        link(&root.path().join("releases"), "a.b.c.extra", "missing");
        link(&root.path().join("releases"), "a.extra", "missing");
        fs::create_dir(root.path().join("releases/a.extra-dir")).expect("sidecar directory");
        // This valid generation directory itself is retained as the newest other.
        prune(root.path(), "1", false, &runner(&["\npgrep-status:1\n", "\npgrep-status:1\n"]), &FakeProcesses::default(), |_| {})
            .await
            .expect("prune");
        assert!(!root.path().join("releases/a").exists());
        assert!(!root.path().join("releases/a.validator.bak").exists());
        assert!(!root.path().join("releases/a.extra").is_symlink());
        assert!(root.path().join("releases/a.b.validator.bak").exists());
        assert!(root.path().join("releases/a.b.c.extra").is_symlink());
        assert!(root.path().join("releases/a.extra-dir").is_dir());
    }

    // Dangling, external and non-symlink current/previous entries fail closed
    // before process enumeration or any directory/mode mutation.
    #[cfg(unix)]
    #[tokio::test]
    async fn unsafe_links_refuse() {
        for name in ["current", "previous"] {
            for target in ["releases/missing", "..", "regular"] {
                let root = fixture(&["old"]);
                if target == "regular" {
                    fs::write(root.path().join(name), "regular").expect("regular");
                } else {
                    link(root.path(), name, target);
                }
                let commands = runner(&[]);
                let error = prune(root.path(), "0", false, &commands, &FakeProcesses::default(), |_| {}).await.expect_err("unsafe link");
                assert!(error.contains(name));
                if target != "regular" {
                    assert!(error.contains("restore the link to an installed release"));
                }
                assert!(root.path().join("releases/old").is_dir());
                assert!(commands.calls().is_empty());
            }
        }
    }

    // Enumeration errors, malformed PIDs and unreadable live processes refuse
    // deletion. A process that exited between enumeration and inspection is safe.
    #[tokio::test]
    async fn process_discovery_fails_closed() {
        for output in
            ["\npgrep-status:2\n", "\npgrep-status:0\n", "bad\npgrep-status:0\n", "-1\npgrep-status:0\n", "10\npgrep-status:0\n", "garbage"]
        {
            let root = fixture(&["old"]);
            assert!(prune(root.path(), "0", false, &runner(&[output]), &FakeProcesses::default(), |_| {}).await.is_err());
            assert!(root.path().join("releases/old").exists());
        }
        let root = fixture(&["old"]);
        let commands = MockRunner::new(vec![Err("cannot spawn".into())]);
        assert!(prune(root.path(), "0", false, &commands, &FakeProcesses::default(), |_| {}).await.is_err());
        assert!(root.path().join("releases/old").exists());
        let processes = FakeProcesses { paths: HashMap::new(), exited: [10].into() };
        prune(root.path(), "0", false, &runner(&["10\npgrep-status:0\n", "10\npgrep-status:0\n"]), &processes, |_| {})
            .await
            .expect("exited process");
        assert!(!root.path().join("releases/old").exists());
    }

    // A daemon appearing after selection protects its generation before even
    // its directory modes change; another candidate can still be deleted.
    #[cfg(unix)]
    #[tokio::test]
    async fn rechecks_before_each_deletion() {
        use std::os::unix::fs::PermissionsExt;
        let root = fixture(&["a", "b"]);
        fs::set_permissions(root.path().join("releases/b"), fs::Permissions::from_mode(0o555)).expect("readonly");
        let processes = FakeProcesses { paths: [(10, root.path().join("releases/b/bin/flotillad"))].into(), exited: HashSet::new() };
        let commands = runner(&["\npgrep-status:1\n", "10\npgrep-status:0\n", "10\npgrep-status:0\n"]);
        prune(root.path(), "0", false, &commands, &processes, |_| {}).await.expect("prune");
        assert!(root.path().join("releases/b").is_dir());
        assert_eq!(fs::metadata(root.path().join("releases/b")).expect("metadata").permissions().mode() & 0o777, 0o555);
        assert!(!root.path().join("releases/a").exists());
    }

    // Failed discovery at the final recheck refuses before deleting or changing
    // modes. Executables outside the fleet do not consume its retention slots.
    #[tokio::test]
    async fn failed_recheck_and_external_executable() {
        let root = fixture(&["old"]);
        let mut reported = Vec::new();
        let result =
            prune(root.path(), "0", false, &runner(&["\npgrep-status:1\n", "\npgrep-status:2\n"]), &FakeProcesses::default(), |name| {
                reported.push(name.to_owned())
            })
            .await;
        assert!(result.is_err());
        assert!(reported.is_empty());
        assert!(root.path().join("releases/old.validator.bak").exists());
        assert!(root.path().join("releases/old").is_dir());
        let outside = tempfile::tempdir().expect("outside executable");
        let processes = FakeProcesses { paths: [(10, outside.path().join("flotillad"))].into(), exited: HashSet::new() };
        prune(root.path(), "0", false, &runner(&["10\npgrep-status:0\n", "10\npgrep-status:0\n"]), &processes, |_| {})
            .await
            .expect("prune with unrelated daemon");
        assert!(!root.path().join("releases/old").exists());
    }

    // Nondecimal retention refuses before discovery; empty stores are no-ops,
    // and arbitrarily large valid K retains all releases as Python did.
    #[tokio::test]
    async fn retention_validation_and_empty_store() {
        let root = fixture(&["old"]);
        for keep in ["", "-1", "+1", " 1", "1.0", "invalid"] {
            let commands = runner(&[]);
            assert!(prune(root.path(), keep, false, &commands, &FakeProcesses::default(), |_| {})
                .await
                .expect_err("invalid retention")
                .contains("non-negative integer"));
            assert!(commands.calls().is_empty());
            assert!(root.path().join("releases/old").exists());
        }
        prune(
            root.path(),
            "9999999999999999999999999999999999999999",
            false,
            &runner(&["\npgrep-status:1\n"]),
            &FakeProcesses::default(),
            |_| {},
        )
        .await
        .expect("huge K");
        assert!(root.path().join("releases/old").exists());
        let empty = tempfile::tempdir().expect("empty root");
        prune(empty.path(), "0", false, &runner(&["\npgrep-status:1\n"]), &FakeProcesses::default(), |_| {}).await.expect("empty store");
        assert!(!empty.path().join("releases").exists());
        assert!(generation_name(&"a".repeat(128)));
        assert!(!generation_name(&"a".repeat(129)));
        assert!(!generation_name("é"));
    }
}
