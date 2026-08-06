use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::{resource::define_resource, status_patch::NoStatusPatch, ReplicationClass, RepositoryKey, Stance};

pub const CREDENTIAL_REFS_ANNOTATION: &str = "flotilla.work/credential-refs";
pub const CREDENTIAL_REFS_ENV: &str = "FLOTILLA_CREDENTIAL_REFS";
pub const CREDENTIAL_REF_SESSION_TAG: &str = "flotilla-credential";
pub const CREDENTIAL_SCOPES_ANNOTATION: &str = "flotilla.work/credential-scopes";
pub const CREDENTIAL_SCOPES_ENV: &str = "FLOTILLA_CREDENTIAL_SCOPES";
pub const CREDENTIAL_SCOPES_SESSION_TAG: &str = "flotilla-credential-scopes";

define_resource!(CredentialSpec, "credentialspecs", CredentialSpecSpec, (), NoStatusPatch, replication = ReplicationClass::Definitions);
define_resource!(CredentialGrant, "credentialgrants", CredentialGrantSpec, (), NoStatusPatch, replication = ReplicationClass::Definitions);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct CredentialSpecSpec {
    pub consumer: CredentialConsumer,
    pub source: CredentialSource,
    pub lifecycle: CredentialLifecycle,
    #[builder(default)]
    #[serde(default)]
    pub placement: CredentialPlacementRequirements,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "adapter", rename_all = "kebab-case")]
pub enum CredentialConsumer {
    Gh,
    GithubApp { installation_id: u64 },
    Forgejo { api_url: String, username: String },
    Claude,
    ClaudeOauth { account_email: String },
    Codex,
    DockerRegistry { registry: String, username: String },
}

impl CredentialConsumer {
    pub fn adapter_name(&self) -> &'static str {
        match self {
            Self::Gh => "gh",
            Self::GithubApp { .. } => "github-app",
            Self::Forgejo { .. } => "forgejo",
            Self::Claude => "claude",
            Self::ClaudeOauth { .. } => "claude-oauth",
            Self::Codex => "codex",
            Self::DockerRegistry { .. } => "docker-registry",
        }
    }

    pub fn delivery_slot(&self) -> &'static str {
        match self {
            Self::Gh | Self::GithubApp { .. } => "github",
            // An API-key credential and a subscription OAuth credential must not
            // share one environment: `ANTHROPIC_API_KEY` outranks
            // `CLAUDE_CODE_OAUTH_TOKEN` in Claude's documented precedence, so the
            // OAuth identity would be silently ignored instead of used.
            Self::Claude | Self::ClaudeOauth { .. } => "claude",
            _ => self.adapter_name(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum CredentialSource {
    File {
        path: String,
    },
    Env {
        name: String,
    },
    IssueCommand {
        command: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        args: Vec<String>,
    },
    GithubApp {
        app_id_path: String,
        private_key_path: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CredentialLifecycle {
    Static,
    Refreshable,
    Issued,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialPlacementRequirements {
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub binaries: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct CredentialGrantSpec {
    pub selector: CredentialGrantSelector,
    pub credentials: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct CredentialGrantSelector {
    pub stance: Stance,
    #[builder(default)]
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub projects: BTreeSet<String>,
    #[builder(default)]
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub repositories: BTreeSet<RepositoryKey>,
}

impl CredentialGrantSelector {
    pub fn matches(&self, stance: Stance, project: Option<&str>, repositories: &BTreeSet<RepositoryKey>) -> bool {
        self.stance == stance
            && (self.projects.is_empty() || project.is_some_and(|project| self.projects.contains(project)))
            && (self.repositories.is_empty() || self.repositories.iter().any(|repository| repositories.contains(repository)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grant_matching_is_stance_first_and_refined_by_project_and_repository() {
        let repository = RepositoryKey("github.com-flotilla-org-flotilla".to_string());
        let selector = CredentialGrantSelector::builder()
            .stance(Stance::Contained)
            .projects(BTreeSet::from(["flotilla".to_string()]))
            .repositories(BTreeSet::from([repository.clone()]))
            .build();

        assert!(selector.matches(Stance::Contained, Some("flotilla"), &BTreeSet::from([repository.clone()])));
        assert!(!selector.matches(Stance::Trusted, Some("flotilla"), &BTreeSet::from([repository.clone()])));
        assert!(!selector.matches(Stance::Contained, Some("other"), &BTreeSet::from([repository.clone()])));
        assert!(!selector.matches(Stance::Contained, Some("flotilla"), &BTreeSet::new()));
    }

    #[test]
    fn claude_oauth_declares_its_account_identity_and_shares_the_claude_delivery_slot() {
        let consumer = CredentialConsumer::ClaudeOauth { account_email: "ops@example.com".to_string() };
        let encoded = serde_json::to_string(&consumer).expect("serialize consumer");
        assert_eq!(encoded, r#"{"adapter":"claude-oauth","account_email":"ops@example.com"}"#);
        assert_eq!(consumer.adapter_name(), "claude-oauth");
        assert_eq!(consumer.delivery_slot(), CredentialConsumer::Claude.delivery_slot());
    }

    #[test]
    fn declarations_and_grants_are_definitions_but_material_has_no_resource_field() {
        use crate::Resource;

        assert_eq!(CredentialSpec::REPLICATION_CLASS, ReplicationClass::Definitions);
        assert_eq!(CredentialGrant::REPLICATION_CLASS, ReplicationClass::Definitions);
        let encoded = serde_json::to_string(&CredentialSpecSpec {
            consumer: CredentialConsumer::Codex,
            source: CredentialSource::Env { name: "HOST_ONLY_KEY".to_string() },
            lifecycle: CredentialLifecycle::Static,
            placement: CredentialPlacementRequirements::default(),
        })
        .expect("serialize declaration");
        assert!(!encoded.contains("secret-material"));
        assert!(!encoded.contains("\"material\""));
    }
}
