//! Modular provider discovery system.
//!
//! This module defines the core types for environment detection and provider
//! factory registration. Detectors probe the host and repo for available tools,
//! producing `EnvironmentAssertion` values collected into an `EnvironmentBag`.
//! Factories consume the bag to construct typed provider instances.

pub mod detectors;
pub mod factories;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;

use crate::config::ConfigStore;
use crate::providers::ai_utility::AiUtility;
use crate::providers::code_review::CodeReview;
use crate::providers::coding_agent::CloudAgentService;
use crate::providers::issue_tracker::IssueTracker;
use crate::providers::terminal::TerminalPool;
use crate::providers::vcs::{CheckoutManager, Vcs};
use crate::providers::workspace::WorkspaceManager;
use crate::providers::CommandRunner;

// ---------------------------------------------------------------------------
// Environment assertion types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum VcsKind {
    Git,
    Jj,
}

#[derive(Debug, Clone, PartialEq)]
pub enum HostPlatform {
    GitHub,
    GitLab,
}

#[derive(Debug, Clone)]
pub enum EnvironmentAssertion {
    BinaryAvailable {
        name: String,
        path: PathBuf,
        version: Option<String>,
    },
    EnvVarSet {
        key: String,
        value: String,
    },
    VcsCheckoutDetected {
        root: PathBuf,
        kind: VcsKind,
        is_main_checkout: bool,
    },
    RemoteHost {
        platform: HostPlatform,
        owner: String,
        repo: String,
        remote_name: String,
    },
    AuthFileExists {
        provider: String,
        path: PathBuf,
    },
    SocketAvailable {
        name: String,
        path: PathBuf,
    },
}

// ---------------------------------------------------------------------------
// EnvironmentBag — typed query over collected assertions
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct EnvironmentBag {
    assertions: Vec<EnvironmentAssertion>,
}

impl EnvironmentBag {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, assertion: EnvironmentAssertion) {
        self.assertions.push(assertion);
    }

    pub fn extend(&mut self, assertions: Vec<EnvironmentAssertion>) {
        self.assertions.extend(assertions);
    }

    pub fn find_binary(&self, name: &str) -> Option<&PathBuf> {
        self.assertions.iter().find_map(|a| match a {
            EnvironmentAssertion::BinaryAvailable { name: n, path, .. } if n == name => Some(path),
            _ => None,
        })
    }

    pub fn find_env_var(&self, key: &str) -> Option<&str> {
        self.assertions.iter().find_map(|a| match a {
            EnvironmentAssertion::EnvVarSet { key: k, value } if k == key => Some(value.as_str()),
            _ => None,
        })
    }

    /// Find a remote host matching the given platform.
    /// Prefers `origin` over other remotes; falls back to the first match.
    pub fn find_remote_host(&self, platform: HostPlatform) -> Option<(&str, &str, &str)> {
        let mut first_match = None;
        for a in &self.assertions {
            if let EnvironmentAssertion::RemoteHost {
                platform: p,
                owner,
                repo,
                remote_name,
            } = a
            {
                if *p == platform {
                    if remote_name == "origin" {
                        return Some((owner.as_str(), repo.as_str(), remote_name.as_str()));
                    }
                    if first_match.is_none() {
                        first_match = Some((owner.as_str(), repo.as_str(), remote_name.as_str()));
                    }
                }
            }
        }
        first_match
    }

    pub fn remote_hosts(&self) -> Vec<&EnvironmentAssertion> {
        self.assertions
            .iter()
            .filter(|a| matches!(a, EnvironmentAssertion::RemoteHost { .. }))
            .collect()
    }

    pub fn has_auth(&self, provider: &str) -> bool {
        self.assertions.iter().any(|a| {
            matches!(
                a,
                EnvironmentAssertion::AuthFileExists { provider: p, .. } if p == provider
            )
        })
    }

    pub fn find_socket(&self, name: &str) -> Option<&PathBuf> {
        self.assertions.iter().find_map(|a| match a {
            EnvironmentAssertion::SocketAvailable { name: n, path, .. } if n == name => Some(path),
            _ => None,
        })
    }

    pub fn find_vcs_checkout(&self, kind: VcsKind) -> Option<(&Path, bool)> {
        self.assertions.iter().find_map(|a| match a {
            EnvironmentAssertion::VcsCheckoutDetected {
                root,
                kind: k,
                is_main_checkout,
            } if *k == kind => Some((root.as_path(), *is_main_checkout)),
            _ => None,
        })
    }

    /// Return `owner/repo` from the first remote host found (GitHub preferred).
    pub fn repo_slug(&self) -> Option<String> {
        self.find_remote_host(HostPlatform::GitHub)
            .or_else(|| self.find_remote_host(HostPlatform::GitLab))
            .map(|(owner, repo, _)| format!("{owner}/{repo}"))
    }

    /// Create a new bag containing assertions from both `self` and `other`.
    pub fn merge(&self, other: &EnvironmentBag) -> EnvironmentBag {
        let mut merged = self.clone();
        merged.assertions.extend(other.assertions.clone());
        merged
    }
}

// ---------------------------------------------------------------------------
// Unmet requirements and provider descriptor
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum UnmetRequirement {
    MissingBinary(String),
    MissingEnvVar(String),
    MissingAuth(String),
    MissingRemoteHost(HostPlatform),
    NoVcsCheckout,
}

#[derive(Debug, Clone)]
pub struct ProviderDescriptor {
    pub name: String,
    pub display_name: String,
    pub abbreviation: String,
    pub section_label: String,
    pub item_noun: String,
}

// ---------------------------------------------------------------------------
// Detector traits
// ---------------------------------------------------------------------------

#[async_trait]
pub trait HostDetector: Send + Sync {
    fn name(&self) -> &str;
    async fn detect(&self, runner: &dyn CommandRunner) -> Vec<EnvironmentAssertion>;
}

#[async_trait]
pub trait RepoDetector: Send + Sync {
    fn name(&self) -> &str;
    async fn detect(
        &self,
        repo_root: &Path,
        runner: &dyn CommandRunner,
    ) -> Vec<EnvironmentAssertion>;
}

// ---------------------------------------------------------------------------
// Factory traits — one per provider category
// ---------------------------------------------------------------------------

#[async_trait]
pub trait VcsFactory: Send + Sync {
    fn descriptor(&self) -> ProviderDescriptor;
    async fn probe(
        &self,
        env: &EnvironmentBag,
        config: &ConfigStore,
        repo_root: &Path,
        runner: Arc<dyn CommandRunner>,
    ) -> Result<Arc<dyn Vcs>, Vec<UnmetRequirement>>;
}

#[async_trait]
pub trait CheckoutManagerFactory: Send + Sync {
    fn descriptor(&self) -> ProviderDescriptor;
    async fn probe(
        &self,
        env: &EnvironmentBag,
        config: &ConfigStore,
        repo_root: &Path,
        runner: Arc<dyn CommandRunner>,
    ) -> Result<Arc<dyn CheckoutManager>, Vec<UnmetRequirement>>;
}

#[async_trait]
pub trait CodeReviewFactory: Send + Sync {
    fn descriptor(&self) -> ProviderDescriptor;
    async fn probe(
        &self,
        env: &EnvironmentBag,
        config: &ConfigStore,
        repo_root: &Path,
        runner: Arc<dyn CommandRunner>,
    ) -> Result<Arc<dyn CodeReview>, Vec<UnmetRequirement>>;
}

#[async_trait]
pub trait IssueTrackerFactory: Send + Sync {
    fn descriptor(&self) -> ProviderDescriptor;
    async fn probe(
        &self,
        env: &EnvironmentBag,
        config: &ConfigStore,
        repo_root: &Path,
        runner: Arc<dyn CommandRunner>,
    ) -> Result<Arc<dyn IssueTracker>, Vec<UnmetRequirement>>;
}

#[async_trait]
pub trait CloudAgentFactory: Send + Sync {
    fn descriptor(&self) -> ProviderDescriptor;
    async fn probe(
        &self,
        env: &EnvironmentBag,
        config: &ConfigStore,
        repo_root: &Path,
        runner: Arc<dyn CommandRunner>,
    ) -> Result<Arc<dyn CloudAgentService>, Vec<UnmetRequirement>>;
}

#[async_trait]
pub trait AiUtilityFactory: Send + Sync {
    fn descriptor(&self) -> ProviderDescriptor;
    async fn probe(
        &self,
        env: &EnvironmentBag,
        config: &ConfigStore,
        repo_root: &Path,
        runner: Arc<dyn CommandRunner>,
    ) -> Result<Arc<dyn AiUtility>, Vec<UnmetRequirement>>;
}

#[async_trait]
pub trait WorkspaceManagerFactory: Send + Sync {
    fn descriptor(&self) -> ProviderDescriptor;
    async fn probe(
        &self,
        env: &EnvironmentBag,
        config: &ConfigStore,
        repo_root: &Path,
        runner: Arc<dyn CommandRunner>,
    ) -> Result<Arc<dyn WorkspaceManager>, Vec<UnmetRequirement>>;
}

#[async_trait]
pub trait TerminalPoolFactory: Send + Sync {
    fn descriptor(&self) -> ProviderDescriptor;
    async fn probe(
        &self,
        env: &EnvironmentBag,
        config: &ConfigStore,
        repo_root: &Path,
        runner: Arc<dyn CommandRunner>,
    ) -> Result<Arc<dyn TerminalPool>, Vec<UnmetRequirement>>;
}

// ---------------------------------------------------------------------------
// Factory registry
// ---------------------------------------------------------------------------

pub struct FactoryRegistry {
    pub vcs: Vec<Box<dyn VcsFactory>>,
    pub checkout_managers: Vec<Box<dyn CheckoutManagerFactory>>,
    pub code_review: Vec<Box<dyn CodeReviewFactory>>,
    pub issue_trackers: Vec<Box<dyn IssueTrackerFactory>>,
    pub cloud_agents: Vec<Box<dyn CloudAgentFactory>>,
    pub ai_utilities: Vec<Box<dyn AiUtilityFactory>>,
    pub workspace_managers: Vec<Box<dyn WorkspaceManagerFactory>>,
    pub terminal_pools: Vec<Box<dyn TerminalPoolFactory>>,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_bag() -> EnvironmentBag {
        let mut bag = EnvironmentBag::new();
        bag.push(EnvironmentAssertion::BinaryAvailable {
            name: "git".into(),
            path: PathBuf::from("/usr/bin/git"),
            version: Some("2.40.0".into()),
        });
        bag.push(EnvironmentAssertion::BinaryAvailable {
            name: "gh".into(),
            path: PathBuf::from("/usr/bin/gh"),
            version: None,
        });
        bag.push(EnvironmentAssertion::EnvVarSet {
            key: "GITHUB_TOKEN".into(),
            value: "ghp_abc123".into(),
        });
        bag.push(EnvironmentAssertion::VcsCheckoutDetected {
            root: PathBuf::from("/home/user/project"),
            kind: VcsKind::Git,
            is_main_checkout: true,
        });
        bag.push(EnvironmentAssertion::RemoteHost {
            platform: HostPlatform::GitHub,
            owner: "acme".into(),
            repo: "widgets".into(),
            remote_name: "upstream".into(),
        });
        bag.push(EnvironmentAssertion::RemoteHost {
            platform: HostPlatform::GitHub,
            owner: "fork-owner".into(),
            repo: "widgets".into(),
            remote_name: "origin".into(),
        });
        bag.push(EnvironmentAssertion::AuthFileExists {
            provider: "github".into(),
            path: PathBuf::from("/home/user/.config/gh/hosts.yml"),
        });
        bag.push(EnvironmentAssertion::SocketAvailable {
            name: "cmux".into(),
            path: PathBuf::from("/tmp/cmux.sock"),
        });
        bag
    }

    #[test]
    fn find_binary_returns_matching_path() {
        let bag = sample_bag();
        assert_eq!(bag.find_binary("git"), Some(&PathBuf::from("/usr/bin/git")));
        assert_eq!(bag.find_binary("gh"), Some(&PathBuf::from("/usr/bin/gh")));
        assert_eq!(bag.find_binary("nonexistent"), None);
    }

    #[test]
    fn find_env_var_returns_value() {
        let bag = sample_bag();
        assert_eq!(bag.find_env_var("GITHUB_TOKEN"), Some("ghp_abc123"));
        assert_eq!(bag.find_env_var("MISSING"), None);
    }

    #[test]
    fn find_remote_host_prefers_origin() {
        let bag = sample_bag();
        let result = bag.find_remote_host(HostPlatform::GitHub);
        // Should prefer origin over upstream
        assert_eq!(result, Some(("fork-owner", "widgets", "origin")));
    }

    #[test]
    fn find_remote_host_falls_back_to_first() {
        let mut bag = EnvironmentBag::new();
        bag.push(EnvironmentAssertion::RemoteHost {
            platform: HostPlatform::GitHub,
            owner: "acme".into(),
            repo: "widgets".into(),
            remote_name: "upstream".into(),
        });
        bag.push(EnvironmentAssertion::RemoteHost {
            platform: HostPlatform::GitHub,
            owner: "other".into(),
            repo: "widgets".into(),
            remote_name: "fork".into(),
        });
        let result = bag.find_remote_host(HostPlatform::GitHub);
        assert_eq!(result, Some(("acme", "widgets", "upstream")));
    }

    #[test]
    fn find_remote_host_filters_by_platform() {
        let bag = sample_bag();
        assert_eq!(bag.find_remote_host(HostPlatform::GitLab), None);
    }

    #[test]
    fn has_auth_checks_provider() {
        let bag = sample_bag();
        assert!(bag.has_auth("github"));
        assert!(!bag.has_auth("gitlab"));
    }

    #[test]
    fn find_vcs_checkout_returns_root_and_flag() {
        let bag = sample_bag();
        let result = bag.find_vcs_checkout(VcsKind::Git);
        assert_eq!(result, Some((Path::new("/home/user/project"), true)));
        assert_eq!(bag.find_vcs_checkout(VcsKind::Jj), None);
    }

    #[test]
    fn repo_slug_from_github() {
        let bag = sample_bag();
        // origin is fork-owner/widgets
        assert_eq!(bag.repo_slug(), Some("fork-owner/widgets".into()));
    }

    #[test]
    fn repo_slug_falls_back_to_gitlab() {
        let mut bag = EnvironmentBag::new();
        bag.push(EnvironmentAssertion::RemoteHost {
            platform: HostPlatform::GitLab,
            owner: "gl-org".into(),
            repo: "project".into(),
            remote_name: "origin".into(),
        });
        assert_eq!(bag.repo_slug(), Some("gl-org/project".into()));
    }

    #[test]
    fn repo_slug_none_when_empty() {
        let bag = EnvironmentBag::new();
        assert_eq!(bag.repo_slug(), None);
    }

    #[test]
    fn merge_combines_assertions() {
        let mut bag1 = EnvironmentBag::new();
        bag1.push(EnvironmentAssertion::BinaryAvailable {
            name: "git".into(),
            path: PathBuf::from("/usr/bin/git"),
            version: None,
        });

        let mut bag2 = EnvironmentBag::new();
        bag2.push(EnvironmentAssertion::BinaryAvailable {
            name: "gh".into(),
            path: PathBuf::from("/usr/bin/gh"),
            version: None,
        });

        let merged = bag1.merge(&bag2);
        assert!(merged.find_binary("git").is_some());
        assert!(merged.find_binary("gh").is_some());
        // Originals unchanged
        assert!(bag1.find_binary("gh").is_none());
    }

    #[test]
    fn find_socket_returns_path() {
        let bag = sample_bag();
        assert_eq!(
            bag.find_socket("cmux"),
            Some(&PathBuf::from("/tmp/cmux.sock"))
        );
        assert_eq!(bag.find_socket("nonexistent"), None);
    }

    #[test]
    fn remote_hosts_returns_all() {
        let bag = sample_bag();
        let hosts = bag.remote_hosts();
        assert_eq!(hosts.len(), 2);
    }

    #[test]
    fn extend_adds_multiple() {
        let mut bag = EnvironmentBag::new();
        bag.extend(vec![
            EnvironmentAssertion::BinaryAvailable {
                name: "a".into(),
                path: PathBuf::from("/a"),
                version: None,
            },
            EnvironmentAssertion::BinaryAvailable {
                name: "b".into(),
                path: PathBuf::from("/b"),
                version: None,
            },
        ]);
        assert!(bag.find_binary("a").is_some());
        assert!(bag.find_binary("b").is_some());
    }

    #[test]
    fn unmet_requirement_variants() {
        // Verify that all UnmetRequirement variants can be constructed and compared
        let reqs = vec![
            UnmetRequirement::MissingBinary("git".into()),
            UnmetRequirement::MissingEnvVar("TOKEN".into()),
            UnmetRequirement::MissingAuth("github".into()),
            UnmetRequirement::MissingRemoteHost(HostPlatform::GitHub),
            UnmetRequirement::NoVcsCheckout,
        ];
        assert_eq!(reqs[0], UnmetRequirement::MissingBinary("git".into()));
        assert_ne!(reqs[0], reqs[1]);
    }

    #[test]
    fn provider_descriptor_fields() {
        let desc = ProviderDescriptor {
            name: "github-cr".into(),
            display_name: "GitHub PRs".into(),
            abbreviation: "PR".into(),
            section_label: "Pull Requests".into(),
            item_noun: "pull request".into(),
        };
        assert_eq!(desc.name, "github-cr");
        assert_eq!(desc.display_name, "GitHub PRs");
        assert_eq!(desc.abbreviation, "PR");
        assert_eq!(desc.section_label, "Pull Requests");
        assert_eq!(desc.item_noun, "pull request");
    }
}
