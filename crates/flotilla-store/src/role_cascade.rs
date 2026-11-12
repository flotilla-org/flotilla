use std::collections::BTreeMap;

use flotilla_resources::role_cascade::project_charter_revision;

use crate::*;

#[allow(async_fn_in_trait)]
pub trait ResolvedCascadeStoreExt: Sized {
    /// Load root and ancestor CrewDefaults through the same graph as supervision.
    /// Existing unbound CrewDefaults remains the root layer during migration.
    async fn load(backend: &ResourceBackend, namespace: &str, project_name: &str, project: &ProjectSpec) -> Result<Self, ResourceError>;
}
impl ResolvedCascadeStoreExt for ResolvedCascade {
    async fn load(backend: &ResourceBackend, namespace: &str, project_name: &str, project: &ProjectSpec) -> Result<Self, ResourceError> {
        CascadeDeclarations::load(backend, namespace).await?.resolve(project_name, project)
    }
}

/// The declarations every Project's cascade resolves from, read once so that a pass resolving
/// many Projects' cascades reads them once.
pub struct CascadeDeclarations {
    projects: Vec<ResourceObject<Project>>,
    specs: BTreeMap<String, ProjectSpec>,
    hierarchy: ProjectHierarchy,
    defaults: Vec<(String, CrewDefaultsSpec)>,
}

impl CascadeDeclarations {
    pub async fn load(backend: &ResourceBackend, namespace: &str) -> Result<Self, ResourceError> {
        let projects = backend.definitions::<Project>(namespace).list().await?;
        let fleet = match backend.definitions::<crate::FleetDesignation>(namespace).get(crate::FLEET_DESIGNATION_NAME).await {
            Ok(designation) => Some(designation.spec.project),
            Err(ResourceError::NotFound { .. }) => None,
            Err(error) => return Err(error),
        };
        let specs: BTreeMap<_, _> = projects.iter().map(|object| (object.metadata.name.clone(), object.spec.clone())).collect();
        let hierarchy =
            ProjectHierarchy::from_declared(specs.iter().map(|(name, spec)| (name.clone(), spec.parent.clone())).collect(), fleet);
        let defaults = backend
            .definitions::<CrewDefaults>(namespace)
            .list()
            .await?
            .into_iter()
            .map(|defaults| (defaults.metadata.name, defaults.spec))
            .collect::<Vec<_>>();
        Ok(Self { projects, specs, hierarchy, defaults })
    }

    /// The declared Projects.
    pub fn projects(&self) -> &[ResourceObject<Project>] {
        &self.projects
    }

    /// The Project hierarchy, unvalidated, as an inspection reads it.
    pub fn hierarchy(&self) -> &ProjectHierarchy {
        &self.hierarchy
    }

    /// `project_name`'s cascade, its local content and revision from the declared Project, or
    /// from `project` when none is declared.
    pub fn resolve(&self, project_name: &str, project: &ProjectSpec) -> Result<ResolvedCascade, ResourceError> {
        let charter_commit = self.projects.iter().find(|object| object.metadata.name == project_name).and_then(project_charter_revision);
        let local = self.specs.get(project_name).unwrap_or(project);
        let mut resolved = ResolvedCascade::from_specs(&self.hierarchy, &self.specs, &self.defaults, project_name, local)?;
        resolved.charter_commit = charter_commit;
        Ok(resolved)
    }
}
