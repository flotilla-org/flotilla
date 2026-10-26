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
    CredentialConsumer,
};

/// An item's registered account requirements (#3879 "Requirement, not a switch").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ItemDeclaration {
    /// The item id: the agent adapter that declared the requirement.
    pub item: &'static str,
    /// The access each requirement names, in its family's vocabulary.
    pub account_access: &'static [&'static str],
}

/// The Claude subscription access (#3884).
pub const CLAUDE_SUBSCRIPTION: &str = "claude:subscription";

/// The claude-code item: its harness runs on a Claude subscription.
pub const CLAUDE_CODE_ITEM: ItemDeclaration = ItemDeclaration { item: "claude-code", account_access: &[CLAUDE_SUBSCRIPTION] };

/// Every registered item that declares account access. Items absent here (Codex in this slice)
/// keep today's declared-credential pipeline.
pub const REGISTERED_ITEMS: &[ItemDeclaration] = &[CLAUDE_CODE_ITEM];

/// The account access the item `item` declares, if it declares any.
pub fn declared_account_access(item: &str) -> Option<AccessName> {
    REGISTERED_ITEMS
        .iter()
        .find(|declaration| declaration.item == item)
        .and_then(|declaration| declaration.account_access.first())
        .map(|access| AccessName((*access).to_string()))
}

/// What a declared credential supplies for a family's access: the account it belongs to, as the
/// declaration states it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuppliedAccess {
    pub account: Option<String>,
}

/// Environment kinds a plan is resolved against, as admission names its placement candidates.
pub mod environment_kind {
    /// A container vessel; its vessel agent writes into it (#3756).
    pub const DOCKER: &str = "docker";
    /// A bare host reached over SSH through its vessel agent (#3792).
    pub const AGENTLESS_SSH: &str = "agentless-ssh";
    /// The daemon's own host, written by the daemon.
    pub const HOST_DIRECT: &str = "host-direct";
    /// A provider-hosted sandbox.
    pub const CLOUD_SANDBOX: &str = "cloud-sandbox";
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
    fn plan(&self, environment_kind: &str) -> Result<ProvisionPlan, String>;
    /// The requester's home under a place's writable base.
    fn home(&self, writable_base: &Path, convoy: &str, role: &str) -> String;
    /// Write the material into the home at a place; returns the session variables that point
    /// the harness at it. Never returns the material itself.
    async fn deliver(&self, home: &str, material: &str, runner: &dyn CommandRunner) -> Result<Vec<(String, String)>, String>;
    /// Delete the delivered copy from the home at a place.
    async fn remove(&self, home: &str, runner: &dyn CommandRunner) -> Result<(), String>;
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

    pub fn new(implementations: Vec<Arc<dyn AccountProvisioner>>) -> Self {
        Self { implementations }
    }

    /// The implementation supplying `access`.
    pub fn for_access(&self, access: &AccessName) -> Option<&Arc<dyn AccountProvisioner>> {
        self.implementations.iter().find(|implementation| implementation.access() == access.0)
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

/// The token file's name inside the requester's home.
pub const CLAUDE_OAUTH_TOKEN_FILE: &str = ".flotilla-oauth-token";

/// Claude subscription access by direct delivery (#3884): the vessel agent writes the account's
/// OAuth token into the requester's explicit `CLAUDE_CONFIG_DIR` home.
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

    fn plan(&self, environment_kind: &str) -> Result<ProvisionPlan, String> {
        use environment_kind::{AGENTLESS_SSH, DOCKER, HOST_DIRECT};
        if ![DOCKER, AGENTLESS_SSH, HOST_DIRECT].contains(&environment_kind) {
            return Err(format!(
                "{} has no plan for {environment_kind} environments: no agent writes the requester's home there",
                Self::IMPLEMENTATION
            ));
        }
        Ok(ProvisionPlan {
            implementation: Self::IMPLEMENTATION.to_string(),
            effective_access: "account".to_string(),
            mechanism: Self::MECHANISM.to_string(),
            update_mode: UpdateMode::Push,
        })
    }

    fn home(&self, writable_base: &Path, convoy: &str, role: &str) -> String {
        writable_base.join("agent-homes").join("requesters").join(convoy).join(role).join("claude").display().to_string()
    }

    async fn deliver(&self, home: &str, material: &str, runner: &dyn CommandRunner) -> Result<Vec<(String, String)>, String> {
        if !Path::new(home).is_absolute() {
            return Err(format!("requester home `{home}` is not absolute"));
        }
        let script = "umask 077 && mkdir -p \"$1\" && chmod 700 \"$1\"";
        runner.run("sh", &["-c", script, "flotilla-requester-home", home], Path::new("/"), &ChannelLabel::Default).await?;
        let token_file = Path::new(home).join(CLAUDE_OAUTH_TOKEN_FILE);
        runner.write_file_with_mode(&token_file, material, 0o600).await?;
        Ok(vec![
            ("CLAUDE_CONFIG_DIR".to_string(), home.to_string()),
            (CLAUDE_OAUTH_TOKEN_FILE_ENV.to_string(), token_file.display().to_string()),
        ])
    }

    async fn remove(&self, home: &str, runner: &dyn CommandRunner) -> Result<(), String> {
        let token_file = Path::new(home).join(CLAUDE_OAUTH_TOKEN_FILE).display().to_string();
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

    // Behaviour (#3881, #3907 contract 3): one implementation plans for Docker, agent-backed and
    // host-direct vessels alike, and refuses a kind no agent writes into, with the reason.
    #[test]
    fn claude_direct_plans_where_an_agent_writes_the_home_and_refuses_elsewhere() {
        for kind in [environment_kind::DOCKER, environment_kind::AGENTLESS_SSH, environment_kind::HOST_DIRECT] {
            let plan = ClaudeDirect.plan(kind).expect("plan");
            assert_eq!((plan.effective_access.as_str(), plan.update_mode), ("account", UpdateMode::Push));
        }
        assert_eq!(
            ClaudeDirect.plan(environment_kind::CLOUD_SANDBOX).expect_err("no plan"),
            "claude-direct has no plan for cloud-sandbox environments: no agent writes the requester's home there"
        );
    }

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

    #[test]
    fn the_token_prefix_quotes_the_file() {
        assert_eq!(claude_token_prefix("/b/it's"), "CLAUDE_CODE_OAUTH_TOKEN=\"$(cat -- '/b/it'\\''s')\"");
    }
}
