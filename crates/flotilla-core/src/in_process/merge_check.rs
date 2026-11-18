//! Git's judgement of a change request's merge, made in this host's clone of its repository
//! (run 4 R4-4): Forgejo's API cannot tell a conflict from a merge check still running.

use std::{path::Path, sync::Arc};

use async_trait::async_trait;
use flotilla_change_request::{MergeCandidate, MergeCheck};
use flotilla_resources::{clone_records::CloneRecord, lifecycle_reads::LifecycleReads, ObservedMergeability, RepositoryKey};
use flotilla_schema::reads::carry;
use flotilla_vcs::vcs::{CheckoutVcsResolver, MergeTreeCheck, MergeTreeOutcome};

/// Judges merges in the newest ready clone of `repository` held at this home, the one the
/// engine's Clone controller made or adopted. Only refs are read, never its working tree, which
/// may be stale.
pub(super) struct ManagedCloneMergeCheck {
    pub(super) lifecycle_reads: Arc<dyn LifecycleReads>,
    pub(super) vcs: Arc<dyn CheckoutVcsResolver>,
    pub(super) repository: RepositoryKey,
}

impl ManagedCloneMergeCheck {
    async fn clone_here(&self) -> Result<Option<CloneRecord>, String> {
        let (reads, repository) = (Arc::clone(&self.lifecycle_reads), self.repository.clone());
        tokio::task::spawn_blocking(carry(move || {
            let home = reads.home()?.to_string();
            let clones = reads.clones()?.of_repository(&repository)?;
            Ok(clones.into_iter().filter(|clone| clone.home == home && clone.is_ready()).max_by_key(|clone| clone.requested_at))
        }))
        .await
        .map_err(|error| format!("clone readings: {error}"))?
    }

    async fn judge(&self, candidate: &MergeCandidate) -> Result<Option<MergeTreeOutcome>, String> {
        let Some(clone) = self.clone_here().await? else { return Ok(None) };
        let vcs = self.vcs.vcs_for_env_ref(&clone.spec.env_ref, Path::new(&clone.spec.path)).await?;
        let check = MergeTreeCheck {
            remote: "origin",
            base: &candidate.base,
            base_ref: &candidate.base_ref,
            head: &candidate.head,
            head_ref: &candidate.head_ref,
        };
        vcs.merge_tree(check).await.map(Some).map_err(|error| format!("clone {} at {}: {error}", clone.name, clone.spec.path))
    }
}

#[async_trait]
impl MergeCheck for ManagedCloneMergeCheck {
    async fn mergeability(&self, candidate: &MergeCandidate) -> Option<ObservedMergeability> {
        match self.judge(candidate).await {
            Ok(Some(MergeTreeOutcome::Clean)) => Some(ObservedMergeability::Mergeable),
            Ok(Some(MergeTreeOutcome::Conflicting { paths })) => {
                tracing::info!(repository = %self.repository, head = %candidate.head, base = %candidate.base, ?paths, "change request conflicts with its base");
                Some(ObservedMergeability::Conflicting)
            }
            Ok(None) => {
                tracing::debug!(repository = %self.repository, "no ready clone here to judge a change request's merge");
                None
            }
            Err(error) => {
                tracing::warn!(repository = %self.repository, head = %candidate.head, %error, "could not judge a change request's merge");
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flotilla_config::default_checkout_path;
    use flotilla_paths::path_context::ExecutionEnvironmentPath;
    use flotilla_protocol::LifecycleReading;
    use flotilla_provider_runtime::{CommandRunner, ProcessCommandRunner};
    use flotilla_resources::{
        clone_records::{CloneRecords, CloneRenewal},
        lifecycle_reads::{LateLifecycleReads, ProvisioningKind},
        CloneSpec, RepositorySpec,
    };
    use flotilla_vcs::{
        providers::git_worktree::GitWorktreeStrategy,
        vcs::{FlotillaVcs, GitCheckoutStrategy, Vcs},
    };

    /// The engine's clones as this home reads them: these launches, and nothing else.
    struct Clones(Vec<CloneRecord>, flotilla_resources::held_joins::HeldJoins);

    impl LifecycleReads for Clones {
        fn home(&self) -> Result<&str, String> {
            Ok("here")
        }
        fn try_reading(&self, _: ProvisioningKind, _: &str, _: Option<&str>) -> Result<Option<LifecycleReading>, String> {
            Ok(None)
        }
        fn joins(&self) -> Result<&flotilla_resources::held_joins::HeldJoins, String> {
            Ok(&self.1)
        }
        fn clones(&self) -> Result<&dyn CloneRecords, String> {
            Ok(self)
        }
    }

    impl CloneRecords for Clones {
        fn codes(&self) -> Vec<flotilla_schema::KindCode> {
            Vec::new()
        }
        fn get(&self, name: &str, _: Option<&str>) -> Result<Option<CloneRecord>, String> {
            Ok(self.0.iter().find(|clone| clone.name == name).cloned())
        }
        fn of_repository(&self, repository: &RepositoryKey) -> Result<Vec<CloneRecord>, String> {
            Ok(self.0.iter().filter(|clone| &clone.spec.repo_ref == repository).cloned().collect())
        }
        fn here(&self) -> Result<Vec<CloneRecord>, String> {
            Ok(self.0.iter().filter(|clone| clone.home == "here").cloned().collect())
        }
        fn every(&self) -> Result<Vec<CloneRecord>, String> {
            Ok(self.0.clone())
        }
        fn request(&self, _: &RepositorySpec, _: &CloneSpec, _: bool) -> Result<String, String> {
            Err("read only".into())
        }
        fn renew(&self, _: &str) -> Result<CloneRenewal, String> {
            Err("read only".into())
        }
    }

    /// Resolves each path to the Git VCS of that path, as the host's checkout providers do.
    struct GitAtPath;

    #[async_trait]
    impl CheckoutVcsResolver for GitAtPath {
        async fn vcs_for(&self, _: Option<&flotilla_protocol::EnvironmentId>, path: &Path) -> Result<Arc<dyn Vcs>, String> {
            self.vcs_for_env_ref("local", path).await
        }
        async fn vcs_for_env_ref(&self, _: &str, path: &Path) -> Result<Arc<dyn Vcs>, String> {
            let runner: Arc<dyn CommandRunner> = Arc::new(ProcessCommandRunner);
            let strategy = GitCheckoutStrategy::Worktree(Box::new(GitWorktreeStrategy::new(default_checkout_path(), Arc::clone(&runner))));
            Ok(Arc::new(FlotillaVcs::new(ExecutionEnvironmentPath::new(path), runner, strategy)))
        }
    }

    fn git(cwd: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git").args(args).current_dir(cwd).output().expect("spawn git");
        assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
        String::from_utf8(output.stdout).expect("utf8").trim().to_string()
    }

    fn commit(repo: &Path, file: &str, contents: &str) -> String {
        std::fs::write(repo.join(file), contents).expect("write");
        git(repo, &["add", file]);
        git(repo, &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-m", file]);
        git(repo, &["rev-parse", "HEAD"])
    }

    fn launch(name: &str, home: &str, repository: &RepositoryKey, path: &Path, reading: LifecycleReading) -> CloneRecord {
        CloneRecord {
            name: name.into(),
            home: home.into(),
            spec: CloneSpec {
                repo_ref: repository.clone(),
                url: "unused".into(),
                env_ref: "local".into(),
                path: path.to_string_lossy().into_owned(),
            },
            discovered: false,
            requested_at: chrono::Utc::now(),
            reading,
            default_branch: Some("main".into()),
        }
    }

    // Behaviour (run 4 R4-4): a change request is judged in this home's ready clone of its
    // repository, from the commits its forge reported, fetched by ref: a conflict with the
    // moved base reads Conflicting and a clean merge Mergeable. Another home's clone, a clone
    // still being made, or no readable clones at all leave the merge unknown, never mergeable.
    #[tokio::test]
    async fn a_merge_is_judged_in_this_homes_ready_clone() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path();
        let work = root.join("work");
        std::fs::create_dir(&work).expect("work");
        git(&work, &["init", "-q", "-b", "main"]);
        commit(&work, "shared.txt", "one\n");
        git(root, &["clone", "-q", "--bare", "work", "remote.git"]);
        git(root, &["clone", "-q", "remote.git", "clone"]);
        git(&work, &["switch", "-q", "-c", "pr"]);
        let head = commit(&work, "shared.txt", "theirs\n");
        git(&work, &["switch", "-q", "main"]);
        let base = commit(&work, "shared.txt", "ours\n");
        git(&work, &["push", "-q", "../remote.git", "main", "pr:refs/pull/1/head"]);

        let repository = RepositorySpec::remote("https://forgejo.example/team/repo.git").expect("repository").key();
        let clone = root.join("clone");
        let check = |clones: Vec<CloneRecord>| ManagedCloneMergeCheck {
            lifecycle_reads: Arc::new(Clones(clones, Default::default())),
            vcs: Arc::new(GitAtPath),
            repository: repository.clone(),
        };
        let candidate = |head: &str, base: &str| MergeCandidate {
            head: head.into(),
            head_ref: "refs/pull/1/head".into(),
            base: base.into(),
            base_ref: "refs/heads/main".into(),
        };
        let ready = LifecycleReading::Ready { identity: "clone".into() };
        let here = check(vec![launch("clone-a#1", "here", &repository, &clone, ready.clone())]);
        assert_eq!(here.mergeability(&candidate(&head, &base)).await, Some(ObservedMergeability::Conflicting));
        let seed = git(&work, &["rev-parse", "main~1"]);
        assert_eq!(here.mergeability(&candidate(&head, &seed)).await, Some(ObservedMergeability::Mergeable));

        let missing = root.join("absent");
        for unjudged in [
            check(vec![launch("clone-a#1", "elsewhere", &repository, &clone, ready.clone())]),
            check(vec![launch("clone-a#1", "here", &repository, &clone, LifecycleReading::InProgress)]),
            check(vec![launch("clone-a#1", "here", &repository, &missing, ready.clone())]),
        ] {
            assert_eq!(unjudged.mergeability(&candidate(&head, &base)).await, None);
        }
        let unread = ManagedCloneMergeCheck {
            lifecycle_reads: Arc::new(LateLifecycleReads::default()),
            vcs: Arc::new(GitAtPath),
            repository: repository.clone(),
        };
        assert_eq!(unread.mergeability(&candidate(&head, &base)).await, None);
    }
}
