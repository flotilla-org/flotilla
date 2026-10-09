use std::{
    collections::BTreeSet,
    fs,
    path::Path,
    process::Command as ProcessCommand,
    sync::{atomic::AtomicBool, Arc, Mutex as StdMutex},
};

use async_trait::async_trait;
use flotilla_core::{
    in_process::DEFAULT_PROVISIONING_NAMESPACE as NAMESPACE,
    providers::{ChannelLabel, CommandOutput, CommandRunner, ProcessCommandRunner},
};
use flotilla_paths::path_context::ExecutionEnvironmentPath;
use flotilla_resources::{
    CheckoutBranchProvenance, Convoy, ConvoySpec, Environment, EnvironmentSpec, HostDirectEnvironmentSpec, InMemoryBackend,
    ResourceBackend, SqliteBackend,
};
use tempfile::TempDir;

use super::*;
use crate::{
    reconcilers::{
        clone::runtime::clone_staging_path, runtime_tests::FailFirstCloneProcessRunner, BranchPreservationReason, CheckoutRemoval,
        CheckoutRemovalOutcome,
    },
    test_git_repo::TestGitRepo,
};

#[tokio::test]
async fn checkout_runtime_creates_convoy_branch_from_snapshotted_base() {
    let temp = TempDir::new().expect("tempdir");
    let clone = TestGitRepo::init(temp.path().join("clone")).with_initial_commit();
    let target = temp.path().join("workspace/flotilla");
    let runtime =
        CheckoutControllerRuntime { vcs: None, runner: Arc::new(ProcessCommandRunner), change_requests: None, forges: Vec::new() };

    runtime
        .create_worktree(
            clone.path().to_str().expect("utf-8 clone path"),
            "feature/multi-repo",
            Some("main"),
            target.to_str().expect("utf-8 target path"),
        )
        .await
        .expect("worktree should create");

    let branch = ProcessCommand::new("git")
        .args(["-C", target.to_str().expect("utf-8 target path"), "branch", "--show-current"])
        .output()
        .expect("git should run");
    assert!(branch.status.success());
    assert_eq!(String::from_utf8(branch.stdout).expect("utf-8 branch").trim(), "feature/multi-repo");
}

#[tokio::test]
async fn checkout_runtime_branches_convoy_off_fetched_origin_tip_when_local_base_is_stale() {
    let temp = TempDir::new().expect("tempdir");
    let origin = TestGitRepo::init(temp.path().join("origin")).with_initial_commit();
    let origin_path = origin.path().to_str().expect("utf-8 origin path");

    let clone_path_buf = temp.path().join("clone");
    let clone_path = clone_path_buf.to_str().expect("utf-8 clone path");
    assert!(ProcessCommand::new("git").args(["clone", origin_path, clone_path]).status().expect("git clone should run").success());

    // Advance origin's main past what the shared clone saw at clone time; nothing
    // ever fetches or fast-forwards a shared clone's local main on its own.
    fs::write(origin.path().join("advance.txt"), "advance\n").expect("write advance file");
    assert!(ProcessCommand::new("git").args(["-C", origin_path, "add", "advance.txt"]).status().expect("git add should run").success());
    assert!(ProcessCommand::new("git")
        .args(["-C", origin_path, "commit", "-m", "advance main"])
        .status()
        .expect("git commit should run")
        .success());
    let origin_tip = ProcessCommand::new("git").args(["-C", origin_path, "rev-parse", "main"]).output().expect("git rev-parse should run");
    let origin_tip = String::from_utf8(origin_tip.stdout).expect("utf-8 rev").trim().to_string();

    let target = temp.path().join("workspace/flotilla");
    let runtime =
        CheckoutControllerRuntime { vcs: None, runner: Arc::new(ProcessCommandRunner), change_requests: None, forges: Vec::new() };

    runtime
        .create_worktree(clone_path, "feature/fresh-from-origin", Some("main"), target.to_str().expect("utf-8 target path"))
        .await
        .expect("worktree should create");

    let head = ProcessCommand::new("git")
        .args(["-C", target.to_str().expect("utf-8 target path"), "rev-parse", "HEAD"])
        .output()
        .expect("git rev-parse should run");
    let head = String::from_utf8(head.stdout).expect("utf-8 head").trim().to_string();

    assert_eq!(head, origin_tip, "fresh convoy branch should start from the fetched origin tip, not the shared clone's stale local main");
}

#[tokio::test]
async fn checkout_runtime_branches_off_a_force_pushed_origin_base_ref() {
    let temp = TempDir::new().expect("tempdir");
    let origin = TestGitRepo::init(temp.path().join("origin")).with_initial_commit();
    let origin_path = origin.path().to_str().expect("utf-8 origin path");

    let clone_path_buf = temp.path().join("clone");
    let clone_path = clone_path_buf.to_str().expect("utf-8 clone path");
    assert!(ProcessCommand::new("git").args(["clone", origin_path, clone_path]).status().expect("git clone should run").success());

    // Rewrite origin's main to a commit that is not a descendant of what the shared
    // clone cached at clone time, simulating a rebase/force-push of the base branch.
    // A plain (non-force) fetch cannot fast-forward onto this and must be rejected.
    assert!(ProcessCommand::new("git")
        .args(["-C", origin_path, "commit", "--amend", "-m", "rewritten root commit"])
        .status()
        .expect("git commit --amend should run")
        .success());
    let origin_tip = ProcessCommand::new("git").args(["-C", origin_path, "rev-parse", "main"]).output().expect("git rev-parse should run");
    let origin_tip = String::from_utf8(origin_tip.stdout).expect("utf-8 rev").trim().to_string();
    let cached_tip = ProcessCommand::new("git")
        .args(["-C", clone_path, "rev-parse", "refs/remotes/origin/main"])
        .output()
        .expect("git rev-parse should run");
    let cached_tip = String::from_utf8(cached_tip.stdout).expect("utf-8 rev").trim().to_string();
    assert_ne!(origin_tip, cached_tip, "the rewrite should actually diverge from what the clone cached");

    let target = temp.path().join("workspace/flotilla");
    let runtime =
        CheckoutControllerRuntime { vcs: None, runner: Arc::new(ProcessCommandRunner), change_requests: None, forges: Vec::new() };

    runtime
        .create_worktree(clone_path, "feature/onto-rewritten-base", Some("main"), target.to_str().expect("utf-8 target path"))
        .await
        .expect("worktree should create");

    let head = ProcessCommand::new("git")
        .args(["-C", target.to_str().expect("utf-8 target path"), "rev-parse", "HEAD"])
        .output()
        .expect("git rev-parse should run");
    let head = String::from_utf8(head.stdout).expect("utf-8 head").trim().to_string();

    assert_eq!(
        head, origin_tip,
        "a force-updating fetch should land on the rewritten origin tip, not the clone's cached pre-rewrite tracking ref"
    );
}

#[tokio::test]
async fn checkout_runtime_falls_back_to_local_base_ref_when_fetch_fails() {
    let temp = TempDir::new().expect("tempdir");
    // A reachable but empty origin: `git remote get-url` and `ls-remote` for the
    // (nonexistent) convoy branch succeed, but fetching `main` fails because origin
    // never advertises it — exercising the same fetch-error path as an offline host.
    let origin = TestGitRepo::init(temp.path().join("origin"));
    let clone =
        TestGitRepo::init(temp.path().join("clone")).with_initial_commit().with_origin(origin.path().to_str().expect("utf-8 origin path"));
    let target = temp.path().join("workspace/flotilla");
    let runtime =
        CheckoutControllerRuntime { vcs: None, runner: Arc::new(ProcessCommandRunner), change_requests: None, forges: Vec::new() };

    runtime
        .create_worktree(
            clone.path().to_str().expect("utf-8 clone path"),
            "feature/offline",
            Some("main"),
            target.to_str().expect("utf-8 target path"),
        )
        .await
        .expect("worktree should still create when fetching the base ref fails");

    let branch = ProcessCommand::new("git")
        .args(["-C", target.to_str().expect("utf-8 target path"), "branch", "--show-current"])
        .output()
        .expect("git should run");
    assert_eq!(String::from_utf8(branch.stdout).expect("utf-8 branch").trim(), "feature/offline");
}

#[tokio::test]
async fn checkout_runtime_recovers_a_worktree_after_its_completion_is_lost() {
    let temp = TempDir::new().expect("tempdir");
    let clone = TestGitRepo::init(temp.path().join("clone")).with_initial_commit();
    let target = temp.path().join("workspace/flotilla");
    let runtime =
        CheckoutControllerRuntime { vcs: None, runner: Arc::new(ProcessCommandRunner), change_requests: None, forges: Vec::new() };

    let first = runtime
        .create_worktree(
            clone.path().to_str().expect("utf-8 clone path"),
            "feature/redrive",
            Some("main"),
            target.to_str().expect("utf-8 target path"),
        )
        .await
        .expect("first worktree actuation should succeed");
    let recovered = runtime
        .create_worktree(
            clone.path().to_str().expect("utf-8 clone path"),
            "feature/redrive",
            Some("main"),
            target.to_str().expect("utf-8 target path"),
        )
        .await
        .expect("redrive should recover the worktree already created on disk");

    assert_eq!(recovered, first);
}

// #2610: teardown removes only empty convoy parents, and logs retained contents.
// Thread-local log capture must remain on this current-thread runtime.
#[tokio::test(flavor = "current_thread")]
async fn convoy_parent_cleanup_contract() {
    for extra in [None, Some("another-checkout"), Some("unexpected-file")] {
        let temp = TempDir::new().expect("tempdir");
        let parent = temp.path().join("convoy-contract");
        let target = parent.join("branch/repository");
        let clone = TestGitRepo::init(temp.path().join("clone")).with_initial_commit();
        let runtime =
            CheckoutControllerRuntime { vcs: None, runner: Arc::new(ProcessCommandRunner), change_requests: None, forges: Vec::new() };
        assert!(!parent.exists(), "materialisation must recreate missing convoy parents");
        runtime
            .create_worktree(
                clone.path().to_str().expect("test fixture operation succeeds"),
                "feature/cleanup",
                Some("main"),
                target.to_str().expect("test fixture operation succeeds"),
            )
            .await
            .expect("test fixture operation succeeds");
        fs::write(target.join("uncommitted.txt"), "archive this").expect("test fixture operation succeeds");
        if let Some(extra) = extra {
            let extra_path = parent.join(extra);
            if extra == "another-checkout" {
                runtime
                    .create_worktree(
                        clone.path().to_str().expect("clone path"),
                        "feature/sibling",
                        Some("main"),
                        extra_path.to_str().expect("sibling path"),
                    )
                    .await
                    .expect("create sibling checkout");
            } else {
                fs::write(&extra_path, "keep me").expect("test fixture operation succeeds");
            }
        }
        let log = tempfile::NamedTempFile::new().expect("test fixture operation succeeds");
        let subscriber =
            tracing_subscriber::fmt().with_ansi(false).with_writer(log.reopen().expect("test fixture operation succeeds")).finish();
        let _logging = tracing::subscriber::set_default(subscriber);
        let outcome = runtime
            .remove_checkout(&CheckoutRemoval::ForcedWorktree {
                clone_path: clone.path().to_str().expect("test fixture operation succeeds").into(),
                branch: "feature/cleanup".into(),
                target_path: target.to_str().expect("test fixture operation succeeds").into(),
            })
            .await
            .expect("test fixture operation succeeds");
        let CheckoutRemovalOutcome::ArchivedAndRemoved { archive_path } = outcome else { panic!("forced removal must archive first") };
        assert!(Path::new(&archive_path).join("worktree.tar.gz").exists());
        assert!(!target.exists());
        assert_eq!(parent.exists(), extra.is_some());
        if extra.is_some() {
            let logs = fs::read_to_string(log.path()).expect("test fixture operation succeeds");
            assert!(logs.contains("convoy-contract") && logs.contains("keeping convoy checkout parent"), "{logs}");
        }
    }
}

// #2610: a convoy in another namespace protects its shared host root,
// using the same sanitized directory name as checkout provisioning.
// Roots belonging to another host must never be swept by this daemon.
#[tokio::test]
async fn empty_convoy_sweep_respects_namespaces_and_hosts() {
    let temp = TempDir::new().expect("test fixture operation succeeds");
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let local = temp.path().join("local");
    let remote = temp.path().join("remote");
    fs::create_dir_all(local.join("convoy-live")).expect("test fixture operation succeeds");
    fs::create_dir_all(local.join("convoy-gone")).expect("test fixture operation succeeds");
    fs::create_dir_all(remote.join("convoy-gone")).expect("test fixture operation succeeds");
    for (name, host, root) in [("local-env", "local", &local), ("remote-env", "remote", &remote)] {
        backend
            .using::<Environment>("environments")
            .create(
                &empty_meta(name),
                &EnvironmentSpec {
                    host_direct: Some(HostDirectEnvironmentSpec {
                        host_ref: host.into(),
                        repo_default_dir: root.to_str().expect("test fixture operation succeeds").into(),
                    }),
                    docker: None,
                },
            )
            .await
            .expect("test fixture operation succeeds");
    }
    backend
        .using::<Convoy>("another-project")
        .create(&empty_meta("convoy/live"), &ConvoySpec::builder().workflow_ref("test".into()).build())
        .await
        .expect("test fixture operation succeeds");
    sweep_host_empty_convoy_directories(&backend, "local").await.expect("test fixture operation succeeds");
    assert!(local.join("convoy-live").exists());
    assert!(!local.join("convoy-gone").exists());
    assert!(remote.join("convoy-gone").exists());
}

// #2610: failed resource discovery must leave every candidate untouched.
// Corrupt the real SQLite store's sequence table to make a typed listing
// fail after namespace discovery, both for Convoys and for Environments.
#[tokio::test]
async fn empty_convoy_sweep_fails_closed_on_listing_errors() {
    for has_convoy_namespace in [false, true] {
        let temp = TempDir::new().expect("tempdir");
        let root = temp.path().join("repos");
        let candidate = root.join("convoy-orphan");
        fs::create_dir_all(&candidate).expect("empty stale convoy directory");
        let database = temp.path().join("resources.sqlite");
        let backend = ResourceBackend::Sqlite(SqliteBackend::open(&database).expect("resource store"));
        backend
            .using::<Environment>(NAMESPACE)
            .create(
                &empty_meta("host-direct"),
                &EnvironmentSpec {
                    host_direct: Some(HostDirectEnvironmentSpec {
                        host_ref: "local".into(),
                        repo_default_dir: root.to_str().expect("root path").into(),
                    }),
                    docker: None,
                },
            )
            .await
            .expect("host environment");
        if has_convoy_namespace {
            backend
                .using::<Convoy>(NAMESPACE)
                .create(&empty_meta("convoy-live"), &ConvoySpec::builder().workflow_ref("test".into()).build())
                .await
                .expect("live convoy");
        }
        let connection = rusqlite::Connection::open(&database).expect("fault injection connection");
        connection.execute("DROP TABLE resource_sequences", []).expect("inject listing failure");
        drop(connection);
        let error = sweep_host_empty_convoy_directories(&backend, "local").await.expect_err("failed listing must stop sweep");
        assert!(error.contains("resource_sequences"), "{error}");
        assert!(candidate.exists(), "listing failure must preserve empty candidates");
    }
}

// #2610: periodic sweeping preserves live, nonempty, unrelated and symlink entries; repeating is harmless.
#[tokio::test]
async fn empty_convoy_sweep_contract() {
    let temp = TempDir::new().expect("test fixture operation succeeds");
    for name in ["convoy-gone", "convoy-live", "convoy-nonempty", "unrelated"] {
        fs::create_dir(temp.path().join(name)).expect("test fixture operation succeeds");
    }
    fs::write(temp.path().join("convoy-nonempty/file"), "keep").expect("test fixture operation succeeds");
    std::os::unix::fs::symlink(temp.path().join("convoy-live"), temp.path().join("convoy-link")).expect("test fixture operation succeeds");
    let live = BTreeSet::from(["convoy-live".to_string()]);
    for _ in 0..2 {
        sweep_empty_convoy_directories(temp.path(), &live).await.expect("test fixture operation succeeds");
        assert!(!temp.path().join("convoy-gone").exists());
        for name in ["convoy-live", "convoy-nonempty", "unrelated", "convoy-link"] {
            assert!(temp.path().join(name).exists(), "{name}");
        }
    }
}

#[tokio::test]
async fn checkout_runtime_removes_zero_commit_worktree_without_git_or_directory_debris() {
    let temp = TempDir::new().expect("tempdir");
    let clone = TestGitRepo::init(temp.path().join("clone")).with_initial_commit();
    let convoy_dir = temp.path().join("checkout-root/convoy-a");
    let target = convoy_dir.join("flotilla.feature-cleanup");
    let runtime =
        CheckoutControllerRuntime { vcs: None, runner: Arc::new(ProcessCommandRunner), change_requests: None, forges: Vec::new() };

    let prepared = runtime
        .create_worktree(
            clone.path().to_str().expect("utf-8 clone path"),
            "feature/cleanup",
            Some("main"),
            target.to_str().expect("utf-8 target path"),
        )
        .await
        .expect("worktree should create");
    assert_eq!(prepared.branch_provenance, CheckoutBranchProvenance::CreatedForConvoy);
    assert!(prepared.commit.is_some(), "worktree should resolve its initial commit");
    let removal = CheckoutRemoval::Worktree {
        clone_path: clone.path().to_str().expect("utf-8 clone path").to_string(),
        branch: "feature/cleanup".to_string(),
        target_path: target.to_str().expect("utf-8 target path").to_string(),
    };
    assert_eq!(runtime.remove_checkout(&removal).await.expect("worktree should be removed"), CheckoutRemovalOutcome::Removed);

    let worktrees = ProcessCommand::new("git")
        .args(["-C", clone.path().to_str().expect("utf-8 clone path"), "worktree", "list", "--porcelain"])
        .output()
        .expect("git should list worktrees");
    assert!(worktrees.status.success());
    assert!(!String::from_utf8(worktrees.stdout).expect("utf-8 worktree list").contains(target.to_str().expect("utf-8 target path")));
    assert!(!convoy_dir.exists(), "empty convoy directory should be removed");

    let branch = ProcessCommand::new("git")
        .args(["-C", clone.path().to_str().expect("utf-8 clone path"), "show-ref", "--verify", "--quiet", "refs/heads/feature/cleanup"])
        .status()
        .expect("git should inspect the branch");
    assert!(!branch.success(), "zero-commit convoy branch should be deleted");
}

#[tokio::test]
async fn checkout_runtime_treats_a_never_created_worktree_as_removed() {
    let temp = TempDir::new().expect("tempdir");
    let clone = temp.path().join("clone-that-failed-auth");
    let target = temp.path().join("workspace/never-created");
    let runtime =
        CheckoutControllerRuntime { vcs: None, runner: Arc::new(ProcessCommandRunner), change_requests: None, forges: Vec::new() };
    let removal = CheckoutRemoval::Worktree {
        clone_path: clone.to_str().expect("utf-8 clone path").to_string(),
        branch: "feature/never-created".to_string(),
        target_path: target.to_str().expect("utf-8 target path").to_string(),
    };

    let outcome =
        runtime.remove_checkout(&removal).await.expect("a worktree that provisioning never created should not wedge finalization");

    assert_eq!(outcome, CheckoutRemovalOutcome::Removed);
}

#[tokio::test]
async fn checkout_runtime_cleans_git_state_when_only_the_worktree_directory_is_missing() {
    let temp = TempDir::new().expect("tempdir");
    let clone = TestGitRepo::init(temp.path().join("clone")).with_initial_commit();
    let target = temp.path().join("workspace/removed-out-of-band");
    let runtime =
        CheckoutControllerRuntime { vcs: None, runner: Arc::new(ProcessCommandRunner), change_requests: None, forges: Vec::new() };
    runtime
        .create_worktree(
            clone.path().to_str().expect("utf-8 clone path"),
            "feature/removed-out-of-band",
            Some("main"),
            target.to_str().expect("utf-8 target path"),
        )
        .await
        .expect("worktree should create");
    fs::remove_dir_all(&target).expect("remove worktree directory out of band");

    let outcome = runtime
        .remove_checkout(&CheckoutRemoval::Worktree {
            clone_path: clone.path().to_str().expect("utf-8 clone path").to_string(),
            branch: "feature/removed-out-of-band".to_string(),
            target_path: target.to_str().expect("utf-8 target path").to_string(),
        })
        .await
        .expect("missing worktree directory should still clean git state");

    assert_eq!(outcome, CheckoutRemovalOutcome::Removed);
    let branch = ProcessCommand::new("git")
        .args([
            "-C",
            clone.path().to_str().expect("utf-8 clone path"),
            "show-ref",
            "--verify",
            "--quiet",
            "refs/heads/feature/removed-out-of-band",
        ])
        .status()
        .expect("git should inspect the branch");
    assert!(!branch.success(), "zero-commit convoy branch should be deleted");
}

#[tokio::test]
async fn checkout_runtime_prunes_base_clone_after_removing_worktree() {
    struct RecordingProcessRunner {
        commands: Arc<StdMutex<Vec<Vec<String>>>>,
    }

    #[async_trait]
    impl CommandRunner for RecordingProcessRunner {
        async fn run(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<String, String> {
            self.commands
                .lock()
                .expect("command lock")
                .push(std::iter::once(cmd.to_string()).chain(args.iter().map(|arg| (*arg).to_string())).collect());
            ProcessCommandRunner.run(cmd, args, cwd, label).await
        }

        async fn run_output(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<CommandOutput, String> {
            self.commands
                .lock()
                .expect("command lock")
                .push(std::iter::once(cmd.to_string()).chain(args.iter().map(|arg| (*arg).to_string())).collect());
            ProcessCommandRunner.run_output(cmd, args, cwd, label).await
        }

        async fn exists(&self, cmd: &str, args: &[&str]) -> bool {
            ProcessCommandRunner.exists(cmd, args).await
        }
    }

    let temp = TempDir::new().expect("tempdir");
    let clone = TestGitRepo::init(temp.path().join("clone")).with_initial_commit();
    let target = temp.path().join("checkout-root/convoy-a/flotilla.feature-prune");
    let stale_target = temp.path().join("checkout-root/stale-convoy/flotilla.feature-stale");
    let commands = Arc::new(StdMutex::new(Vec::new()));
    let runtime = CheckoutControllerRuntime {
        vcs: None,
        runner: Arc::new(RecordingProcessRunner { commands: Arc::clone(&commands) }),
        change_requests: None,
        forges: Vec::new(),
    };

    runtime
        .create_worktree(
            clone.path().to_str().expect("utf-8 clone path"),
            "feature/prune",
            Some("main"),
            target.to_str().expect("utf-8 target path"),
        )
        .await
        .expect("worktree should create");
    runtime
        .create_worktree(
            clone.path().to_str().expect("utf-8 clone path"),
            "feature/stale",
            Some("main"),
            stale_target.to_str().expect("utf-8 stale target path"),
        )
        .await
        .expect("stale worktree should create");
    // This fixture represents a released registration eligible for pruning.
    // Issue #2675 requires still-managed siblings to remain locked instead.
    let released = ProcessCommand::new("git")
        .args(["-C", clone.path().to_str().expect("clone path"), "worktree", "unlock", stale_target.to_str().expect("stale path")])
        .status()
        .expect("release stale fixture registration");
    assert!(released.success());
    fs::remove_dir_all(&stale_target).expect("simulate checkout directory disappearing without git cleanup");
    let stale_worktrees = ProcessCommand::new("git")
        .args(["-C", clone.path().to_str().expect("utf-8 clone path"), "worktree", "list", "--porcelain"])
        .output()
        .expect("git should list stale worktree metadata");
    assert!(String::from_utf8(stale_worktrees.stdout)
        .expect("utf-8 stale worktree list")
        .contains(stale_target.to_str().expect("utf-8 stale target path")));
    let removal = CheckoutRemoval::Worktree {
        clone_path: clone.path().to_str().expect("utf-8 clone path").to_string(),
        branch: "feature/prune".to_string(),
        target_path: target.to_str().expect("utf-8 target path").to_string(),
    };

    runtime.remove_checkout(&removal).await.expect("worktree cleanup should prune its base clone");

    let worktrees = ProcessCommand::new("git")
        .args(["-C", clone.path().to_str().expect("utf-8 clone path"), "worktree", "list", "--porcelain"])
        .output()
        .expect("git should list worktrees");
    assert!(worktrees.status.success());
    let worktrees = String::from_utf8(worktrees.stdout).expect("utf-8 worktree list");
    assert!(!worktrees.contains(target.to_str().expect("utf-8 target path")));
    assert!(!worktrees.contains(stale_target.to_str().expect("utf-8 stale target path")), "base-clone prune must remove stale metadata");
    assert!(!target.exists(), "checkout resource cleanup must not retain its directory");
    assert!(commands
        .lock()
        .expect("command lock")
        .iter()
        .any(|command| { command == &["git", "-C", clone.path().to_str().expect("utf-8 clone path"), "worktree", "prune"] }));
}

// Known checkout roots must never inherit branch or status from an ancestor.
// Cover absent roots, standalone repositories and linked-worktree gitfiles,
// nested at several depths beneath both clean and dirty enclosing checkouts.
#[hegel::test]
fn checkout_root_inspection_does_not_inherit_enclosing_repository(tc: hegel::TestCase) {
    use flotilla_core::{
        providers::vcs::{CloneProvisioner, GitCloneProvisioner},
        vcs::{GitCliBackend, VcsBackend},
    };
    use hegel::generators as gs;

    let depth = tc.draw(gs::integers::<usize>().min_value(1).max_value(3));
    let kind = tc.draw(gs::integers::<usize>().min_value(0).max_value(2));
    let dirty_parent = tc.draw(gs::booleans());
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    runtime.block_on(async {
        let temp = TempDir::new().expect("tempdir");
        let parent = TestGitRepo::init(temp.path()).with_initial_commit();
        // Ignore fixture children so the clean-ancestor cases are actually clean.
        fs::write(parent.path().join(".git/info/exclude"), "nested/\n").expect("ignore nested fixtures");
        let mut target = parent.path().to_path_buf();
        for _ in 0..depth {
            target.push("nested");
        }
        fs::create_dir_all(&target).expect("create target");
        match kind {
            0 => {}
            1 => {
                TestGitRepo::init(&target).with_initial_commit();
            }
            2 => {
                assert!(ProcessCommand::new("git")
                    .arg("-C")
                    .arg(parent.path())
                    .args(["worktree", "add", "-b", "inner"])
                    .arg(&target)
                    .output()
                    .expect("create linked worktree")
                    .status
                    .success());
            }
            _ => unreachable!(),
        }
        if dirty_parent {
            fs::write(parent.path().join("README.md"), "dirty ancestor\n").expect("dirty parent");
        }
        let runner = ProcessCommandRunner;
        let vcs = GitCliBackend::checkout_root(&target, &runner);
        let branch = vcs.current_branch().await;
        let status = vcs.working_tree_status(false).await.expect("status process");
        let inspection = GitCloneProvisioner::new(Arc::new(ProcessCommandRunner))
            .inspect_clone(&ExecutionEnvironmentPath::new(&target))
            .await
            .expect("inspect clone");
        if kind == 0 {
            assert!(branch.is_err(), "a directory without its own Git entry is not a checkout");
            assert!(!status.success(), "status must not inspect the enclosing checkout");
            assert!(inspection.default_branch.is_none(), "clone inspection must not borrow the ancestor branch");
        } else {
            let expected = if kind == 1 { "main" } else { "inner" };
            assert_eq!(branch.expect("checkout branch").trim(), expected);
            assert!(status.success() && status.stdout.is_empty(), "the nested checkout itself is clean");
            assert_eq!(inspection.default_branch.as_deref(), Some(expected));
        }
    });
}

#[tokio::test]
async fn checkout_runtime_removes_unregistered_worktree_path_with_embedded_repository() {
    // A stale checkout has no Git entry of its own. Cleanup must not
    // inspect the enclosing repository, even when it contains another repo.
    let temp = TempDir::new().expect("tempdir");
    TestGitRepo::init(temp.path()).with_initial_commit();
    let clone = TestGitRepo::init(temp.path().join("clone")).with_initial_commit();
    let target = temp.path().join("checkout-root/convoy-a/flotilla.feature-cleanup");
    TestGitRepo::init(target.join("embedded")).with_initial_commit();
    let runtime =
        CheckoutControllerRuntime { vcs: None, runner: Arc::new(ProcessCommandRunner), change_requests: None, forges: Vec::new() };
    let removal = CheckoutRemoval::Worktree {
        clone_path: clone.path().to_str().expect("utf-8 clone path").to_string(),
        branch: "feature/cleanup".to_string(),
        target_path: target.to_str().expect("utf-8 target path").to_string(),
    };

    assert_eq!(runtime.remove_checkout(&removal).await.expect("stale worktree path should be removed"), CheckoutRemovalOutcome::Removed);

    assert!(!target.exists(), "successful cleanup must not strand an embedded repository");
}

#[tokio::test]
async fn checkout_runtime_preserves_and_reports_branch_with_commits() {
    let temp = TempDir::new().expect("tempdir");
    let clone = TestGitRepo::init(temp.path().join("clone")).with_initial_commit();
    let convoy_dir = temp.path().join("checkout-root/convoy-a");
    let target = convoy_dir.join("feature-work/flotilla");
    let runtime =
        CheckoutControllerRuntime { vcs: None, runner: Arc::new(ProcessCommandRunner), change_requests: None, forges: Vec::new() };

    let prepared = runtime
        .create_worktree(
            clone.path().to_str().expect("utf-8 clone path"),
            "feature/work",
            Some("main"),
            target.to_str().expect("utf-8 target path"),
        )
        .await
        .expect("worktree should create");
    assert_eq!(prepared.branch_provenance, CheckoutBranchProvenance::CreatedForConvoy);
    assert!(prepared.commit.is_some(), "worktree should resolve its initial commit");
    fs::write(target.join("work.txt"), "real work\n").expect("work file should be written");
    assert!(ProcessCommand::new("git")
        .args(["-C", target.to_str().expect("utf-8 target path"), "add", "work.txt"])
        .status()
        .expect("git add should run")
        .success());
    assert!(ProcessCommand::new("git")
        .args(["-C", target.to_str().expect("utf-8 target path"), "commit", "-m", "real work"])
        .status()
        .expect("git commit should run")
        .success());

    let removal = CheckoutRemoval::Worktree {
        clone_path: clone.path().to_str().expect("utf-8 clone path").to_string(),
        branch: "feature/work".to_string(),
        target_path: target.to_str().expect("utf-8 target path").to_string(),
    };
    assert_eq!(
        runtime.remove_checkout(&removal).await.expect("worktree should be removed"),
        CheckoutRemovalOutcome::PreservedBranch { branch: "feature/work".to_string(), reason: BranchPreservationReason::CommitsPastBase }
    );
    assert!(!convoy_dir.exists(), "empty convoy directory should be removed");

    let branch = ProcessCommand::new("git")
        .args(["-C", clone.path().to_str().expect("utf-8 clone path"), "show-ref", "--verify", "--quiet", "refs/heads/feature/work"])
        .status()
        .expect("git should inspect the branch");
    assert!(branch.success(), "convoy branch with commits should be preserved");
    let marker = ProcessCommand::new("git")
        .args([
            "-C",
            clone.path().to_str().expect("utf-8 clone path"),
            "show-ref",
            "--verify",
            "--quiet",
            &bootstrap_branch_ref("feature/work"),
        ])
        .status()
        .expect("git should inspect the ownership marker");
    assert!(!marker.success(), "ownership marker should be removed after preserving committed work");
}

#[tokio::test]
async fn landed_checkout_on_deleted_squash_branch_does_not_hold_teardown() {
    let temp = TempDir::new().expect("tempdir");
    let clone = TestGitRepo::init(temp.path().join("clone")).with_initial_commit();
    let remote = temp.path().join("remote.git");
    assert!(ProcessCommand::new("git").args(["init", "--bare"]).arg(&remote).status().expect("create bare remote").success());
    assert!(ProcessCommand::new("git")
        .arg("-C")
        .arg(clone.path())
        .args(["remote", "add", "origin"])
        .arg(&remote)
        .status()
        .expect("add origin")
        .success());
    let target = temp.path().join("checkout-root/convoy-a/work");
    let runtime =
        CheckoutControllerRuntime { vcs: None, runner: Arc::new(ProcessCommandRunner), change_requests: None, forges: Vec::new() };
    runtime
        .create_worktree(
            clone.path().to_str().expect("clone path"),
            "feature/original",
            Some("main"),
            target.to_str().expect("target path"),
        )
        .await
        .expect("create worktree");
    let switched = ProcessCommand::new("git")
        .args(["-C", target.to_str().expect("target path"), "switch", "-c", "feature/merged"])
        .status()
        .expect("switch branch");
    assert!(switched.success());
    fs::write(target.join("result.txt"), "merged work\n").expect("write work");
    for args in [["add", "result.txt"].as_slice(), ["commit", "-m", "merged work"].as_slice()] {
        assert!(ProcessCommand::new("git").arg("-C").arg(&target).args(args).status().expect("git operation").success());
    }
    for args in [["push", "origin", "feature/merged"].as_slice(), ["push", "origin", ":feature/merged"].as_slice()] {
        assert!(ProcessCommand::new("git").arg("-C").arg(&target).args(args).status().expect("remote branch lifecycle").success());
    }

    let removal = CheckoutRemoval::LandedWorktree {
        clone_path: clone.path().to_str().expect("clone path").to_string(),
        branch: "feature/original".to_string(),
        target_path: target.to_str().expect("target path").to_string(),
    };
    fs::write(target.join("uncommitted.txt"), "keep this work\n").expect("write local work");
    let refusal = runtime.remove_checkout(&removal).await.expect_err("dirty landed checkout must be preserved");
    assert!(refusal.contains("DirtyCheckout"));
    assert!(target.join("uncommitted.txt").exists());
    fs::remove_file(target.join("uncommitted.txt")).expect("clear local work");
    assert!(matches!(runtime.remove_checkout(&removal).await.expect("landed removal"), CheckoutRemovalOutcome::ArchivedAndRemoved { .. }));
    assert!(!target.exists());
    assert!(!target.parent().expect("checkout parent").exists(), "landed archived checkout must remove its empty convoy parent");
    let archive_parent = temp.path().join(".flotilla-archives");
    assert!(fs::read_dir(archive_parent)
        .expect("archive directory")
        .any(|entry| { entry.expect("entry").file_name().to_string_lossy().starts_with("work-") }));
}

#[tokio::test]
async fn checkout_runtime_removes_a_shared_bootstrap_branch_in_either_teardown_order() {
    let temp = TempDir::new().expect("tempdir");
    let clone = TestGitRepo::init(temp.path().join("clone")).with_initial_commit();
    let runtime =
        CheckoutControllerRuntime { vcs: None, runner: Arc::new(ProcessCommandRunner), change_requests: None, forges: Vec::new() };

    for reverse_teardown in [false, true] {
        let case = if reverse_teardown { "reverse" } else { "forward" };
        let branch = format!("feature/shared-{case}");
        let workspace = temp.path().join(format!("workspace-{case}"));
        let targets = [workspace.join("first"), workspace.join("second")];
        let prepared = runtime
            .create_worktree(clone.path().to_str().expect("clone"), &branch, Some("main"), targets[0].to_str().expect("target"))
            .await
            .expect("first worktree");
        assert_eq!(prepared.branch_provenance, CheckoutBranchProvenance::CreatedForConvoy);
        // Older stored generations may already share a branch. Construct
        // that predecessor state directly, then keep its teardown contract.
        assert!(ProcessCommand::new("git")
            .args([
                "-C",
                clone.path().to_str().expect("clone"),
                "worktree",
                "add",
                "--force",
                targets[1].to_str().expect("target"),
                &branch
            ])
            .status()
            .expect("old sibling worktree")
            .success());

        let removals = targets.each_ref().map(|target| CheckoutRemoval::Worktree {
            clone_path: clone.path().to_str().expect("utf-8 clone path").to_string(),
            branch: branch.clone(),
            target_path: target.to_str().expect("utf-8 target path").to_string(),
        });
        let order = if reverse_teardown { [1, 0] } else { [0, 1] };
        assert_eq!(
            runtime.remove_checkout(&removals[order[0]]).await.expect("first worktree should be removed"),
            CheckoutRemovalOutcome::PreservedBranch { branch: branch.clone(), reason: BranchPreservationReason::CheckedOutElsewhere }
        );
        assert_eq!(
            runtime.remove_checkout(&removals[order[1]]).await.expect("last worktree should be removed"),
            CheckoutRemovalOutcome::Removed
        );
        assert!(!workspace.exists(), "empty workspace directory should be removed");

        for reference in [format!("refs/heads/{branch}"), bootstrap_branch_ref(&branch)] {
            let reference = ProcessCommand::new("git")
                .args(["-C", clone.path().to_str().expect("utf-8 clone path"), "show-ref", "--verify", "--quiet", &reference])
                .status()
                .expect("git should inspect the reference");
            assert!(!reference.success(), "zero-commit branch and ownership marker should be deleted");
        }
    }
}

#[tokio::test]
async fn checkout_runtime_refuses_existing_local_branch_and_recovers_existing_target() {
    let temp = TempDir::new().expect("tempdir");
    let missing_origin = temp.path().join("missing-origin.git");
    let clone =
        TestGitRepo::init(temp.path().join("clone")).with_initial_commit().with_origin(missing_origin.to_str().expect("utf-8 origin path"));
    let target = temp.path().join("workspace/flotilla");
    let runtime =
        CheckoutControllerRuntime { vcs: None, runner: Arc::new(ProcessCommandRunner), change_requests: None, forges: Vec::new() };

    // #2698: refuse new work on an existing branch without contacting its unavailable origin.
    let error = runtime
        .create_worktree(clone.path().to_str().expect("clone"), "main", Some("main"), target.to_str().expect("target"))
        .await
        .expect_err("branch conflict");
    assert!(error.contains("main") && error.contains("already exists"), "{error}");
    assert!(!target.exists());
    // Existing targets are reconciliation retries, so recovery and cleanup
    // still preserve a branch that the convoy did not create.
    assert!(ProcessCommand::new("git")
        .args(["-C", clone.path().to_str().expect("clone"), "worktree", "add", "--force", target.to_str().expect("target"), "main"])
        .status()
        .expect("worktree")
        .success());
    let prepared = runtime
        .create_worktree(
            clone.path().to_str().expect("utf-8 clone path"),
            "main",
            Some("main"),
            target.to_str().expect("utf-8 target path"),
        )
        .await
        .expect("local branch should not require its origin");
    assert_eq!(prepared.branch_provenance, CheckoutBranchProvenance::PreExisting);

    let branch = ProcessCommand::new("git")
        .args(["-C", target.to_str().expect("utf-8 target path"), "branch", "--show-current"])
        .output()
        .expect("git should run");
    assert!(branch.status.success());
    assert_eq!(String::from_utf8(branch.stdout).expect("utf-8 branch").trim(), "main");

    let removal = CheckoutRemoval::Worktree {
        clone_path: clone.path().to_str().expect("utf-8 clone path").to_string(),
        branch: "main".to_string(),
        target_path: target.to_str().expect("utf-8 target path").to_string(),
    };
    assert_eq!(
        runtime.remove_checkout(&removal).await.expect("worktree should be removed"),
        CheckoutRemovalOutcome::PreservedBranch { branch: "main".to_string(), reason: BranchPreservationReason::NotCreatedForConvoy }
    );
    let branch = ProcessCommand::new("git")
        .args(["-C", clone.path().to_str().expect("utf-8 clone path"), "show-ref", "--verify", "--quiet", "refs/heads/main"])
        .status()
        .expect("git should inspect the branch");
    assert!(branch.success(), "pre-existing local branch should be preserved");
}

#[tokio::test]
async fn checkout_runtime_resolves_a_remote_only_snapshotted_base() {
    let temp = TempDir::new().expect("tempdir");
    let source = TestGitRepo::init(temp.path().join("source")).with_initial_commit();
    let source_path = source.path().to_str().expect("utf-8 source path");
    assert!(ProcessCommand::new("git")
        .args(["-C", source_path, "switch", "-c", "stable"])
        .status()
        .expect("git switch should run")
        .success());
    fs::write(source.path().join("stable.txt"), "stable base\n").expect("write stable file");
    assert!(ProcessCommand::new("git").args(["-C", source_path, "add", "stable.txt"]).status().expect("git add should run").success());
    assert!(ProcessCommand::new("git")
        .args(["-C", source_path, "commit", "-m", "stable commit"])
        .status()
        .expect("git commit should run")
        .success());
    assert!(ProcessCommand::new("git").args(["-C", source_path, "switch", "main"]).status().expect("git switch should run").success());
    let clone_path = temp.path().join("clone");
    assert!(ProcessCommand::new("git")
        .args(["clone", "--branch", "main", source_path, clone_path.to_str().expect("utf-8 clone path")])
        .status()
        .expect("git clone should run")
        .success());
    let target = temp.path().join("workspace/flotilla");
    let runtime =
        CheckoutControllerRuntime { vcs: None, runner: Arc::new(ProcessCommandRunner), change_requests: None, forges: Vec::new() };

    runtime
        .create_worktree(
            clone_path.to_str().expect("utf-8 clone path"),
            "feature/remote-base",
            Some("stable"),
            target.to_str().expect("utf-8 target path"),
        )
        .await
        .expect("worktree should create");

    assert_eq!(fs::read_to_string(target.join("stable.txt")).expect("stable file should exist"), "stable base\n");
    let branch = ProcessCommand::new("git")
        .args(["-C", target.to_str().expect("utf-8 target path"), "branch", "--show-current"])
        .output()
        .expect("git should run");
    assert_eq!(String::from_utf8(branch.stdout).expect("utf-8 branch").trim(), "feature/remote-base");
}

#[tokio::test]
async fn checkout_runtime_refuses_an_existing_remote_convoy_branch() {
    let temp = TempDir::new().expect("tempdir");
    let source = TestGitRepo::init(temp.path().join("source")).with_initial_commit();
    let source_path = source.path().to_str().expect("utf-8 source path");
    assert!(ProcessCommand::new("git")
        .args(["-C", source_path, "switch", "-c", "feature/existing"])
        .status()
        .expect("git switch should run")
        .success());
    fs::write(source.path().join("feature.txt"), "existing branch\n").expect("write feature file");
    assert!(ProcessCommand::new("git").args(["-C", source_path, "add", "feature.txt"]).status().expect("git add should run").success());
    assert!(ProcessCommand::new("git")
        .args(["-C", source_path, "commit", "-m", "feature commit"])
        .status()
        .expect("git commit should run")
        .success());
    assert!(ProcessCommand::new("git").args(["-C", source_path, "switch", "main"]).status().expect("git switch should run").success());
    let clone_path = temp.path().join("clone");
    assert!(ProcessCommand::new("git")
        .args(["clone", "--branch", "main", source_path, clone_path.to_str().expect("utf-8 clone path")])
        .status()
        .expect("git clone should run")
        .success());
    let target = temp.path().join("workspace/flotilla");
    let runtime =
        CheckoutControllerRuntime { vcs: None, runner: Arc::new(ProcessCommandRunner), change_requests: None, forges: Vec::new() };

    // #2698: even a branch visible only on the remote is a name conflict.
    let error = runtime
        .create_worktree(clone_path.to_str().expect("clone"), "feature/existing", Some("main"), target.to_str().expect("target"))
        .await
        .expect_err("branch conflict");
    assert!(error.contains("feature/existing"), "{error}");
    assert!(!target.exists(), "refusal must leave no worktree");
}

#[tokio::test]
async fn checkout_runtime_refuses_a_remote_branch_created_after_the_clone() {
    let temp = TempDir::new().expect("tempdir");
    let source = TestGitRepo::init(temp.path().join("source")).with_initial_commit();
    let source_path = source.path().to_str().expect("utf-8 source path");
    let clone_path = temp.path().join("clone");
    assert!(ProcessCommand::new("git")
        .args(["clone", "--branch", "main", source_path, clone_path.to_str().expect("utf-8 clone path")])
        .status()
        .expect("git clone should run")
        .success());

    assert!(ProcessCommand::new("git")
        .args(["-C", source_path, "switch", "-c", "feature/created-later"])
        .status()
        .expect("git switch should run")
        .success());
    fs::write(source.path().join("created-later.txt"), "remote branch\n").expect("write feature file");
    assert!(ProcessCommand::new("git")
        .args(["-C", source_path, "add", "created-later.txt"])
        .status()
        .expect("git add should run")
        .success());
    assert!(ProcessCommand::new("git")
        .args(["-C", source_path, "commit", "-m", "later branch commit"])
        .status()
        .expect("git commit should run")
        .success());

    let target = temp.path().join("workspace/flotilla");
    let runtime =
        CheckoutControllerRuntime { vcs: None, runner: Arc::new(ProcessCommandRunner), change_requests: None, forges: Vec::new() };
    // #2698: even a branch visible only on the remote is a name conflict.
    let error = runtime
        .create_worktree(clone_path.to_str().expect("clone"), "feature/created-later", Some("main"), target.to_str().expect("target"))
        .await
        .expect_err("branch conflict");
    assert!(error.contains("feature/created-later"), "{error}");
    assert!(!target.exists(), "refusal must leave no worktree");
}

#[tokio::test]
async fn fresh_clone_checkout_creates_convoy_branch_from_snapshotted_base() {
    let temp = TempDir::new().expect("tempdir");
    let source = TestGitRepo::init(temp.path().join("source")).with_initial_commit();
    let target = temp.path().join("fresh-clone");
    let runtime =
        CheckoutControllerRuntime { vcs: None, runner: Arc::new(ProcessCommandRunner), change_requests: None, forges: Vec::new() };

    runtime
        .create_fresh_clone(
            source.path().to_str().expect("utf-8 source path"),
            "feature/multi-repo",
            Some("main"),
            target.to_str().expect("utf-8 target path"),
        )
        .await
        .expect("fresh clone should create");

    let branch = ProcessCommand::new("git")
        .args(["-C", target.to_str().expect("utf-8 target path"), "branch", "--show-current"])
        .output()
        .expect("git should run");
    assert!(branch.status.success());
    assert_eq!(String::from_utf8(branch.stdout).expect("utf-8 branch").trim(), "feature/multi-repo");
}

#[tokio::test]
async fn checkout_runtime_recovers_a_fresh_clone_after_its_completion_is_lost() {
    let temp = TempDir::new().expect("tempdir");
    let source = TestGitRepo::init(temp.path().join("source")).with_initial_commit();
    let target = temp.path().join("fresh-clone");
    let runtime =
        CheckoutControllerRuntime { vcs: None, runner: Arc::new(ProcessCommandRunner), change_requests: None, forges: Vec::new() };

    let first = runtime
        .create_fresh_clone(
            source.path().to_str().expect("utf-8 source path"),
            "feature/redrive",
            Some("main"),
            target.to_str().expect("utf-8 target path"),
        )
        .await
        .expect("first clone actuation should succeed");
    let recovered = runtime
        .create_fresh_clone(
            source.path().to_str().expect("utf-8 source path"),
            "feature/redrive",
            Some("main"),
            target.to_str().expect("utf-8 target path"),
        )
        .await
        .expect("redrive should recover the clone already created on disk");

    assert_eq!(recovered, first);
}

#[tokio::test]
async fn checkout_runtime_retries_after_an_interrupted_fresh_clone() {
    let temp = TempDir::new().expect("tempdir");
    let source = TestGitRepo::init(temp.path().join("source")).with_initial_commit();
    let target = temp.path().join("fresh-clone");
    let runtime = CheckoutControllerRuntime {
        vcs: None,
        runner: Arc::new(FailFirstCloneProcessRunner { failed: AtomicBool::new(false) }),
        change_requests: None,
        forges: Vec::new(),
    };

    runtime
        .create_fresh_clone(
            source.path().to_str().expect("utf-8 source path"),
            "feature/redrive",
            Some("main"),
            target.to_str().expect("utf-8 target path"),
        )
        .await
        .expect_err("interrupted clone should fail its first actuation");
    assert!(!Path::new(&clone_staging_path(target.to_str().expect("utf-8 target path"))).exists());

    runtime
        .create_fresh_clone(
            source.path().to_str().expect("utf-8 source path"),
            "feature/redrive",
            Some("main"),
            target.to_str().expect("utf-8 target path"),
        )
        .await
        .expect("redrive should replace the interrupted clone");

    let branch = ProcessCommand::new("git")
        .args(["-C", target.to_str().expect("utf-8 target path"), "branch", "--show-current"])
        .output()
        .expect("git should inspect the retried clone");
    assert!(branch.status.success());
    assert_eq!(String::from_utf8(branch.stdout).expect("utf-8 branch").trim(), "feature/redrive");
}

#[tokio::test]
async fn checkout_runtime_removes_an_interrupted_fresh_clone() {
    let temp = TempDir::new().expect("tempdir");
    let target = temp.path().join("fresh-clone");
    let target = target.to_str().expect("utf-8 target path");
    let staging_path = clone_staging_path(target);
    fs::create_dir_all(&staging_path).expect("create partial clone directory");
    fs::write(Path::new(&staging_path).join("partial"), "incomplete clone").expect("write partial clone content");
    let runtime =
        CheckoutControllerRuntime { vcs: None, runner: Arc::new(ProcessCommandRunner), change_requests: None, forges: Vec::new() };

    let outcome = runtime
        .remove_checkout(&CheckoutRemoval::FreshClone { target_path: target.to_string() })
        .await
        .expect("fresh clone removal should clean staging");

    assert_eq!(outcome, CheckoutRemovalOutcome::Removed);
    assert!(!Path::new(&staging_path).exists());
}

// Issue #2675: losing the Clone resource must not strand a locked
// registration when its remaining checkout is finalized.
#[tokio::test]
async fn orphaned_worktree_teardown_releases_registration_before_deleting_path() {
    let temp = TempDir::new().expect("tempdir");
    let source = TestGitRepo::init(temp.path().join("source")).with_initial_commit();
    let target = temp.path().join("orphan");
    let runtime =
        CheckoutControllerRuntime { vcs: None, runner: Arc::new(ProcessCommandRunner), change_requests: None, forges: Vec::new() };
    runtime
        .create_worktree(source.path().to_str().expect("source"), "convoy/work", Some("main"), target.to_str().expect("target"))
        .await
        .expect("create managed worktree");
    let admin = source.path().join(".git/worktrees/orphan");
    assert!(admin.join("locked").exists());
    let outcome = runtime
        .remove_checkout(&CheckoutRemoval::OrphanedWorktree { target_path: target.to_str().expect("target").into() })
        .await
        .expect("remove orphan");
    assert_eq!(outcome, CheckoutRemovalOutcome::Removed);
    assert!(!target.exists());
    assert!(!admin.join("locked").exists(), "orphan teardown releases protection before deleting its path");
}

#[tokio::test]
async fn checkout_runtime_treats_an_already_missing_orphaned_worktree_as_removed() {
    let temp = TempDir::new().expect("tempdir");
    let target = temp.path().join("already-removed-worktree");
    let runtime =
        CheckoutControllerRuntime { vcs: None, runner: Arc::new(ProcessCommandRunner), change_requests: None, forges: Vec::new() };

    let outcome = runtime
        .remove_checkout(&CheckoutRemoval::OrphanedWorktree { target_path: target.to_str().expect("utf-8 target path").to_string() })
        .await
        .expect("an absent checkout directory should not wedge finalization");

    assert_eq!(outcome, CheckoutRemovalOutcome::Removed);
}

#[tokio::test]
async fn fresh_clone_checkout_treats_head_as_the_remote_default() {
    let temp = TempDir::new().expect("tempdir");
    let source = TestGitRepo::init(temp.path().join("source")).with_initial_commit();
    let target = temp.path().join("fresh-clone");
    let runtime =
        CheckoutControllerRuntime { vcs: None, runner: Arc::new(ProcessCommandRunner), change_requests: None, forges: Vec::new() };

    runtime
        .create_fresh_clone(
            source.path().to_str().expect("utf-8 source path"),
            "feature/from-head",
            Some("HEAD"),
            target.to_str().expect("utf-8 target path"),
        )
        .await
        .expect("fresh clone should create");

    let branch = ProcessCommand::new("git")
        .args(["-C", target.to_str().expect("utf-8 target path"), "branch", "--show-current"])
        .output()
        .expect("git should run");
    assert_eq!(String::from_utf8(branch.stdout).expect("utf-8 branch").trim(), "feature/from-head");
}

#[tokio::test]
async fn fresh_clone_checkout_refuses_an_existing_convoy_branch() {
    let temp = TempDir::new().expect("tempdir");
    let source = TestGitRepo::init(temp.path().join("source")).with_initial_commit();
    let source_path = source.path().to_str().expect("utf-8 source path");
    assert!(ProcessCommand::new("git")
        .args(["-C", source_path, "switch", "-c", "feature/existing"])
        .status()
        .expect("git switch should run")
        .success());
    fs::write(source.path().join("feature.txt"), "existing branch\n").expect("write feature file");
    assert!(ProcessCommand::new("git").args(["-C", source_path, "add", "feature.txt"]).status().expect("git add should run").success());
    assert!(ProcessCommand::new("git")
        .args(["-C", source_path, "commit", "-m", "feature commit"])
        .status()
        .expect("git commit should run")
        .success());
    let target = temp.path().join("fresh-clone");
    let runtime =
        CheckoutControllerRuntime { vcs: None, runner: Arc::new(ProcessCommandRunner), change_requests: None, forges: Vec::new() };

    // #2698: a new convoy branch must not silently adopt an existing remote tip.
    let error = runtime
        .create_fresh_clone(source_path, "feature/existing", Some("main"), target.to_str().expect("utf-8 target path"))
        .await
        .expect_err("existing branch must be refused");
    assert!(error.contains("feature/existing"), "{error}");
    assert!(!target.exists(), "refusal must leave no clone");
}

fn empty_meta(name: &str) -> flotilla_resources::InputMeta {
    flotilla_resources::InputMeta::builder().name(name.to_string()).build()
}
