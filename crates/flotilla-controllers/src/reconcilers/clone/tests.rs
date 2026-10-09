use super::*;
use crate::{
    reconcilers::runtime_tests::{BlockingCloneProcessRunner, FailFirstCloneProcessRunner},
    test_git_repo::TestGitRepo,
};
use flotilla_core::providers::ProcessCommandRunner;
use std::{
    collections::BTreeSet,
    fs,
    path::Path,
    process::Command as ProcessCommand,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tempfile::TempDir;
use tokio::sync::Notify;

#[tokio::test]
async fn clone_runtime_single_flights_concurrent_requests_for_the_same_target() {
    let temp = TempDir::new().expect("tempdir");
    let source = TestGitRepo::init(temp.path().join("source")).with_initial_commit();
    let target = temp.path().join("clone");
    let runner = Arc::new(BlockingCloneProcessRunner {
        clone_attempts: AtomicUsize::new(0),
        clone_started: Notify::new(),
        release_clone: Notify::new(),
    });
    let flights = Arc::new(CloneFlights::default());
    let first_runtime =
        Arc::new(CloneControllerRuntime { vcs: None, runner: runner.clone(), flights: Arc::clone(&flights), forges: vec![] });
    let second_runtime = Arc::new(CloneControllerRuntime { vcs: None, runner: runner.clone(), flights, forges: vec![] });
    let repo_url = source.path().to_str().expect("utf-8 source path").to_string();
    let target_path = target.to_str().expect("utf-8 target path").to_string();

    let first = tokio::spawn({
        let runtime = Arc::clone(&first_runtime);
        let repo_url = repo_url.clone();
        let target_path = target_path.clone();
        async move { runtime.clone_and_inspect(&repo_url, &target_path).await }
    });
    runner.clone_started.notified().await;
    let second = tokio::spawn({
        let runtime = Arc::clone(&second_runtime);
        async move { runtime.clone_and_inspect(&repo_url, &target_path).await }
    });
    tokio::time::sleep(Duration::from_millis(25)).await;
    runner.release_clone.notify_one();

    let first = first.await.expect("first clone task should join");
    let second = second.await.expect("second clone task should join");

    assert_eq!(first.expect("first clone should succeed").as_deref(), Some("main"));
    assert_eq!(second.expect("second clone should succeed").as_deref(), Some("main"));
    assert_eq!(runner.clone_attempts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn clone_runtime_adopts_a_matching_clone_that_wins_an_external_flight() {
    let temp = TempDir::new().expect("tempdir");
    let source = TestGitRepo::init(temp.path().join("source")).with_initial_commit();
    let target = temp.path().join("clone");
    let runner = Arc::new(BlockingCloneProcessRunner {
        clone_attempts: AtomicUsize::new(0),
        clone_started: Notify::new(),
        release_clone: Notify::new(),
    });
    let first_runtime =
        Arc::new(CloneControllerRuntime { vcs: None, runner: runner.clone(), flights: Arc::new(CloneFlights::default()), forges: vec![] });
    let external_runtime =
        Arc::new(CloneControllerRuntime { vcs: None, runner: runner.clone(), flights: Arc::new(CloneFlights::default()), forges: vec![] });
    let repo_url = source.path().to_str().expect("utf-8 source path").to_string();
    let target_path = target.to_str().expect("utf-8 target path").to_string();

    let first = tokio::spawn({
        let runtime = Arc::clone(&first_runtime);
        let repo_url = repo_url.clone();
        let target_path = target_path.clone();
        async move { runtime.clone_and_inspect(&repo_url, &target_path).await }
    });
    runner.clone_started.notified().await;
    let external = tokio::spawn({
        let runtime = Arc::clone(&external_runtime);
        async move { runtime.clone_and_inspect(&repo_url, &target_path).await }
    });
    let external = external.await.expect("external clone task should join");
    runner.release_clone.notify_one();
    let first = first.await.expect("first clone task should join");

    assert_eq!(external.expect("external clone should succeed").as_deref(), Some("main"));
    assert_eq!(first.expect("losing flight should adopt the external clone").as_deref(), Some("main"));
    assert_eq!(runner.clone_attempts.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn clone_runtime_retries_after_an_interrupted_clone() {
    let temp = TempDir::new().expect("tempdir");
    let source = TestGitRepo::init(temp.path().join("source")).with_initial_commit();
    let target = temp.path().join("clone");
    let runtime = CloneControllerRuntime {
        vcs: None,
        runner: Arc::new(FailFirstCloneProcessRunner { failed: AtomicBool::new(false) }),
        flights: Arc::new(CloneFlights::default()),
        forges: vec![],
    };
    let repo_url = source.path().to_str().expect("utf-8 source path");
    let target_path = target.to_str().expect("utf-8 target path");

    runtime.clone_and_inspect(repo_url, target_path).await.expect_err("interrupted clone should fail its first actuation");
    assert!(!target.exists(), "failed clone debris should be removed");

    let default_branch = runtime.clone_and_inspect(repo_url, target_path).await.expect("redrive should replace the interrupted clone");

    assert_eq!(default_branch.as_deref(), Some("main"));
}

#[tokio::test]
async fn clone_runtime_adopts_existing_forge_clone_across_host_aliases() {
    let temp = TempDir::new().expect("tempdir");
    let source = TestGitRepo::init(temp.path().join("source")).with_initial_commit();
    let target = temp.path().join("clone");
    let source_path = source.path().to_str().expect("source path");
    assert!(ProcessCommand::new("git")
        .args(["clone", source_path, target.to_str().expect("target path")])
        .status()
        .expect("clone fixture")
        .success());
    assert!(ProcessCommand::new("git")
        .arg("-C")
        .arg(&target)
        .args(["remote", "set-url", "origin", "https://forgejo.lab.flotilla.work/robert/ghostty-ops"])
        .status()
        .expect("set clone origin")
        .success());
    let runtime = CloneControllerRuntime {
        vcs: None,
        runner: Arc::new(ProcessCommandRunner),
        flights: Arc::new(CloneFlights::default()),
        forges: vec![flotilla_resources::ForgeSpec::builder()
            .forge_id("lab".to_string())
            .kind(flotilla_resources::ForgeKind::Forgejo)
            .hosts(BTreeSet::from(["forgejo-manchego".to_string(), "forgejo.lab.flotilla.work".to_string()]))
            .https_url("https://forgejo.lab.flotilla.work".to_string())
            .git_ssh_host("forgejo.lab.flotilla.work".to_string())
            .build()],
    };

    let branch = runtime
        .clone_and_inspect("https://forgejo-manchego/robert/ghostty-ops", target.to_str().expect("target path"))
        .await
        .expect("existing clone should be adopted by Forge identity");
    assert_eq!(branch.as_deref(), Some("main"));

    assert!(ProcessCommand::new("git")
        .arg("-C")
        .arg(&target)
        .args(["remote", "set-url", "origin", "forgejo-manchego:robert/ghostty-ops.git"])
        .status()
        .expect("set alias origin")
        .success());
    let branch = runtime
        .clone_and_inspect("https://forgejo.lab.flotilla.work/robert/ghostty-ops", target.to_str().expect("target path"))
        .await
        .expect("alias-origin clone should be adopted by Forge identity");
    assert_eq!(branch.as_deref(), Some("main"));
}

#[tokio::test]
async fn clone_runtime_rejects_a_dirty_target_then_recovers_after_it_is_removed() {
    let temp = TempDir::new().expect("tempdir");
    let source = TestGitRepo::init(temp.path().join("source")).with_initial_commit();
    let target = temp.path().join("clone");
    fs::create_dir_all(&target).expect("dirty target directory");
    fs::write(target.join("leftover"), "debris").expect("dirty target contents");
    let runtime = CloneControllerRuntime {
        vcs: None,
        runner: Arc::new(ProcessCommandRunner),
        flights: Arc::new(CloneFlights::default()),
        forges: vec![],
    };
    let repo_url = source.path().to_str().expect("utf-8 source path");
    let target_path = target.to_str().expect("utf-8 target path");

    let error = runtime.clone_and_inspect(repo_url, target_path).await.expect_err("dirty target should not be overwritten");

    assert!(error.contains("clone target") && error.contains("already exists"), "unexpected dirty-target error: {error}");
    assert!(target.join("leftover").exists(), "dirty target must remain untouched");
    assert!(!Path::new(&clone_staging_path(target_path)).exists(), "rejected target must not start a staged clone");

    fs::remove_dir_all(&target).expect("remove transient obstruction");
    let default_branch = runtime.clone_and_inspect(repo_url, target_path).await.expect("clone should recover after obstruction removal");

    assert_eq!(default_branch.as_deref(), Some("main"));
}
