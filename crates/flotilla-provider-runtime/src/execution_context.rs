//! The execution context flotilla prepares for one environment, in two layers.
//!
//! The base layer is credential-free: the composed `PATH` (generation layers,
//! declared supplies, the system tail), supply variables, locale and
//! non-secret harness settings, every path already absolute against the
//! target user's discovered home. The bound layer carries the account
//! material a crew is granted: credential wrapper directories, token-file
//! variables and harness account homes. Crew launch renders both layers;
//! an observation receives the base layer only, so it cannot inherit account
//! authority, and both see the same tools.
//!
//! Which variables carry account state is declared by each harness or
//! provider family ([`AccountState`]), never listed here.

use std::collections::{BTreeMap, BTreeSet};

use flotilla_protocol::arg::validate_env_key;

use crate::host_environment::{HostEnvironment, SYSTEM_PATH_TAIL};

/// Locale variables the base layer carries from the target's discovery.
pub const LOCALE_VARIABLES: &[&str] = &["LANG", "LC_ALL", "TZ"];

/// The locale a target that reports none runs with.
const DEFAULT_LANG: &str = "C.UTF-8";

/// Variables an observation's isolation overlay always replaces.
const OVERLAY_VARIABLES: &[&str] = &["HOME", "XDG_CONFIG_HOME", "XDG_CACHE_HOME", "XDG_STATE_HOME", "XDG_RUNTIME_DIR", "TMPDIR"];

/// Why an environment's context could not be prepared or rendered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PreparationError {
    /// The host's declarations contradict what flotilla prepares.
    HostConfiguration(String),
    /// Something preparation needs is not known yet, such as the home.
    NotReady(String),
    /// A launch asked for what its prepared context does not grant.
    Refused(String),
}

impl PreparationError {
    /// The reason, naming the environment it applies to.
    pub fn for_environment(&self, environment: &str) -> String {
        match self {
            Self::HostConfiguration(reason) => format!("host configuration error for {environment}: {reason}"),
            Self::NotReady(reason) => format!("{environment} is not ready: {reason}"),
            Self::Refused(reason) => format!("launch on {environment} refused: {reason}"),
        }
    }
}

impl std::fmt::Display for PreparationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::HostConfiguration(reason) => write!(f, "host configuration error: {reason}"),
            Self::NotReady(reason) => write!(f, "not ready: {reason}"),
            Self::Refused(reason) => f.write_str(reason),
        }
    }
}

/// The variables one harness or provider family keeps its account state in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AccountState {
    pub family: &'static str,
    /// Directories holding the family's login and configuration. An
    /// observation points each at a throwaway directory.
    pub homes: &'static [&'static str],
    /// Variables carrying account material directly. An observation never
    /// has them.
    pub secrets: &'static [&'static str],
    /// Settings an observation sets so the family's tool does no more than
    /// answer an identity or version query.
    pub observation_defaults: &'static [(&'static str, &'static str)],
}

impl AccountState {
    pub const fn new(family: &'static str) -> Self {
        Self { family, homes: &[], secrets: &[], observation_defaults: &[] }
    }

    pub const fn homes(mut self, homes: &'static [&'static str]) -> Self {
        self.homes = homes;
        self
    }

    pub const fn secrets(mut self, secrets: &'static [&'static str]) -> Self {
        self.secrets = secrets;
        self
    }

    pub const fn observation_defaults(mut self, defaults: &'static [(&'static str, &'static str)]) -> Self {
        self.observation_defaults = defaults;
        self
    }
}

/// The account-state declarations of every registered family.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AccountVariables {
    owners: BTreeMap<String, String>,
    homes: BTreeSet<String>,
    observation_defaults: BTreeMap<String, String>,
}

impl AccountVariables {
    pub fn from_families<'a>(families: impl IntoIterator<Item = &'a AccountState>) -> Self {
        let mut variables = Self::default();
        for family in families {
            for name in family.homes.iter().chain(family.secrets) {
                variables.owners.entry((*name).to_string()).or_insert_with(|| family.family.to_string());
            }
            variables.homes.extend(family.homes.iter().map(|name| (*name).to_string()));
            for (name, value) in family.observation_defaults {
                variables.observation_defaults.entry((*name).to_string()).or_insert_with(|| (*value).to_string());
            }
        }
        variables
    }

    /// The family whose account state `name` carries.
    pub fn owner(&self, name: &str) -> Option<&str> {
        self.owners.get(name).map(String::as_str)
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.owners.keys().map(String::as_str)
    }

    pub fn homes(&self) -> impl Iterator<Item = &str> {
        self.homes.iter().map(String::as_str)
    }

    pub fn observation_defaults(&self) -> impl Iterator<Item = (&str, &str)> {
        self.observation_defaults.iter().map(|(name, value)| (name.as_str(), value.as_str()))
    }
}

/// The credential-free layer every probe and crew on an environment shares.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BaseLayer {
    generation: Vec<String>,
    supplied: Vec<String>,
    variables: BTreeMap<String, String>,
    /// The supply that declared each supply variable.
    supplies: BTreeMap<String, String>,
}

impl BaseLayer {
    /// A target whose environment was declared elsewhere, such as an image:
    /// its `PATH` and variables as declared, minus its home.
    pub fn declared(environment: impl IntoIterator<Item = (String, String)>, accounts: &AccountVariables) -> Result<Self, String> {
        let mut layer = Self::default();
        for (name, value) in environment {
            if let Some(family) = accounts.owner(&name) {
                return Err(format!("the declared environment sets {name}, which carries `{family}` account state"));
            }
            match name.as_str() {
                "PATH" => layer.supplied = dedup(value.split(':').filter(|entry| !entry.is_empty()).map(str::to_string)),
                "HOME" => {}
                _ => {
                    layer.variables.insert(name, value);
                }
            }
        }
        if layer.supplied.is_empty() {
            layer.supplied = SYSTEM_PATH_TAIL.iter().map(|entry| entry.to_string()).collect();
        }
        Ok(layer)
    }

    /// The composed `PATH`: generation layers, then supplies and the system tail.
    pub fn path(&self) -> String {
        self.path_entries().join(":")
    }

    fn path_entries(&self) -> Vec<String> {
        dedup(self.generation.iter().chain(&self.supplied).cloned())
    }

    /// Variables besides `PATH`, by name.
    pub fn variables(&self) -> impl Iterator<Item = (&str, &str)> {
        self.variables.iter().map(|(name, value)| (name.as_str(), value.as_str()))
    }

    /// `PATH`, then every variable by name: the layer's one rendering, so two
    /// runners given the same layer render the same bytes.
    pub fn environment(&self) -> Vec<(String, String)> {
        std::iter::once(("PATH".to_string(), self.path())).chain(self.variables.iter().map(|(n, v)| (n.clone(), v.clone()))).collect()
    }

    /// Whether this layer sets `name`.
    pub fn sets(&self, name: &str) -> bool {
        name == "PATH" || self.variables.contains_key(name)
    }

    /// The supply that declared variable `name`, if a supply did.
    pub fn supply_of(&self, name: &str) -> Option<&str> {
        self.supplies.get(name).map(String::as_str)
    }
}

/// The account material one crew is granted, from its selected plans.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BoundLayer {
    path: Vec<String>,
    variables: BTreeMap<String, String>,
}

impl BoundLayer {
    /// `path` holds credential wrapper directories; `variables` holds
    /// token-file variables and harness account homes.
    pub fn new(path: Vec<String>, variables: impl IntoIterator<Item = (String, String)>) -> Result<Self, String> {
        for entry in &path {
            if !entry.starts_with('/') || entry.contains(':') {
                return Err(format!("bound path entry {entry:?} is not an absolute directory"));
            }
        }
        let mut layer = Self { path: dedup(path.into_iter()), variables: BTreeMap::new() };
        for (name, value) in variables {
            validate_env_key(&name).map_err(|error| format!("bound layer: {error}"))?;
            if matches!(name.as_str(), "PATH" | "HOME") {
                return Err(format!("the bound layer cannot set {name}"));
            }
            if layer.variables.insert(name.clone(), value).is_some() {
                return Err(format!("the bound layer sets {name} twice"));
            }
        }
        Ok(layer)
    }

    /// The material a launch's credential delivery produced: the directories
    /// it put on `PATH` ahead of `base`, and its other variables. An entry
    /// already in the base layer or the system tail is not a wrapper
    /// directory: delivery prepends its wrappers to the runner's own `PATH`,
    /// and every wrapper directory is a private absolute path of its own.
    pub fn delivered(delivered: &[(String, String)], base: &BaseLayer) -> Result<Self, String> {
        let known = base.path_entries();
        let path = delivered
            .iter()
            .filter(|(name, _)| name == "PATH")
            .flat_map(|(_, value)| value.split(':'))
            .filter(|entry| !entry.is_empty() && !known.iter().any(|known| known == entry) && !SYSTEM_PATH_TAIL.contains(entry))
            .map(str::to_string)
            .collect();
        let mut variables = BTreeMap::new();
        for (name, value) in delivered.iter().filter(|(name, _)| name != "PATH") {
            variables.insert(name.clone(), value.clone());
        }
        Self::new(path, variables)
    }

    pub fn path(&self) -> &[String] {
        &self.path
    }

    pub fn variables(&self) -> impl Iterator<Item = (&str, &str)> {
        self.variables.iter().map(|(name, value)| (name.as_str(), value.as_str()))
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.variables.get(name).map(String::as_str)
    }
}

/// One environment's prepared context: the base layer, and the bound layer
/// when it was prepared for a crew.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreparedContext {
    base: BaseLayer,
    bound: Option<BoundLayer>,
}

impl PreparedContext {
    pub fn base(&self) -> &BaseLayer {
        &self.base
    }

    pub fn bound(&self) -> Option<&BoundLayer> {
        self.bound.as_ref()
    }

    /// The `PATH` a crew runs with: generation layers, the bound layer's
    /// wrapper directories, then supplies and the system tail, so a wrapper
    /// shadows a supplied tool of the same name.
    pub fn crew_path(&self) -> String {
        let bound = self.bound.iter().flat_map(|bound| bound.path.iter());
        dedup(self.base.generation.iter().chain(bound).chain(&self.base.supplied).cloned()).join(":")
    }

    /// Render both layers onto a crew launch's own variables. The launch may
    /// not set a base variable, and may set a bound variable, including a
    /// family's account variable, only as the bound layer sets it. A launch
    /// `PATH` is replaced by the crew `PATH`: the wrapper directories it may
    /// name reach the crew only through the bound layer. Refused renderings
    /// leave `launch` unchanged.
    pub fn render_crew(&self, launch: &mut Vec<(String, String)>, accounts: &AccountVariables) -> Result<(), PreparationError> {
        for (name, value) in launch.iter().filter(|(name, _)| name != "PATH") {
            if self.base.sets(name) {
                return Err(match self.base.supplies.get(name) {
                    Some(supply) => PreparationError::HostConfiguration(format!("supply `{supply}` sets {name}, which flotilla sets")),
                    None => PreparationError::Refused(format!("the launch sets {name}, which the environment's base layer sets")),
                });
            }
            let bound = self.bound.as_ref().and_then(|bound| bound.get(name));
            if let Some(family) = accounts.owner(name) {
                if bound != Some(value.as_str()) {
                    return Err(PreparationError::Refused(format!(
                        "the launch sets {name}, `{family}` account state that no bound plan supplies"
                    )));
                }
            } else if bound.is_some_and(|bound| bound != value) {
                return Err(PreparationError::Refused(format!("the launch sets {name}, which its bound plan sets differently")));
            }
        }
        launch.retain(|(name, _)| name != "PATH");
        if let Some(bound) = &self.bound {
            for (name, value) in bound.variables() {
                if !launch.iter().any(|(key, _)| key == name) {
                    launch.push((name.to_string(), value.to_string()));
                }
            }
        }
        launch.push(("PATH".to_string(), self.crew_path()));
        launch.extend(self.base.variables.iter().map(|(name, value)| (name.clone(), value.clone())));
        Ok(())
    }
}

/// What an observation runs with: the base layer, and the isolation overlay
/// its executor adds around it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ObservationContext {
    base: BaseLayer,
    accounts: AccountVariables,
}

impl ObservationContext {
    /// The base layer is all an observation inherits; the bound layer is
    /// never constructed for one.
    pub fn new(base: BaseLayer, accounts: &AccountVariables) -> Self {
        Self { base, accounts: accounts.clone() }
    }

    pub fn base(&self) -> &BaseLayer {
        &self.base
    }

    pub fn accounts(&self) -> &AccountVariables {
        &self.accounts
    }

    /// The same isolation over a base declared by the target itself.
    pub fn with_base(&self, base: BaseLayer) -> Self {
        Self { base, accounts: self.accounts.clone() }
    }

    /// `NAME=value` assignments for the observed command: the base layer,
    /// then the families' observation defaults. The overlay's throwaway
    /// directories are added by the executor, after these, so they win.
    pub fn assignments(&self) -> Vec<String> {
        let defaults = self.accounts.observation_defaults().filter(|(name, _)| !OVERLAY_VARIABLES.contains(name));
        self.base
            .environment()
            .into_iter()
            .filter(|(name, _)| !OVERLAY_VARIABLES.contains(&name.as_str()))
            .chain(defaults.map(|(name, value)| (name.to_string(), value.to_string())))
            .map(|(name, value)| format!("{name}={value}"))
            .collect()
    }

    /// Family account homes the executor points into its throwaway root.
    pub fn account_homes(&self) -> Vec<String> {
        self.accounts.homes().filter(|name| !OVERLAY_VARIABLES.contains(name)).map(str::to_string).collect()
    }
}

/// Prepares the execution context of one environment. Every runner renders
/// what this produces; none composes an environment of its own.
#[derive(Clone, Debug)]
pub struct ExecutionPreparer {
    host: HostEnvironment,
    accounts: AccountVariables,
    home: Result<String, String>,
    generation: Vec<String>,
    locale: Vec<(String, String)>,
    settings: Vec<(String, String)>,
}

impl ExecutionPreparer {
    /// `home` is the target user's home as discovered for this runner. An
    /// unknown or relative home makes every preparation not ready.
    pub fn new(host: HostEnvironment, accounts: AccountVariables, home: Option<&str>) -> Self {
        let home = match home {
            Some(home) if home.starts_with('/') => Ok(home.to_string()),
            Some(home) => Err(format!("the target user's home {home:?} is not absolute")),
            None => Err("the target user's home is unknown".to_string()),
        };
        Self { host, accounts, home, generation: Vec::new(), locale: Vec::new(), settings: Vec::new() }
    }

    /// Directories flotilla installs from its generation, first on `PATH`.
    pub fn with_generation(mut self, generation: Vec<String>) -> Self {
        self.generation = generation;
        self
    }

    /// The target's locale, from its discovered variables.
    pub fn with_locale<'a>(mut self, lookup: impl Fn(&str) -> Option<&'a str>) -> Self {
        self.locale = LOCALE_VARIABLES.iter().filter_map(|name| lookup(name).map(|value| (name.to_string(), value.to_string()))).collect();
        self
    }

    /// Non-secret harness settings. They override the discovered locale.
    pub fn with_settings(mut self, settings: &[(String, String)]) -> Self {
        self.settings = settings.to_vec();
        self
    }

    pub fn accounts(&self) -> &AccountVariables {
        &self.accounts
    }

    /// The credential-free layer.
    pub fn base(&self) -> Result<BaseLayer, PreparationError> {
        let home = self.home.clone().map_err(PreparationError::NotReady)?;
        self.host.check_account_state(&self.accounts).map_err(PreparationError::HostConfiguration)?;
        let mut variables = BTreeMap::from([("LANG".to_string(), DEFAULT_LANG.to_string())]);
        let mut supplies = BTreeMap::new();
        for (name, value) in self.locale.iter().chain(&self.settings) {
            if matches!(name.as_str(), "PATH" | "HOME") || name.starts_with("FLOTILLA_") {
                return Err(PreparationError::HostConfiguration(format!("harness setting {name} is a variable flotilla composes")));
            }
            if let Some(family) = self.accounts.owner(name) {
                return Err(PreparationError::HostConfiguration(format!("harness setting {name} carries `{family}` account state")));
            }
            variables.insert(name.clone(), value.clone());
        }
        for supply in self.host.supplies() {
            for (name, value) in &supply.env {
                if variables.contains_key(name) {
                    return Err(PreparationError::HostConfiguration(format!("supply `{}` sets {name}, which flotilla sets", supply.name)));
                }
                variables.insert(name.clone(), expand_home(value, &home));
                supplies.insert(name.clone(), supply.name.clone());
            }
        }
        let supplied = self
            .host
            .supplies()
            .iter()
            .flat_map(|supply| supply.path.iter().map(|entry| expand_home(entry, &home)))
            .chain(SYSTEM_PATH_TAIL.iter().map(|entry| entry.to_string()));
        Ok(BaseLayer { generation: dedup(self.generation.iter().cloned()), supplied: dedup(supplied), variables, supplies })
    }

    /// The context for a crew: the base layer and its bound layer.
    pub fn prepare(&self, bound: Option<BoundLayer>) -> Result<PreparedContext, PreparationError> {
        Ok(PreparedContext { base: self.base()?, bound })
    }

    /// The context for an observation: the base layer only.
    pub fn observation(&self) -> Result<ObservationContext, PreparationError> {
        Ok(ObservationContext::new(self.base()?, &self.accounts))
    }
}

fn expand_home(value: &str, home: &str) -> String {
    match value.strip_prefix('~') {
        Some("") => home.to_string(),
        Some(rest) if rest.starts_with('/') => format!("{}{rest}", home.trim_end_matches('/')),
        _ => value.to_string(),
    }
}

fn dedup(entries: impl Iterator<Item = String>) -> Vec<String> {
    let mut seen = BTreeSet::new();
    entries.filter(|entry| seen.insert(entry.clone())).collect()
}

#[cfg(test)]
mod tests;
