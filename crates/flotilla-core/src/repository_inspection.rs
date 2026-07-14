use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use flotilla_resources::{RepositoryKey, RepositorySpec};

use crate::providers::{ChannelLabel, CommandRunner};

#[derive(Debug, Clone, PartialEq, Eq, bon::Builder)]
pub struct LocalCheckoutInspection {
    pub path: PathBuf,
    pub host_ref: String,
    pub git_ref: String,
    pub is_main: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositoryInspection {
    pub spec: RepositorySpec,
    pub checkout: LocalCheckoutInspection,
    pub transport_url: Option<String>,
}

impl RepositoryInspection {
    pub fn key(&self) -> RepositoryKey {
        self.spec.key()
    }
}

#[async_trait]
pub trait RepositoryInspector: Send + Sync {
    async fn inspect_path(&self, path: &Path, remote: Option<&str>) -> Result<RepositoryInspection, String>;
}

pub struct GitRepositoryInspector {
    runner: Arc<dyn CommandRunner>,
    host_ref: String,
}

impl GitRepositoryInspector {
    pub fn new(runner: Arc<dyn CommandRunner>, host_ref: impl Into<String>) -> Self {
        Self { runner, host_ref: host_ref.into() }
    }

    async fn git(&self, cwd: &Path, args: &[&str]) -> Result<String, String> {
        self.runner
            .run("git", args, cwd, &ChannelLabel::Noop)
            .await
            .map(|output| output.trim().to_string())
            .map_err(|error| format!("git {} in {}: {error}", args.join(" "), cwd.display()))
    }

    async fn selected_remote(&self, cwd: &Path, branch: &str, requested: Option<&str>) -> Result<Option<String>, String> {
        if let Some(requested) = requested {
            if looks_like_remote_url(requested) {
                return Ok(Some(requested.to_string()));
            }
            return self
                .git(cwd, &["remote", "get-url", requested])
                .await
                .map(Some)
                .map_err(|_| format!("remote `{requested}` is not configured for {}", cwd.display()));
        }

        let remotes = self
            .git(cwd, &["remote"])
            .await?
            .lines()
            .map(str::trim)
            .filter(|remote| !remote.is_empty())
            .map(str::to_string)
            .collect::<Vec<_>>();
        match remotes.as_slice() {
            [] => Ok(None),
            [remote] => self.git(cwd, &["remote", "get-url", remote]).await.map(Some),
            _ => {
                let branch_key = format!("branch.{branch}.remote");
                let tracked = self.git(cwd, &["config", "--get", &branch_key]).await.ok();
                match tracked.filter(|tracked| remotes.contains(tracked)) {
                    Some(remote) => self.git(cwd, &["remote", "get-url", &remote]).await.map(Some),
                    None => {
                        let mut identities = std::collections::BTreeMap::new();
                        for remote in &remotes {
                            let url = self.git(cwd, &["remote", "get-url", remote]).await?;
                            identities.insert(self.canonical_remote(cwd, &url).await?, url);
                        }
                        if identities.len() == 1 {
                            Ok(identities.into_values().next())
                        } else {
                            Err(format!(
                                "repository {} has multiple distinct remotes ({}); select one with --remote",
                                cwd.display(),
                                remotes.join(", ")
                            ))
                        }
                    }
                }
            }
        }
    }

    async fn canonical_remote(&self, cwd: &Path, remote: &str) -> Result<String, String> {
        let Some(host) = ssh_remote_host(remote) else {
            return flotilla_resources::canonicalize_repo_url(remote);
        };
        if host.contains('.') {
            return flotilla_resources::canonicalize_repo_url(remote);
        }
        let ssh_config = self
            .runner
            .run("ssh", &["-G", host], cwd, &ChannelLabel::Noop)
            .await
            .map_err(|_| format!("unrecognised remote host alias `{host}`"))?;
        let resolved = ssh_config
            .lines()
            .find_map(|line| {
                let (key, value) = line.trim().split_once(char::is_whitespace)?;
                key.eq_ignore_ascii_case("hostname").then(|| value.trim())
            })
            .filter(|resolved| !resolved.is_empty() && *resolved != host)
            .ok_or_else(|| format!("unrecognised remote host alias `{host}`"))?;
        flotilla_resources::canonicalize_repo_url(&remote.replacen(host, resolved, 1))
    }
}

#[async_trait]
impl RepositoryInspector for GitRepositoryInspector {
    async fn inspect_path(&self, path: &Path, remote: Option<&str>) -> Result<RepositoryInspection, String> {
        let path =
            std::fs::canonicalize(path).map_err(|error| format!("repository path {} cannot be resolved: {error}", path.display()))?;
        let top_level = PathBuf::from(self.git(&path, &["rev-parse", "--show-toplevel"]).await?);
        let top_level = std::fs::canonicalize(&top_level)
            .map_err(|error| format!("repository root {} cannot be resolved: {error}", top_level.display()))?;
        let branch = self.git(&top_level, &["rev-parse", "--abbrev-ref", "HEAD"]).await?;
        let git_ref = if branch == "HEAD" { self.git(&top_level, &["rev-parse", "HEAD"]).await? } else { branch.clone() };
        let selected_remote = self.selected_remote(&top_level, &branch, remote).await?;
        let (spec, transport_url) = match selected_remote {
            Some(remote) => (RepositorySpec::remote(self.canonical_remote(&top_level, &remote).await?)?, Some(remote)),
            None => {
                let common_dir = PathBuf::from(self.git(&top_level, &["rev-parse", "--git-common-dir"]).await?);
                let common_dir = if common_dir.is_absolute() { common_dir } else { top_level.join(common_dir) };
                let common_dir = std::fs::canonicalize(&common_dir)
                    .map_err(|error| format!("git common directory {} cannot be resolved: {error}", common_dir.display()))?;
                (RepositorySpec::local(&self.host_ref, common_dir.to_string_lossy())?, None)
            }
        };
        Ok(RepositoryInspection {
            spec,
            checkout: LocalCheckoutInspection {
                path: top_level,
                host_ref: self.host_ref.clone(),
                git_ref: git_ref.clone(),
                is_main: matches!(git_ref.as_str(), "main" | "master" | "trunk"),
            },
            transport_url,
        })
    }
}

fn looks_like_remote_url(value: &str) -> bool {
    value.contains("://") || value.contains(':')
}

fn ssh_remote_host(remote: &str) -> Option<&str> {
    if let Some(rest) = remote.strip_prefix("ssh://") {
        return rest.split('/').next()?.rsplit_once('@').map_or(Some(rest.split('/').next()?), |(_, host)| Some(host));
    }
    if remote.contains("://") {
        return None;
    }
    let (authority, path) = remote.split_once(':')?;
    (!path.is_empty()).then(|| authority.rsplit_once('@').map_or(authority, |(_, host)| host))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use flotilla_resources::RepositoryIdentity;

    use super::{GitRepositoryInspector, RepositoryInspector};
    use crate::providers::discovery::test_support::DiscoveryMockRunner;

    fn git_repo() -> (tempfile::TempDir, std::path::PathBuf) {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let root = temp.path().join("repo");
        std::fs::create_dir(&root).expect("repo dir");
        std::fs::create_dir(root.join(".git")).expect("git dir");
        (temp, root)
    }

    #[tokio::test]
    async fn ssh_alias_resolves_to_machine_independent_remote_identity() {
        let (_temp, root) = git_repo();
        let runner = DiscoveryMockRunner::builder()
            .on_run("git", &["rev-parse", "--show-toplevel"], Ok(root.to_string_lossy().into_owned()))
            .on_run("git", &["rev-parse", "--abbrev-ref", "HEAD"], Ok("main\n".to_string()))
            .on_run("git", &["remote"], Ok("origin\n".to_string()))
            .on_run("git", &["remote", "get-url", "origin"], Ok("work-github:org/repo.git\n".to_string()))
            .on_run("ssh", &["-G", "work-github"], Ok("hostname github.com\nuser git\n".to_string()))
            .build();
        let inspector = GitRepositoryInspector::new(Arc::new(runner), "host-01");

        let inspected = inspector.inspect_path(&root, None).await.expect("inspection should succeed");

        assert!(matches!(
            inspected.spec.identity(),
            RepositoryIdentity::Remote { canonical_remote } if canonical_remote == "https://github.com/org/repo"
        ));
    }

    #[tokio::test]
    async fn unresolved_ssh_alias_fails_instead_of_becoming_a_repository_key() {
        let (_temp, root) = git_repo();
        let runner = DiscoveryMockRunner::builder()
            .on_run("git", &["rev-parse", "--show-toplevel"], Ok(root.to_string_lossy().into_owned()))
            .on_run("git", &["rev-parse", "--abbrev-ref", "HEAD"], Ok("main\n".to_string()))
            .on_run("git", &["remote"], Ok("origin\n".to_string()))
            .on_run("git", &["remote", "get-url", "origin"], Ok("mystery:org/repo.git\n".to_string()))
            .on_run("ssh", &["-G", "mystery"], Ok("hostname mystery\n".to_string()))
            .build();
        let inspector = GitRepositoryInspector::new(Arc::new(runner), "host-01");

        let error = inspector.inspect_path(&root, None).await.expect_err("unknown alias should fail");

        assert!(error.contains("unrecognised remote host alias"));
    }

    #[tokio::test]
    async fn remote_less_worktrees_with_one_common_dir_converge() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let first = temp.path().join("first");
        let second = temp.path().join("second");
        let common = temp.path().join("common.git");
        std::fs::create_dir(&first).expect("first");
        std::fs::create_dir(&second).expect("second");
        std::fs::create_dir(&common).expect("common");
        let runner = DiscoveryMockRunner::builder()
            .on_run("git", &["rev-parse", "--show-toplevel"], Ok(first.to_string_lossy().into_owned()))
            .on_run("git", &["rev-parse", "--show-toplevel"], Ok(second.to_string_lossy().into_owned()))
            .on_run("git", &["rev-parse", "--abbrev-ref", "HEAD"], Ok("main\n".to_string()))
            .on_run("git", &["rev-parse", "--abbrev-ref", "HEAD"], Ok("feature\n".to_string()))
            .on_run("git", &["remote"], Ok(String::new()))
            .on_run("git", &["remote"], Ok(String::new()))
            .on_run("git", &["rev-parse", "--git-common-dir"], Ok(common.to_string_lossy().into_owned()))
            .on_run("git", &["rev-parse", "--git-common-dir"], Ok(common.to_string_lossy().into_owned()))
            .build();
        let inspector = GitRepositoryInspector::new(Arc::new(runner), "host-01");

        let first = inspector.inspect_path(&first, None).await.expect("first inspection");
        let second = inspector.inspect_path(&second, None).await.expect("second inspection");

        assert_eq!(first.key(), second.key());
    }

    #[tokio::test]
    async fn ambiguous_multiple_remotes_require_an_explicit_selection() {
        let (_temp, root) = git_repo();
        let runner = DiscoveryMockRunner::builder()
            .on_run("git", &["rev-parse", "--show-toplevel"], Ok(root.to_string_lossy().into_owned()))
            .on_run("git", &["rev-parse", "--abbrev-ref", "HEAD"], Ok("main\n".to_string()))
            .on_run("git", &["remote"], Ok("origin\nupstream\n".to_string()))
            .on_run("git", &["remote", "get-url", "origin"], Ok("https://github.com/fork/repo.git\n".to_string()))
            .on_run("git", &["remote", "get-url", "upstream"], Ok("https://github.com/upstream/repo.git\n".to_string()))
            .build();
        let inspector = GitRepositoryInspector::new(Arc::new(runner), "host-01");

        let error = inspector.inspect_path(&root, None).await.expect_err("ambiguous remotes should fail");

        assert!(error.contains("multiple distinct remotes"));
        assert!(error.contains("--remote"));
    }

    #[tokio::test]
    async fn multiple_remote_names_with_one_normalized_identity_are_unambiguous() {
        let (_temp, root) = git_repo();
        let runner = DiscoveryMockRunner::builder()
            .on_run("git", &["rev-parse", "--show-toplevel"], Ok(root.to_string_lossy().into_owned()))
            .on_run("git", &["rev-parse", "--abbrev-ref", "HEAD"], Ok("main\n".to_string()))
            .on_run("git", &["remote"], Ok("origin\nmirror\n".to_string()))
            .on_run("git", &["remote", "get-url", "origin"], Ok("https://github.com/org/repo.git\n".to_string()))
            .on_run("git", &["remote", "get-url", "mirror"], Ok("git@github.com:org/repo.git\n".to_string()))
            .build();
        let inspector = GitRepositoryInspector::new(Arc::new(runner), "host-01");

        let inspected = inspector.inspect_path(&root, None).await.expect("same identity should be unambiguous");

        assert!(matches!(
            inspected.spec.identity(),
            RepositoryIdentity::Remote { canonical_remote } if canonical_remote == "https://github.com/org/repo"
        ));
    }
}
