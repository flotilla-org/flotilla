use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::{resource::define_resource, status_patch::NoStatusPatch, ReplicationClass, RepositoryKey};

pub const CREDENTIAL_REFS_ANNOTATION: &str = "flotilla.work/credential-refs";
pub const CREDENTIAL_REFS_ENV: &str = "FLOTILLA_CREDENTIAL_REFS";
pub const CREDENTIAL_REF_SESSION_TAG: &str = "flotilla-credential";
pub const CREDENTIAL_SCOPES_ANNOTATION: &str = "flotilla.work/credential-scopes";
pub const CREDENTIAL_SCOPES_ENV: &str = "FLOTILLA_CREDENTIAL_SCOPES";
pub const CREDENTIAL_SCOPES_SESSION_TAG: &str = "flotilla-credential-scopes";
pub const CREDENTIAL_PERMISSIONS_ANNOTATION: &str = "flotilla.work/credential-permissions";
pub const CREDENTIAL_PERMISSIONS_ENV: &str = "FLOTILLA_CREDENTIAL_PERMISSIONS";
pub const CREDENTIAL_PERMISSIONS_SESSION_TAG: &str = "flotilla-credential-permissions";

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
    GithubApp {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        installation_id: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        installation_repository: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        permissions: Option<BTreeMap<String, String>>,
    },
    Forgejo {
        forge_ref: String,
        username: String,
    },
    GitHttpToken {
        host: String,
        username: String,
    },
    Claude,
    ClaudeOauth {
        account_email: String,
    },
    Codex,
    DockerRegistry {
        registry: String,
        username: String,
    },
    ReviewBundleStore {
        endpoint: String,
        bucket: String,
        region: String,
        public_base_url: String,
        #[serde(default)]
        allow_http: bool,
        #[serde(default)]
        virtual_hosted_style: bool,
    },
}

impl CredentialConsumer {
    pub fn adapter_name(&self) -> &'static str {
        match self {
            Self::Gh => "gh",
            Self::GithubApp { .. } => "github-app",
            Self::Forgejo { .. } => "forgejo",
            Self::GitHttpToken { .. } => "git-http-token",
            Self::Claude => "claude",
            Self::ClaudeOauth { .. } => "claude-oauth",
            Self::Codex => "codex",
            Self::DockerRegistry { .. } => "docker-registry",
            Self::ReviewBundleStore { .. } => "review-bundle-store",
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

/// Validate GitHub's installation-token permission names and levels, then
/// narrow a grant's union to the declaration's maximum when one is present.
pub fn capped_github_app_permissions(
    requested: Option<&BTreeMap<String, String>>,
    declaration: Option<&BTreeMap<String, String>>,
) -> Result<Option<BTreeMap<String, String>>, String> {
    if let Some(declaration) = declaration {
        validate_github_app_permissions(declaration)?;
    }
    let Some(requested) = requested else { return Ok(declaration.cloned()) };
    validate_github_app_permissions(requested)?;
    Ok(Some(match declaration {
        None => requested.clone(),
        Some(cap) => requested
            .iter()
            .filter_map(|(name, level)| {
                cap.get(name).map(|maximum| {
                    let rank = permission_level_rank(level).expect("validated permission level");
                    let maximum_rank = permission_level_rank(maximum).expect("validated declaration level");
                    (name.clone(), if rank <= maximum_rank { level.clone() } else { maximum.clone() })
                })
            })
            .collect(),
    }))
}

pub fn permission_level_rank(level: &str) -> Result<u8, String> {
    match level {
        "read" => Ok(1),
        "write" => Ok(2),
        "admin" => Ok(3),
        _ => Err(format!("invalid GitHub App permission level `{level}`")),
    }
}

fn validate_github_app_permissions(permissions: &BTreeMap<String, String>) -> Result<(), String> {
    for (name, level) in permissions {
        let allowed = match name.as_str() {
            "actions"
            | "administration"
            | "artifact_metadata"
            | "attestations"
            | "checks"
            | "code_quality"
            | "codespaces"
            | "contents"
            | "dependabot_secrets"
            | "deployments"
            | "discussions"
            | "environments"
            | "issues"
            | "merge_queues"
            | "metadata"
            | "packages"
            | "pages"
            | "pull_requests"
            | "repository_custom_properties"
            | "repository_hooks"
            | "secret_scanning_alerts"
            | "secrets"
            | "security_events"
            | "single_file"
            | "statuses"
            | "vulnerability_alerts"
            | "custom_properties_for_organizations"
            | "members"
            | "organization_administration"
            | "organization_custom_roles"
            | "organization_custom_org_roles"
            | "organization_copilot_seat_management"
            | "organization_copilot_agent_settings"
            | "organization_announcement_banners"
            | "organization_hooks"
            | "organization_personal_access_tokens"
            | "organization_personal_access_token_requests"
            | "organization_packages"
            | "organization_secrets"
            | "organization_self_hosted_runners"
            | "organization_user_blocking"
            | "email_addresses"
            | "followers"
            | "git_ssh_keys"
            | "gpg_keys"
            | "interaction_limits"
            | "starring" => matches!(level.as_str(), "read" | "write"),
            "repository_projects"
            | "organization_custom_properties"
            | "organization_projects"
            | "enterprise_custom_properties_for_organizations" => matches!(level.as_str(), "read" | "write" | "admin"),
            "organization_events" | "organization_plan" => level == "read",
            "workflows" | "profile" => level == "write",
            _ => return Err(format!("invalid GitHub App permission name `{name}`")),
        };
        if !allowed {
            return Err(format!("invalid GitHub App permission level `{level}` for `{name}`"));
        }
    }
    Ok(())
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
    #[builder(default)]
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub permissions: BTreeMap<String, BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    #[builder(default)]
    pub landing_credentials: BTreeMap<String, LandingCredentialScope>,
}

/// Constraint retained when a human-gate approval releases a landing
/// credential. Temporal-only is the safe fallback where a forge cannot mint
/// a branch-constrained token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum LandingCredentialScope {
    Branch { repository: RepositoryKey, branch: String },
    TemporalOnly,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
#[serde(deny_unknown_fields)]
pub struct CredentialGrantSelector {
    #[builder(default)]
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub projects: BTreeSet<String>,
    #[builder(default)]
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub repositories: BTreeSet<RepositoryKey>,
    #[builder(default)]
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub roles: BTreeSet<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository_trust: Option<RepositoryTrust>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RepositoryTrust {
    Own,
    Fork,
}

impl CredentialGrantSelector {
    pub fn matches(&self, project: Option<&str>, repositories: &BTreeMap<RepositoryKey, RepositoryTrust>, role: &str) -> bool {
        (self.projects.is_empty() || project.is_some_and(|project| self.projects.contains(project)))
            && (self.roles.is_empty() || self.roles.contains(role))
            && (self.repositories.is_empty() || self.repositories.iter().any(|repository| repositories.contains_key(repository)))
            && self.repository_trust.is_none_or(|trust| {
                let selected = repositories.iter().filter(|(key, _)| self.repositories.is_empty() || self.repositories.contains(*key));
                let mut found = false;
                for (_, repository_trust) in selected {
                    found = true;
                    if *repository_trust != trust {
                        return false;
                    }
                }
                found
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grant_matching_uses_role_project_repository_and_trust() {
        let repository = RepositoryKey("github.com-flotilla-org-flotilla".to_string());
        let selector = CredentialGrantSelector::builder()
            .projects(BTreeSet::from(["flotilla".to_string()]))
            .repositories(BTreeSet::from([repository.clone()]))
            .roles(BTreeSet::from(["coder".to_string()]))
            .repository_trust(RepositoryTrust::Own)
            .build();
        let own = BTreeMap::from([(repository.clone(), RepositoryTrust::Own)]);
        let fork = BTreeMap::from([(repository, RepositoryTrust::Fork)]);
        assert!(selector.matches(Some("flotilla"), &own, "coder"));
        assert!(!selector.matches(Some("flotilla"), &own, "reviewer"));
        assert!(!selector.matches(Some("flotilla"), &fork, "coder"));
        assert!(!selector.matches(Some("other"), &own, "coder"));
        assert!(!selector.matches(Some("flotilla"), &BTreeMap::new(), "coder"));
    }

    #[test]
    fn obsolete_stance_selector_is_rejected() {
        let error = serde_json::from_str::<CredentialGrantSelector>(r#"{"stance":"contained","projects":["flotilla"]}"#)
            .expect_err("obsolete selector must not broaden a grant");
        assert!(error.to_string().contains("unknown field `stance`"));
    }

    #[test]
    fn grant_permissions_are_capped_and_invalid_values_fail() {
        let requested = BTreeMap::from([("contents".to_string(), "write".to_string()), ("issues".to_string(), "read".to_string())]);
        let cap = BTreeMap::from([("contents".to_string(), "read".to_string())]);
        assert_eq!(capped_github_app_permissions(Some(&requested), Some(&cap)).expect("cap"), Some(cap.clone()));
        assert_eq!(capped_github_app_permissions(None, Some(&cap)).expect("unchanged declaration"), Some(cap));
        assert_eq!(
            capped_github_app_permissions(Some(&BTreeMap::from([("invented".to_string(), "read".to_string())])), None),
            Err("invalid GitHub App permission name `invented`".to_string())
        );
        assert!(capped_github_app_permissions(Some(&BTreeMap::from([("issues".to_string(), "owner".to_string())])), None)
            .expect_err("invalid level")
            .contains("invalid GitHub App permission level"));
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
    fn git_http_token_declares_a_forge_agnostic_host_and_username() {
        let consumer =
            CredentialConsumer::GitHttpToken { host: "forgejo.lab.flotilla.work".to_string(), username: "crew-reader".to_string() };

        let encoded = serde_json::to_string(&consumer).expect("serialize consumer");

        assert_eq!(encoded, r#"{"adapter":"git-http-token","host":"forgejo.lab.flotilla.work","username":"crew-reader"}"#);
        assert_eq!(consumer.adapter_name(), "git-http-token");
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
