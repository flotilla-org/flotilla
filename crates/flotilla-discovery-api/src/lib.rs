//! Provider construction contracts shared by discovery and provider components.
//!
//! ADR 0060 (`docs/adr/0060-component-crates-and-composition-root.md`): depend
//! only on base types and provider-runtime; never on core, provider components,
//! discovery assembly, storage or composition roots. The build-graph gate
//! enforces the same upward-dependency prohibition as provider-runtime.
pub mod detectors;
pub mod environment;
pub mod provider_config;

use async_trait::async_trait;
pub use environment::{host_environment_keys, EnvironmentAssertion, EnvironmentBag, VcsKind};
use flotilla_paths::path_context::ExecutionEnvironmentPath;
use flotilla_provider_runtime::{CommandRunner, EnvVars};
use provider_config::ProviderConfigView;
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Unmet requirements and provider descriptor
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum UnmetRequirement {
    MissingBinary(String),
    MissingEnvVar(String),
    MissingAuth(String),
    MissingConfig(String),
    MissingRemoteHost(String),
    NoVcsCheckout,
    /// Config references a backend or implementation that no factory provides.
    UnknownProviderPreference {
        category: ProviderCategory,
        key: String,
    },
}

impl std::fmt::Display for UnmetRequirement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingBinary(name) => write!(f, "missing binary {}", bounded_requirement_name(name)),
            Self::MissingEnvVar(name) => write!(f, "missing environment variable {}", bounded_requirement_name(name)),
            Self::MissingAuth(_) => f.write_str("authentication unavailable"),
            Self::MissingConfig(_) => f.write_str("configuration unavailable"),
            Self::MissingRemoteHost(name) => write!(f, "missing remote host {}", bounded_requirement_name(name)),
            Self::NoVcsCheckout => f.write_str("no VCS checkout"),
            Self::UnknownProviderPreference { category, key } => {
                write!(f, "unknown {} provider {}", category.display_name(), bounded_requirement_name(key))
            }
        }
    }
}

fn bounded_requirement_name(name: &str) -> String {
    name.chars().take(80).collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProviderCategory {
    Vcs,
    ChangeRequest,
    IssueProvider,
    CloudAgent,
    AiUtility,
    TerminalPool,
    EnvironmentProvider,
}

impl ProviderCategory {
    pub const ALL: [Self; 7] = [
        Self::Vcs,
        Self::ChangeRequest,
        Self::IssueProvider,
        Self::CloudAgent,
        Self::AiUtility,
        Self::TerminalPool,
        Self::EnvironmentProvider,
    ];

    pub fn slug(&self) -> &'static str {
        match self {
            Self::Vcs => "vcs",
            Self::ChangeRequest => "change_request",
            Self::IssueProvider => "issue_tracker",
            Self::CloudAgent => "cloud_agent",
            Self::AiUtility => "ai_utility",
            Self::TerminalPool => "terminal_pool",
            Self::EnvironmentProvider => "environment_provider",
        }
    }

    pub fn display_name(&self) -> &'static str {
        match self {
            Self::Vcs => "VCS",
            Self::ChangeRequest => "Change Requests",
            Self::IssueProvider => "Issue Provider",
            Self::CloudAgent => "Cloud Agent",
            Self::AiUtility => "AI Utility",
            Self::TerminalPool => "Terminal Pool",
            Self::EnvironmentProvider => "Environment Provider",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ProviderDescriptor {
    pub category: ProviderCategory,
    pub backend: String,
    pub implementation: String,
    pub display_name: String,
    pub abbreviation: String,
    pub section_label: String,
    pub item_noun: String,
}

impl ProviderDescriptor {
    pub fn named(category: ProviderCategory, name: impl Into<String>) -> Self {
        let name = name.into();
        Self {
            category,
            backend: name.clone(),
            implementation: name.clone(),
            display_name: name,
            abbreviation: String::new(),
            section_label: String::new(),
            item_noun: String::new(),
        }
    }

    pub fn labeled(
        category: ProviderCategory,
        backend: impl Into<String>,
        implementation: impl Into<String>,
        display_name: impl Into<String>,
        abbreviation: impl Into<String>,
        section_label: impl Into<String>,
        item_noun: impl Into<String>,
    ) -> Self {
        Self {
            category,
            backend: backend.into(),
            implementation: implementation.into(),
            display_name: display_name.into(),
            abbreviation: abbreviation.into(),
            section_label: section_label.into(),
            item_noun: item_noun.into(),
        }
    }

    /// Shorthand for backends with a single implementation — sets `implementation = backend`.
    /// Use `labeled()` when a backend has multiple implementations (e.g. claude api vs cli).
    pub fn labeled_simple(
        category: ProviderCategory,
        backend: impl Into<String>,
        display_name: impl Into<String>,
        abbreviation: impl Into<String>,
        section_label: impl Into<String>,
        item_noun: impl Into<String>,
    ) -> Self {
        let backend = backend.into();
        Self {
            category,
            implementation: backend.clone(),
            backend,
            display_name: display_name.into(),
            abbreviation: abbreviation.into(),
            section_label: section_label.into(),
            item_noun: item_noun.into(),
        }
    }
}

// ---------------------------------------------------------------------------
// Detector traits
// ---------------------------------------------------------------------------

#[async_trait]
pub trait HostDetector: Send + Sync {
    async fn detect(&self, runner: &dyn CommandRunner, env: &dyn EnvVars) -> Vec<EnvironmentAssertion>;
}

#[async_trait]
pub trait RepoDetector: Send + Sync {
    async fn detect(
        &self,
        repo_root: &ExecutionEnvironmentPath,
        runner: &dyn CommandRunner,
        env: &dyn EnvVars,
    ) -> Vec<EnvironmentAssertion>;
}

// ---------------------------------------------------------------------------
// Factory trait and category aliases
// ---------------------------------------------------------------------------

#[async_trait]
pub trait Factory: Send + Sync {
    type Descriptor;
    type Output: ?Sized + Send + Sync;

    fn descriptor(&self) -> Self::Descriptor;

    async fn probe(
        &self,
        env: &EnvironmentBag,
        config: &dyn ProviderConfigView,
        repo_root: &ExecutionEnvironmentPath,
        runner: Arc<dyn CommandRunner>,
    ) -> Result<Arc<Self::Output>, Vec<UnmetRequirement>>;
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod requirement_display_tests {
    use super::*;
    // Human diagnostics use bounded requirement names; auth/config details may contain secrets.
    #[test]
    fn requirements_use_safe_human_descriptions() {
        assert_eq!(UnmetRequirement::MissingBinary("claude".into()).to_string(), "missing binary claude");
        assert_eq!(
            UnmetRequirement::MissingEnvVar("ANTHROPIC_API_KEY".into()).to_string(),
            "missing environment variable ANTHROPIC_API_KEY"
        );
        assert_eq!(UnmetRequirement::MissingAuth("private-token".into()).to_string(), "authentication unavailable");
        assert_eq!(UnmetRequirement::MissingConfig("private-token".into()).to_string(), "configuration unavailable");
        assert!(UnmetRequirement::MissingBinary("x".repeat(10000)).to_string().len() <= 100);
    }
}
