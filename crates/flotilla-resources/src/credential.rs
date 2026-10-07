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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CredentialGrant;
impl crate::Resource for CredentialGrant {
    type Spec = CredentialGrantSpec;
    type Status = ();
    type StatusPatch = NoStatusPatch;
    const API_PATHS: crate::ApiPaths =
        crate::ApiPaths { group: "flotilla.work", version: "v1", plural: "credentialgrants", kind: "CredentialGrant" };
    const REPLICATION_CLASS: ReplicationClass = ReplicationClass::Definitions;
    fn validate_spec(_meta: &crate::InputMeta, spec: &Self::Spec) -> Result<(), crate::ResourceError> {
        let selector = &spec.selector;
        if selector.host_action.is_some()
            && (!selector.projects.is_empty()
                || !selector.repositories.is_empty()
                || !selector.roles.is_empty()
                || selector.repository_trust.is_some())
        {
            return Err(crate::ResourceError::invalid("host-action and work credential selectors are mutually exclusive"));
        }
        if selector.host_action.is_some() && !spec.landing_credentials.is_empty() {
            return Err(crate::ResourceError::invalid("host-action grants cannot contain landing credentials"));
        }
        Ok(())
    }
}

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
        /// REST App bot login, including `[bot]`. Old declarations decode without it
        /// for one fleet generation; update project-map credential manifests in this roll.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        actor_login: Option<String>,
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
    pub fn github_actor_login(&self) -> Option<&str> {
        match self {
            Self::GithubApp { actor_login, .. } => actor_login.as_deref(),
            _ => None,
        }
    }

    /// GraphQL reports an App actor by its slug, without REST's `[bot]` suffix.
    pub fn github_graphql_actor_login(&self) -> Option<&str> {
        self.github_actor_login().map(|login| login.strip_suffix("[bot]").unwrap_or(login))
    }

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

/// Refuse ambiguous permission composition among grants already selected for one vessel.
/// Presence of a credential key (even an empty permission set) makes a grant explicit.
pub fn validate_matching_grant_permissions<'a>(grants: impl IntoIterator<Item = (&'a str, &'a CredentialGrantSpec)>) -> Result<(), String> {
    let mut modes = BTreeMap::<&str, (Option<&str>, Option<&str>)>::new();
    for (name, grant) in grants {
        for credential in &grant.credentials {
            let (listed, unlisted) = modes.entry(credential).or_default();
            if grant.permissions.contains_key(credential) {
                listed.get_or_insert(name);
            } else {
                unlisted.get_or_insert(name);
            }
        }
    }
    let errors = modes.into_iter().filter_map(|(credential, (listed, unlisted))| {
        let (Some(listed), Some(unlisted)) = (listed, unlisted) else { return None };
        Some(format!(
            "credential `{credential}` mixes permissions listed by grant `{listed}` with unlisted permissions in grant `{unlisted}`; make grant `{unlisted}` explicit for credential `{credential}`"
        ))
    }).collect::<Vec<_>>();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("\n"))
    }
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
    /// Host actions never match work. ADR 0047: retain this default for one roll.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_action: Option<HostActionSelector>,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HostImageAction {
    ImagePull,
    ImagePush,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
#[serde(deny_unknown_fields)]
pub struct HostActionSelector {
    pub action: HostImageAction,
    /// Empty selects all eligible hosts; push additionally requires declared build capacity.
    #[builder(default)]
    #[serde(default)]
    pub hosts: BTreeSet<String>,
}

impl CredentialGrantSelector {
    pub fn matches_host_action(&self, host: &str, action: HostImageAction, declared_builder: bool) -> bool {
        self.projects.is_empty()
            && self.repositories.is_empty()
            && self.roles.is_empty()
            && self.repository_trust.is_none()
            && self.host_action.as_ref().is_some_and(|selector| {
                selector.action == action
                    && (selector.hosts.is_empty() || selector.hosts.contains(host))
                    && (action != HostImageAction::ImagePush || declared_builder)
            })
    }

    /// Whether both selectors can match the same work, independent of installed workflows.
    /// Repository selectors are existential, so disjoint sets can match a multi-repository vessel.
    pub fn overlaps(&self, other: &Self) -> bool {
        if self.host_action.is_some() || other.host_action.is_some() {
            return match (&self.host_action, &other.host_action) {
                (Some(left), Some(right)) => {
                    left.action == right.action
                        && (left.hosts.is_empty() || right.hosts.is_empty() || !left.hosts.is_disjoint(&right.hosts))
                }
                _ => false,
            };
        }
        fn compatible<T: Ord>(left: &BTreeSet<T>, right: &BTreeSet<T>) -> bool {
            left.is_empty() || right.is_empty() || !left.is_disjoint(right)
        }
        if !compatible(&self.projects, &other.projects) || !compatible(&self.roles, &other.roles) {
            return false;
        }
        let project = self
            .projects
            .intersection(&other.projects)
            .next()
            .or_else(|| self.projects.iter().next().filter(|_| other.projects.is_empty()))
            .or_else(|| other.projects.iter().next().filter(|_| self.projects.is_empty()));
        let role = self
            .roles
            .intersection(&other.roles)
            .next()
            .or_else(|| self.roles.iter().next().filter(|_| other.roles.is_empty()))
            .or_else(|| other.roles.iter().next().filter(|_| self.roles.is_empty()))
            .map(String::as_str)
            .unwrap_or("");
        let mut keys = self.repositories.union(&other.repositories).cloned().collect::<BTreeSet<_>>();
        // One fresh representative covers repositories outside either selector.
        let mut fresh = RepositoryKey("credential-selector-witness".into());
        while keys.contains(&fresh) {
            fresh.0.push('_');
        }
        keys.insert(fresh);
        let matches = |repositories: &BTreeMap<RepositoryKey, RepositoryTrust>| {
            self.matches(project.map(String::as_str), repositories, role) && other.matches(project.map(String::as_str), repositories, role)
        };
        if matches(&BTreeMap::new()) {
            return true;
        }
        // At most one witness per selector suffices: removing other repositories
        // preserves existential membership and uniform trust constraints.
        for left in &keys {
            for left_trust in [RepositoryTrust::Own, RepositoryTrust::Fork] {
                let mut repositories = BTreeMap::from([(left.clone(), left_trust)]);
                if matches(&repositories) {
                    return true;
                }
                for right in keys.iter().filter(|right| *right > left) {
                    for right_trust in [RepositoryTrust::Own, RepositoryTrust::Fork] {
                        repositories.insert(right.clone(), right_trust);
                        if matches(&repositories) {
                            return true;
                        }
                        repositories.remove(right);
                    }
                }
            }
        }
        false
    }

    pub fn matches(&self, project: Option<&str>, repositories: &BTreeMap<RepositoryKey, RepositoryTrust>, role: &str) -> bool {
        self.host_action.is_none()
            && (self.projects.is_empty() || project.is_some_and(|project| self.projects.contains(project)))
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

    // #2491: even an explicitly empty map conflicts with an omitted map;
    // refusal identifies both grants and the credential in either order.
    #[test]
    fn empty_explicit_permissions_still_refuse_unlisted_grants() {
        let unlisted = CredentialGrantSpec::builder()
            .selector(CredentialGrantSelector::builder().build())
            .credentials(BTreeSet::from(["app".into()]))
            .build();
        let listed = CredentialGrantSpec { permissions: BTreeMap::from([("app".into(), BTreeMap::new())]), ..unlisted.clone() };
        for grants in [[("base", &unlisted), ("explicit", &listed)], [("explicit", &listed), ("base", &unlisted)]] {
            let error = validate_matching_grant_permissions(grants).expect_err("empty map is explicit");
            for name in ["app", "base", "explicit", "make grant `base` explicit"] {
                assert!(error.contains(name), "{error}");
            }
        }
    }

    // Repository sets need not intersect for grants to select the same vessel;
    // opposite trust constraints can coexist only on separately selected repositories.
    #[test]
    fn selector_overlap_handles_multi_repository_work_and_trust() {
        let own = CredentialGrantSelector::builder()
            .repositories(BTreeSet::from([RepositoryKey("a".into())]))
            .repository_trust(RepositoryTrust::Own)
            .build();
        let fork = CredentialGrantSelector::builder()
            .repositories(BTreeSet::from([RepositoryKey("b".into())]))
            .repository_trust(RepositoryTrust::Fork)
            .build();
        assert!(own.overlaps(&fork));
        assert!(!own.overlaps(&CredentialGrantSelector { repositories: own.repositories.clone(), ..fork.clone() }));
        assert!(!own.overlaps(&CredentialGrantSelector { repositories: BTreeSet::new(), ..fork }));
    }

    // Owner ruling #2491: composition is per credential; an empty listed map is
    // explicit, and order or duplicate grants must not affect refusal.
    #[hegel::test]
    fn grant_permission_composition_is_order_independent(tc: hegel::TestCase) {
        use hegel::generators as gs;
        // Cover zero through four grants, listed/unlisted, empty/nonempty maps,
        // distinct credentials, duplicates, and both input orders.
        let count = tc.draw(gs::integers::<usize>().min_value(0).max_value(4));
        let grants = (0..count)
            .map(|index| {
                let credential = if tc.draw(gs::booleans()) { "app" } else { "other" };
                let listed = tc.draw(gs::booleans());
                let permissions =
                    if tc.draw(gs::booleans()) { BTreeMap::new() } else { BTreeMap::from([("contents".into(), "read".into())]) };
                (
                    format!("grant-{index}"),
                    CredentialGrantSpec::builder()
                        .selector(CredentialGrantSelector::builder().build())
                        .credentials(BTreeSet::from([credential.into()]))
                        .permissions(if listed { BTreeMap::from([(credential.into(), permissions)]) } else { BTreeMap::new() })
                        .build(),
                )
            })
            .collect::<Vec<_>>();
        let mixed = ["app", "other"].iter().any(|credential| {
            let selected = grants.iter().filter(|(_, grant)| grant.credentials.contains(*credential)).collect::<Vec<_>>();
            selected.iter().any(|(_, grant)| grant.permissions.contains_key(*credential))
                && selected.iter().any(|(_, grant)| !grant.permissions.contains_key(*credential))
        });
        let check = || validate_matching_grant_permissions(grants.iter().map(|(name, grant)| (name.as_str(), grant)));
        assert_eq!(check().is_err(), mixed);
        assert_eq!(validate_matching_grant_permissions(grants.iter().rev().map(|(name, grant)| (name.as_str(), grant))).is_err(), mixed);
        assert_eq!(
            validate_matching_grant_permissions(grants.iter().chain(&grants).map(|(name, grant)| (name.as_str(), grant))).is_err(),
            mixed
        );
    }

    // The manifest gate's potential co-selection must agree with matching over
    // all small work contexts, including disjoint repository selectors.
    #[hegel::test]
    fn selector_overlap_matches_exhaustive_work_contexts(tc: hegel::TestCase) {
        use hegel::generators as gs;
        // Each selector dimension covers wildcard, singleton, and two values;
        // trust spans unconstrained, own, and fork.
        let make = || {
            let mut selector = CredentialGrantSelector::builder().build();
            for value in ["a", "b"] {
                if tc.draw(gs::booleans()) {
                    selector.projects.insert(value.into());
                }
                if tc.draw(gs::booleans()) {
                    selector.roles.insert(value.into());
                }
                if tc.draw(gs::booleans()) {
                    selector.repositories.insert(RepositoryKey(value.into()));
                }
            }
            selector.repository_trust = match tc.draw(gs::integers::<usize>().min_value(0).max_value(2)) {
                0 => None,
                1 => Some(RepositoryTrust::Own),
                _ => Some(RepositoryTrust::Fork),
            };
            selector
        };
        let left = make();
        let right = make();
        let mut expected = false;
        for project in [None, Some("a"), Some("b"), Some("other")] {
            for role in ["", "a", "b", "other"] {
                // Exhaust every absent/own/fork assignment, not just witness pairs.
                for assignment in 0..27 {
                    let mut assignment = assignment;
                    let mut repositories = BTreeMap::new();
                    for key in ["a", "b", "other"] {
                        match assignment % 3 {
                            1 => {
                                repositories.insert(RepositoryKey(key.into()), RepositoryTrust::Own);
                            }
                            2 => {
                                repositories.insert(RepositoryKey(key.into()), RepositoryTrust::Fork);
                            }
                            _ => {}
                        }
                        assignment /= 3;
                    }
                    expected |= left.matches(project, &repositories, role) && right.matches(project, &repositories, role);
                }
            }
        }
        assert_eq!(left.overlaps(&right), expected, "{left:?} / {right:?}");
        assert_eq!(right.overlaps(&left), expected);
    }

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
    fn github_app_actor_login_decodes_old_declarations_and_configured_identity() {
        let old: CredentialConsumer =
            serde_json::from_str(r#"{"adapter":"github-app","installation_id":42}"#).expect("old App declaration");
        assert_eq!(old.github_actor_login(), None);
        let configured: CredentialConsumer =
            serde_json::from_str(r#"{"adapter":"github-app","installation_id":42,"actor_login":"crew-app[bot]"}"#)
                .expect("configured App declaration");
        assert_eq!(configured.github_actor_login(), Some("crew-app[bot]"));
        assert_eq!(configured.github_graphql_actor_login(), Some("crew-app"));
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

#[cfg(test)]
mod host_action_tests {
    use super::*;

    // #2729: host actions cannot leak to crews. Generate both actions, explicit
    // and wildcard host sets, builder/vessel hosts, and wrong host identities.
    #[hegel::test]
    fn host_grants_are_disjoint_from_work(tc: hegel::TestCase) {
        let push = tc.draw(hegel::generators::booleans());
        let builder = tc.draw(hegel::generators::booleans());
        let wildcard = tc.draw(hegel::generators::booleans());
        let matching_host = tc.draw(hegel::generators::booleans());
        let action = if push { HostImageAction::ImagePush } else { HostImageAction::ImagePull };
        let selector = CredentialGrantSelector::builder()
            .host_action(
                HostActionSelector::builder()
                    .action(action)
                    .hosts(if wildcard { BTreeSet::new() } else { BTreeSet::from(["builder".into()]) })
                    .build(),
            )
            .build();
        let host = if matching_host { "builder" } else { "other" };
        assert_eq!(selector.matches_host_action(host, action, builder), (wildcard || matching_host) && (!push || builder));
        assert!(!selector.matches(Some("fleet"), &BTreeMap::new(), "coder"));
        assert!(!selector.overlaps(&CredentialGrantSelector::builder().build()));
        let other = if push { HostImageAction::ImagePull } else { HostImageAction::ImagePush };
        assert!(!selector.matches_host_action(host, other, true));
    }

    // Previous-generation work grants omit host_action and retain their behavior.
    #[test]
    fn old_work_selector_decodes_without_host_actions() {
        let selector: CredentialGrantSelector = serde_json::from_str("{}").expect("old selector");
        assert!(selector.matches(None, &BTreeMap::new(), "coder"));
        assert!(!selector.matches_host_action("builder", HostImageAction::ImagePush, true));
    }
}
