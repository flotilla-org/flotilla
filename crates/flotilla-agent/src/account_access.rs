//! Account access for agent items (#3907): what an item declares it needs, and the registered
//! implementations that supply that access to a requester.
//!
//! - An item declares its account requirements in its own registration ([`ItemDeclaration`]),
//!   naming the access in its provider family's vocabulary. Core never switches on item ids.
//! - A provider family registers an [`AccountProvisioner`]: which declared credentials supply its
//!   access, the plan (or refusal) it returns per candidate environment kind (#3881), and the
//!   delivery that carries out the plan at a place.
//!
//! Selection, bindings and their records are core's; the access description, mechanism and
//! delivery are the family's.

use std::{path::Path, sync::Arc};

use async_trait::async_trait;
use flotilla_protocol::arg::{flatten, Arg};
use flotilla_provider_runtime::{ChannelLabel, CommandRunner};
use flotilla_resources::{
    account_access::{AccessName, ProvisionPlan, UpdateMode},
    CredentialConsumer, EnvironmentSpec, FulfilmentRealisation, PlacementPolicySpec,
};

/// An item's registered account requirement (#3879 "Requirement, not a switch").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ItemDeclaration {
    /// The item id: the agent adapter that declared the requirement.
    pub item: &'static str,
    /// The access the requirement names, in its family's vocabulary.
    pub account_access: &'static str,
}

/// The Claude subscription access (#3884).
pub const CLAUDE_SUBSCRIPTION: &str = "claude:subscription";

/// The claude-code item: its harness runs on a Claude subscription.
pub const CLAUDE_CODE_ITEM: ItemDeclaration = ItemDeclaration { item: "claude-code", account_access: CLAUDE_SUBSCRIPTION };

/// Every registered item that declares account access. Items absent here (Codex in this slice)
/// keep today's declared-credential pipeline.
pub const REGISTERED_ITEMS: &[ItemDeclaration] = &[CLAUDE_CODE_ITEM];

/// The account access the item `item` declares, if it declares any.
pub fn declared_account_access(item: &str) -> Option<AccessName> {
    REGISTERED_ITEMS
        .iter()
        .find(|declaration| declaration.item == item)
        .map(|declaration| AccessName(declaration.account_access.to_string()))
}

/// What a declared credential supplies for a family's access: the account it belongs to, as the
/// declaration states it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuppliedAccess {
    pub account: Option<String>,
}

/// The candidate environment kind a plan is resolved against: one vocabulary for admission's
/// placement candidates and a launch's place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvironmentKind {
    /// A container vessel; its vessel agent writes into it (#3756).
    Docker,
    /// A bare host reached over SSH through its vessel agent (#3792).
    AgentlessSsh,
    /// The daemon's own host, written by the daemon.
    HostDirect,
    /// A provider-hosted sandbox.
    CloudSandbox,
}

impl EnvironmentKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Docker => "docker",
            Self::AgentlessSsh => "agentless-ssh",
            Self::HostDirect => "host-direct",
            Self::CloudSandbox => "cloud-sandbox",
        }
    }

    /// A fulfilment kind's realisation, on a host that is or is not agentless.
    pub fn of_realisation(realisation: &FulfilmentRealisation, agentless_host: bool) -> Self {
        match realisation {
            FulfilmentRealisation::CloudSandboxPerVessel { .. } => Self::CloudSandbox,
            FulfilmentRealisation::DockerPerVessel { .. } => Self::Docker,
            FulfilmentRealisation::HostDirect if agentless_host => Self::AgentlessSsh,
            FulfilmentRealisation::HostDirect => Self::HostDirect,
        }
    }

    /// A placement policy's realisation.
    pub fn of_policy(spec: &PlacementPolicySpec, agentless_host: bool) -> Self {
        if spec.cloud_sandbox_per_vessel.is_some() {
            Self::CloudSandbox
        } else if spec.docker_per_vessel.is_some() {
            Self::Docker
        } else if agentless_host {
            Self::AgentlessSsh
        } else {
            Self::HostDirect
        }
    }

    /// A launch's place: its Environment record, if readable here. An unknown shape gets no
    /// kind, so no plan; an unreadable record is a provisioned container vessel.
    pub fn of_environment(environment: Option<&EnvironmentSpec>) -> Result<Self, String> {
        match environment {
            Some(spec) if spec.cloud_sandbox.is_some() => Ok(Self::CloudSandbox),
            Some(spec) if spec.host_direct.is_some() => Ok(Self::HostDirect),
            Some(spec) if spec.docker.is_some() => Ok(Self::Docker),
            Some(_) => Err("the environment has a shape no account plan knows".to_string()),
            None => Ok(Self::Docker),
        }
    }
}

/// A registered implementation that supplies one family's access (#3881).
#[async_trait]
pub trait AccountProvisioner: Send + Sync {
    /// The implementation's name, recorded on every plan it makes.
    fn implementation(&self) -> &'static str;
    /// The access it supplies.
    fn access(&self) -> &'static str;
    /// Whether a declared credential supplies this access, and for which account.
    fn supplies(&self, consumer: &CredentialConsumer) -> Option<SuppliedAccess>;
    /// The plan for an environment kind, or the reason there is none.
    fn plan(&self, kind: EnvironmentKind) -> Result<ProvisionPlan, String>;
    /// The session variables that point the harness at its durable `home` and at the material
    /// delivered into the material directory `dir` (#4012). Never the material itself.
    fn environment(&self, home: &str, dir: &str) -> Vec<(String, String)>;
    /// Write the material into the requester's material directory at a place.
    async fn deliver(&self, dir: &str, material: &str, runner: &dyn CommandRunner) -> Result<(), String>;
    /// Delete the delivered copy from the requester's material directory at a place.
    async fn remove(&self, dir: &str, runner: &dyn CommandRunner) -> Result<(), String>;
}

/// The registered account provisioners.
#[derive(Clone)]
pub struct AccountProvisioners {
    implementations: Vec<Arc<dyn AccountProvisioner>>,
}

impl AccountProvisioners {
    pub fn seeded() -> Self {
        Self { implementations: vec![Arc::new(ClaudeDirect)] }
    }

    /// The implementation supplying `access`.
    pub fn for_access(&self, access: &AccessName) -> Option<&Arc<dyn AccountProvisioner>> {
        self.implementations.iter().find(|implementation| implementation.access() == access.0)
    }

    /// The implementation that made a recorded plan.
    pub fn for_implementation(&self, implementation: &str) -> Option<&Arc<dyn AccountProvisioner>> {
        self.implementations.iter().find(|candidate| candidate.implementation() == implementation)
    }

    /// Whether any registered family's access is supplied by this credential. Such credentials
    /// reach a requester only through its binding, never as vessel-wide credential references.
    pub fn supplies_any(&self, consumer: &CredentialConsumer) -> bool {
        self.implementations.iter().any(|implementation| implementation.supplies(consumer).is_some())
    }
}

impl Default for AccountProvisioners {
    fn default() -> Self {
        Self::seeded()
    }
}

impl std::fmt::Debug for AccountProvisioners {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.implementations.iter().map(|implementation| implementation.implementation())).finish()
    }
}

/// The variable naming the requester's delivered token file. The Claude launch reads the token
/// from it when the harness starts; the token itself never enters the session's environment.
pub const CLAUDE_OAUTH_TOKEN_FILE_ENV: &str = "FLOTILLA_CLAUDE_OAUTH_TOKEN_FILE";

/// The token file's name inside the requester's material directory.
pub const CLAUDE_OAUTH_TOKEN_FILE: &str = ".flotilla-oauth-token";

/// Claude subscription access by direct delivery (#3884): the vessel agent writes the account's
/// OAuth token into the requester's material directory, and the harness runs in its explicit
/// `CLAUDE_CONFIG_DIR` home. Claude reads the token from the file its launch names rather than
/// from its home, so the token stays on the place's runtime base while the home is durable (#4012).
#[derive(Debug, Clone, Copy, Default)]
pub struct ClaudeDirect;

impl ClaudeDirect {
    pub const IMPLEMENTATION: &'static str = "claude-direct";
    pub const MECHANISM: &'static str = "oauth-token-file:CLAUDE_CONFIG_DIR";
}

#[async_trait]
impl AccountProvisioner for ClaudeDirect {
    fn implementation(&self) -> &'static str {
        Self::IMPLEMENTATION
    }

    fn access(&self) -> &'static str {
        CLAUDE_SUBSCRIPTION
    }

    fn supplies(&self, consumer: &CredentialConsumer) -> Option<SuppliedAccess> {
        // The family's own knowledge: a subscription OAuth declaration supplies the
        // subscription; an API key is a different access (#3878).
        match consumer {
            CredentialConsumer::ClaudeOauth { account_email } => Some(SuppliedAccess { account: Some(account_email.clone()) }),
            _ => None,
        }
    }

    fn plan(&self, kind: EnvironmentKind) -> Result<ProvisionPlan, String> {
        // Keep this strict: it is the only guard against a wrong-kind placement, and the launch
        // side treats a vessel whose Environment record it cannot read as Docker.
        if kind == EnvironmentKind::CloudSandbox {
            return Err(format!(
                "{} has no plan for {} environments: no agent writes the requester's home there",
                Self::IMPLEMENTATION,
                kind.as_str()
            ));
        }
        Ok(ProvisionPlan {
            implementation: Self::IMPLEMENTATION.to_string(),
            effective_access: "account".to_string(),
            mechanism: Self::MECHANISM.to_string(),
            update_mode: UpdateMode::Push,
        })
    }

    fn environment(&self, home: &str, dir: &str) -> Vec<(String, String)> {
        vec![
            ("CLAUDE_CONFIG_DIR".to_string(), home.to_string()),
            (CLAUDE_OAUTH_TOKEN_FILE_ENV.to_string(), Path::new(dir).join(CLAUDE_OAUTH_TOKEN_FILE).display().to_string()),
        ]
    }

    async fn deliver(&self, dir: &str, material: &str, runner: &dyn CommandRunner) -> Result<(), String> {
        if !Path::new(dir).is_absolute() {
            return Err(format!("requester material directory `{dir}` is not absolute"));
        }
        let script = "umask 077 && mkdir -p \"$1\" && chmod 700 \"$1\"";
        runner.run("sh", &["-c", script, "flotilla-requester-home", dir], Path::new("/"), &ChannelLabel::Default).await?;
        let token_file = Path::new(dir).join(CLAUDE_OAUTH_TOKEN_FILE);
        runner.write_file_with_mode(&token_file, material, 0o600).await
    }

    async fn remove(&self, dir: &str, runner: &dyn CommandRunner) -> Result<(), String> {
        let token_file = Path::new(dir).join(CLAUDE_OAUTH_TOKEN_FILE).display().to_string();
        runner.run("rm", &["-f", "--", &token_file], Path::new("/"), &ChannelLabel::Default).await.map(|_| ())
    }
}

/// The shell prefix that hands the delivered token to the Claude process as it starts.
pub(crate) fn claude_token_prefix(token_file: &str) -> String {
    format!("CLAUDE_CODE_OAUTH_TOKEN=\"$(cat -- {})\"", flatten(&[Arg::Quoted(token_file.to_string())], 0))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Behaviour (#3879): only the subscription declaration supplies the subscription; the item's
    // requirement comes from its registration, and Codex declares none in this slice.
    #[test]
    fn the_claude_item_declares_the_subscription_that_oauth_declarations_supply() {
        assert_eq!(declared_account_access("claude-code"), Some(AccessName(CLAUDE_SUBSCRIPTION.into())));
        assert_eq!(declared_account_access("codex"), None);
        let oauth = CredentialConsumer::ClaudeOauth { account_email: "box@example.com".into() };
        assert_eq!(ClaudeDirect.supplies(&oauth), Some(SuppliedAccess { account: Some("box@example.com".into()) }));
        assert_eq!(ClaudeDirect.supplies(&CredentialConsumer::Claude), None);
        assert!(AccountProvisioners::seeded().supplies_any(&oauth));
        assert!(!AccountProvisioners::seeded().supplies_any(&CredentialConsumer::Codex));
    }

    // Review #3910: requesters whose names differ only in characters a path cannot carry get
    // different homes; plain names are kept as they are.
    #[test]
    fn requester_homes_never_collide() {
        let base = Path::new("/base");
        let home = |convoy: &str, role: &str| {
            flotilla_resources::account_access::requester_home(base, convoy, role, "claude").display().to_string()
        };
        assert_eq!(home("convoy-1", "coder"), "/base/agent-homes/requesters/convoy-1/coder/claude");
        let homes = [home("a.b", "coder"), home("a-b", "coder"), home("a/b", "coder"), home("a-b", "x.y"), home("a-b", "x-y")];
        assert_eq!(homes.iter().collect::<std::collections::BTreeSet<_>>().len(), homes.len(), "{homes:?}");
        assert!(homes.iter().all(|home| !home.contains("..") && home.matches('/').count() == 6), "{homes:?}");
    }
}
