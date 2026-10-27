//! The declared definition kinds on the engine (#3824 layer 2b): Forge, FleetDesignation,
//! CredentialSpec, CredentialGrant, CrewDefaults, ImageLayer and WorkflowTemplate, each a plain
//! kind (a spec with no machine) written by its home's single writer.
//!
//! Each kind keeps its serde spec, the shape manifests author, and stores it as typed columns. The
//! provenance the old store kept as labels and annotations is one typed datum, [`DefinitionOrigin`],
//! on every kind (#3212: no labels or annotations). The two wide kinds, ImageLayer and
//! WorkflowTemplate, store the columns their reads select by and the authored definition as one
//! frozen document (ADR 0064, `frozen`); the others are laid out in full.
//!
//! Nothing reads or writes these tables yet: the import rehearsal restores into them, and they
//! join the daemon's decided store with the definition writers and readers in layer 2b-ii.

use std::collections::{BTreeMap, BTreeSet};

use flotilla_protocol::RepositoryKey;
use flotilla_resources::{
    CredentialConsumer, CredentialGrantSelector, CredentialGrantSpec, CredentialLifecycle, CredentialPlacementRequirements,
    CredentialSource, CredentialSpecSpec, CrewDefaultsSpec, FleetDesignationSpec, ForgeKind, ForgeSpec, HostActionSelector,
    HostImageAction, ImageCacheBinding, ImageLayerSource, ImageLayerSpec, ImageLayerTier, ImageSelection, LandingCredentialScope,
    RepositoryTrust, RoleDefinition, WorkflowTemplateSpec,
};
use flotilla_schema::{Frozen, KindRegistry, RegistryError, Stored};

pub use flotilla_resources::definitions::{CharterOrigin, DefinitionOrigin};

/// A Forge, as the engine stores it: kind code `fo`.
#[derive(Clone, Debug, PartialEq, Eq, Stored)]
#[store(kind = "Forge", code = "fo")]
pub struct ForgeDefinition {
    pub origin: DefinitionOrigin,
    pub forge_id: String,
    pub forge_kind: ForgeKind,
    /// DNS names and SSH aliases; the remote-host selection reads them.
    #[store(child)]
    pub hosts: BTreeSet<String>,
    pub https_url: String,
    pub git_ssh_host: String,
}

impl ForgeDefinition {
    pub fn new(origin: DefinitionOrigin, spec: &ForgeSpec) -> Self {
        Self {
            origin,
            forge_id: spec.forge_id.clone(),
            forge_kind: spec.kind,
            hosts: spec.hosts.clone(),
            https_url: spec.https_url.clone(),
            git_ssh_host: spec.git_ssh_host.clone(),
        }
    }

    pub fn spec(&self) -> ForgeSpec {
        ForgeSpec {
            forge_id: self.forge_id.clone(),
            kind: self.forge_kind,
            hosts: self.hosts.clone(),
            https_url: self.https_url.clone(),
            git_ssh_host: self.git_ssh_host.clone(),
        }
    }
}

/// The fleet's designation, as the engine stores it: kind code `fl`.
#[derive(Clone, Debug, PartialEq, Eq, Stored)]
#[store(kind = "FleetDesignation", code = "fl")]
pub struct FleetDesignationDefinition {
    pub origin: DefinitionOrigin,
    pub fleet: Option<String>,
    pub project: String,
    #[store(flatten)]
    pub image_cache: Option<ImageCacheBinding>,
}

impl FleetDesignationDefinition {
    pub fn new(origin: DefinitionOrigin, spec: &FleetDesignationSpec) -> Self {
        Self { origin, fleet: spec.fleet.clone(), project: spec.project.clone(), image_cache: spec.image_cache.clone() }
    }

    pub fn spec(&self) -> FleetDesignationSpec {
        FleetDesignationSpec { fleet: self.fleet.clone(), project: self.project.clone(), image_cache: self.image_cache.clone() }
    }
}

/// A managed credential's declaration, as the engine stores it: kind code `cp`.
#[derive(Clone, Debug, PartialEq, Eq, Stored)]
#[store(kind = "CredentialSpec", code = "cp")]
pub struct CredentialSpecDefinition {
    pub origin: DefinitionOrigin,
    pub consumer: CredentialConsumer,
    pub source: CredentialSource,
    pub lifecycle: CredentialLifecycle,
    /// The binaries a placement must provide.
    #[store(child)]
    pub binaries: BTreeSet<String>,
    pub authorization_epoch: u32,
}

impl CredentialSpecDefinition {
    pub fn new(origin: DefinitionOrigin, spec: &CredentialSpecSpec) -> Self {
        Self {
            origin,
            consumer: spec.consumer.clone(),
            source: spec.source.clone(),
            lifecycle: spec.lifecycle,
            binaries: spec.placement.binaries.clone(),
            authorization_epoch: spec.authorization_epoch,
        }
    }

    pub fn spec(&self) -> CredentialSpecSpec {
        CredentialSpecSpec {
            consumer: self.consumer.clone(),
            source: self.source.clone(),
            lifecycle: self.lifecycle,
            placement: CredentialPlacementRequirements { binaries: self.binaries.clone() },
            authorization_epoch: self.authorization_epoch,
        }
    }
}

/// A grant's landing-credential constraint, as stored: the repository key as its text.
#[derive(Clone, Debug, PartialEq, Eq, Stored)]
pub enum LandingScope {
    #[store(code = "br")]
    Branch { repository: String, branch: String },
    #[store(code = "tm")]
    TemporalOnly,
}

impl From<&LandingCredentialScope> for LandingScope {
    fn from(scope: &LandingCredentialScope) -> Self {
        match scope {
            LandingCredentialScope::Branch { repository, branch } => {
                LandingScope::Branch { repository: repository.0.clone(), branch: branch.clone() }
            }
            LandingCredentialScope::TemporalOnly => LandingScope::TemporalOnly,
        }
    }
}

impl From<&LandingScope> for LandingCredentialScope {
    fn from(scope: &LandingScope) -> Self {
        match scope {
            LandingScope::Branch { repository, branch } => {
                LandingCredentialScope::Branch { repository: RepositoryKey(repository.clone()), branch: branch.clone() }
            }
            LandingScope::TemporalOnly => LandingCredentialScope::TemporalOnly,
        }
    }
}

/// A credential grant, as the engine stores it: kind code `gt`. The selector's sets are child
/// tables, which the grant query selects by.
#[derive(Clone, Debug, PartialEq, Eq, Stored)]
#[store(kind = "CredentialGrant", code = "gt")]
pub struct CredentialGrantDefinition {
    pub origin: DefinitionOrigin,
    /// The host action a host-action grant selects; none for a work grant.
    pub host_action: Option<HostImageAction>,
    /// The hosts a host-action grant names; empty selects every eligible host.
    #[store(child)]
    pub host_action_hosts: BTreeSet<String>,
    #[store(child)]
    pub projects: BTreeSet<String>,
    #[store(child)]
    pub repositories: BTreeSet<String>,
    #[store(child)]
    pub roles: BTreeSet<String>,
    pub repository_trust: Option<RepositoryTrust>,
    #[store(child)]
    pub credentials: BTreeSet<String>,
    /// Per credential, the GitHub App permissions the grant allows: a document, read whole.
    #[store(cbor)]
    pub permissions: BTreeMap<String, BTreeMap<String, String>>,
    #[store(child)]
    pub landing_credentials: BTreeMap<String, LandingScope>,
}

impl CredentialGrantDefinition {
    pub fn new(origin: DefinitionOrigin, spec: &CredentialGrantSpec) -> Self {
        let selector = &spec.selector;
        Self {
            origin,
            host_action: selector.host_action.as_ref().map(|host| host.action),
            host_action_hosts: selector.host_action.as_ref().map(|host| host.hosts.clone()).unwrap_or_default(),
            projects: selector.projects.clone(),
            repositories: selector.repositories.iter().map(|key| key.0.clone()).collect(),
            roles: selector.roles.clone(),
            repository_trust: selector.repository_trust,
            credentials: spec.credentials.clone(),
            permissions: spec.permissions.clone(),
            landing_credentials: spec.landing_credentials.iter().map(|(name, scope)| (name.clone(), scope.into())).collect(),
        }
    }

    pub fn spec(&self) -> CredentialGrantSpec {
        CredentialGrantSpec {
            selector: CredentialGrantSelector {
                host_action: self.host_action.map(|action| HostActionSelector { action, hosts: self.host_action_hosts.clone() }),
                projects: self.projects.clone(),
                repositories: self.repositories.iter().map(|key| RepositoryKey(key.clone())).collect(),
                roles: self.roles.clone(),
                repository_trust: self.repository_trust,
            },
            credentials: self.credentials.clone(),
            permissions: self.permissions.clone(),
            landing_credentials: self.landing_credentials.iter().map(|(name, scope)| (name.clone(), scope.into())).collect(),
        }
    }
}

/// One layer of crew defaults, as the engine stores it: kind code `dc`.
#[derive(Clone, Debug, PartialEq, Eq, Stored)]
#[store(kind = "CrewDefaults", code = "dc")]
pub struct CrewDefaultsDefinition {
    pub origin: DefinitionOrigin,
    pub project_ref: Option<String>,
    pub default_workflow_ref: Option<String>,
    #[store(child)]
    pub roles: BTreeMap<String, RoleDefinition>,
    /// Skill lists by role: a document, read whole by the cascade.
    #[store(cbor)]
    pub skills: BTreeMap<String, Vec<String>>,
    /// The installation image selection: a document, read whole by image resolution.
    #[store(cbor)]
    pub image: Option<ImageSelection>,
}

impl CrewDefaultsDefinition {
    pub fn new(origin: DefinitionOrigin, spec: &CrewDefaultsSpec) -> Self {
        Self {
            origin,
            project_ref: spec.project_ref.clone(),
            default_workflow_ref: spec.default_workflow_ref.clone(),
            roles: spec.roles.clone(),
            skills: spec.skills.clone(),
            image: spec.image.clone(),
        }
    }

    pub fn spec(&self) -> CrewDefaultsSpec {
        CrewDefaultsSpec {
            project_ref: self.project_ref.clone(),
            default_workflow_ref: self.default_workflow_ref.clone(),
            roles: self.roles.clone(),
            skills: self.skills.clone(),
            image: self.image.clone(),
        }
    }
}

/// An image layer, as the engine stores it: kind code `il`. The columns are the ones the
/// catalogue selects by; the authored layer, with its fragment, probe and context files, is one
/// frozen document.
#[derive(Clone, Debug, PartialEq, Eq, Stored)]
#[store(kind = "ImageLayer", code = "il")]
pub struct ImageLayerDefinition {
    pub origin: DefinitionOrigin,
    pub layer: String,
    pub layer_version: String,
    pub tier: ImageLayerTier,
    pub source: ImageLayerSource,
    pub project: Option<String>,
    #[store(child)]
    pub provides: BTreeSet<String>,
    #[store(frozen)]
    pub definition: Frozen<ImageLayerSpec>,
}

impl ImageLayerDefinition {
    pub fn new(origin: DefinitionOrigin, spec: &ImageLayerSpec) -> Result<Self, String> {
        Ok(Self {
            origin,
            layer: spec.name.clone(),
            layer_version: spec.version.clone(),
            tier: spec.tier,
            source: spec.source,
            project: spec.project.clone(),
            provides: spec.provides.clone(),
            definition: Frozen::new(spec.clone()).map_err(|error| format!("freeze the layer: {error}"))?,
        })
    }

    pub fn spec(&self) -> &ImageLayerSpec {
        self.definition.value()
    }
}

/// A workflow template, as the engine stores it: kind code `wt`. The role names are a child
/// table; the authored template is one frozen document, never queried by its internals.
#[derive(Clone, Debug, PartialEq, Eq, Stored)]
#[store(kind = "WorkflowTemplate", code = "wt")]
pub struct WorkflowTemplateDefinition {
    pub origin: DefinitionOrigin,
    #[store(child)]
    pub roles: BTreeSet<String>,
    #[store(frozen)]
    pub template: Frozen<WorkflowTemplateSpec>,
}

impl WorkflowTemplateDefinition {
    pub fn new(origin: DefinitionOrigin, spec: &WorkflowTemplateSpec) -> Result<Self, String> {
        Ok(Self {
            origin,
            roles: spec.roles.iter().map(|role| role.role.clone()).collect(),
            template: Frozen::new(spec.clone()).map_err(|error| format!("freeze the template: {error}"))?,
        })
    }

    pub fn spec(&self) -> &WorkflowTemplateSpec {
        self.template.value()
    }
}

/// Registers the definition kinds into `registry` under their codes: Forge `fo`,
/// FleetDesignation `fl`, CredentialSpec `cp`, CredentialGrant `gt`, CrewDefaults `dc`,
/// ImageLayer `il` and WorkflowTemplate `wt`. All are plain kinds.
pub fn register(registry: &mut KindRegistry) -> Result<(), RegistryError> {
    registry.register::<ForgeDefinition>()?;
    registry.register::<FleetDesignationDefinition>()?;
    registry.register::<CredentialSpecDefinition>()?;
    registry.register::<CredentialGrantDefinition>()?;
    registry.register::<CrewDefaultsDefinition>()?;
    registry.register::<ImageLayerDefinition>()?;
    registry.register::<WorkflowTemplateDefinition>()?;
    Ok(())
}

/// The definition kinds alone, for their DDL snapshot.
pub fn registry() -> Result<KindRegistry, RegistryError> {
    let mut registry = KindRegistry::new();
    register(&mut registry)?;
    Ok(registry)
}
