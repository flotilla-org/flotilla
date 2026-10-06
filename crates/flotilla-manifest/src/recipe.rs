//! Platform-independent action addresses and viewer-specific direct transport reachability.
//! Viewers resolve addresses on their own platform; only genuine commands carry argv.

use std::collections::BTreeMap;

use flotilla_protocol::{HostName, ViewAddress};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DirectTransport {
    Local,
    Ssh(String),
}

/// A structured action recipe. Kind is an open vocabulary; argv is present
/// only for genuine commands. The target is also the viewer's focus-if-live key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recipe {
    shape: RecipeShape,
    // Compatibility for recipe-shape v1 (#2818): remove after the next fleet roll.
    legacy: LegacyRecipe,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RecipeShape {
    Address { kind: String, target: String },
    Command { target: String, argv: Vec<String> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LegacyRecipe {
    Attach(Vec<String>),
    View(Vec<String>),
    Command(Vec<String>),
    Checkout(Vec<String>),
}

impl Recipe {
    pub fn kind(&self) -> &str {
        match &self.shape {
            RecipeShape::Address { kind, .. } => kind,
            RecipeShape::Command { .. } => "command",
        }
    }

    pub fn target(&self) -> &str {
        match &self.shape {
            RecipeShape::Address { target, .. } | RecipeShape::Command { target, .. } => target,
        }
    }

    pub fn argv(&self) -> Option<&[String]> {
        match &self.shape {
            RecipeShape::Address { .. } => None,
            RecipeShape::Command { argv, .. } => Some(argv),
        }
    }

    pub fn command(target: impl Into<String>, argv: Vec<String>) -> Self {
        Self { legacy: LegacyRecipe::Command(argv.clone()), shape: RecipeShape::Command { target: target.into(), argv } }
    }

    fn checkout_command(target: String, argv: Vec<String>) -> Self {
        Self { legacy: LegacyRecipe::Checkout(argv.clone()), shape: RecipeShape::Command { target, argv } }
    }

    fn address(kind: &str, target: String, legacy: LegacyRecipe) -> Self {
        Self { shape: RecipeShape::Address { kind: kind.to_owned(), target }, legacy }
    }

    pub(crate) fn legacy(&self) -> &LegacyRecipe {
        &self.legacy
    }
}

pub trait RecipeMint: Send + Sync {
    fn direct_transport(&self, _host: &HostName) -> Result<DirectTransport, String> {
        Err("viewer has no known transport to the session host".to_owned())
    }
    /// Recipe attaching a live entity — a session into a pane, or a vessel's
    /// running session into a workspace; `attach_ref` is any reference the
    /// daemon accepts (rows expose it as a capability fact).
    fn attach(&self, attach_ref: &str, host: &HostName) -> Option<Recipe>;
    /// Recipe opening a transient shell rooted at a standing checkout.
    fn checkout_terminal(&self, path: &str, host: &HostName) -> Option<Recipe>;
    /// Recipe materialising a scoped view of an entity with no live session.
    fn scoped_view(&self, target: &flotilla_protocol::ViewAddress) -> Option<Recipe>;
}

/// Recipes implemented by the Flotilla CLI: attach a live entity or open a
/// scoped focal view for an awareness-band latent.
pub struct FlotillaRecipes {
    flotilla_bin: String,
    local_host: Option<HostName>,
    ssh_hosts: BTreeMap<String, String>,
}

impl FlotillaRecipes {
    pub fn new(flotilla_bin: impl Into<String>) -> Self {
        Self { flotilla_bin: flotilla_bin.into(), local_host: None, ssh_hosts: BTreeMap::new() }
    }

    pub fn with_host_routes(mut self, local_host: HostName, ssh_hosts: BTreeMap<String, String>) -> Self {
        self.local_host = Some(local_host);
        self.ssh_hosts = ssh_hosts;
        self
    }
}

impl RecipeMint for FlotillaRecipes {
    fn direct_transport(&self, host: &HostName) -> Result<DirectTransport, String> {
        if self.local_host.as_ref() == Some(host) {
            return Ok(DirectTransport::Local);
        }
        self.ssh_hosts
            .get(host.as_str())
            .cloned()
            .map(DirectTransport::Ssh)
            .ok_or_else(|| format!("host {} has no configured SSH reachability from this viewer", host.as_str()))
    }

    fn attach(&self, attach_ref: &str, host: &HostName) -> Option<Recipe> {
        if host.as_str().contains('/') {
            return None;
        }
        Some(Recipe::address(
            "attach",
            format!("session:{host}/{attach_ref}"),
            LegacyRecipe::Attach(vec![
                self.flotilla_bin.clone(),
                "attach".to_owned(),
                "--host".to_owned(),
                host.to_string(),
                attach_ref.to_owned(),
            ]),
        ))
    }

    fn checkout_terminal(&self, path: &str, host: &HostName) -> Option<Recipe> {
        if host.as_str().contains('/') {
            return None;
        }
        // Transient checkout terminals are genuine CLI commands, not live sessions.
        let argv = vec![
            self.flotilla_bin.clone(),
            "attach".to_owned(),
            "--transient".to_owned(),
            "--host".to_owned(),
            host.to_string(),
            path.to_owned(),
        ];
        Some(Recipe::checkout_command(format!("checkout:{host}/{path}"), argv))
    }

    fn scoped_view(&self, target: &ViewAddress) -> Option<Recipe> {
        Some(Recipe::address(
            "view",
            format!("view:{target}"),
            LegacyRecipe::View(vec![self.flotilla_bin.clone(), "view".to_owned(), target.to_string()]),
        ))
    }
}
