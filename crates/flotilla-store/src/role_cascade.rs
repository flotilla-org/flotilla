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
        let objects = backend.definitions::<Project>(namespace).list().await?;
        // Freeze local content and its revision from the same definition read.
        let charter_commit = objects.iter().find(|object| object.metadata.name == project_name).and_then(project_charter_revision);
        let fleet = match backend.definitions::<crate::FleetDesignation>(namespace).get(crate::FLEET_DESIGNATION_NAME).await {
            Ok(designation) => Some(designation.spec.project),
            Err(ResourceError::NotFound { .. }) => None,
            Err(error) => return Err(error),
        };
        let specs: BTreeMap<_, _> = objects.into_iter().map(|object| (object.metadata.name, object.spec)).collect();
        let hierarchy =
            ProjectHierarchy::from_declared(specs.iter().map(|(name, spec)| (name.clone(), spec.parent.clone())).collect(), fleet);
        let defaults = backend
            .definitions::<CrewDefaults>(namespace)
            .list()
            .await?
            .into_iter()
            .map(|defaults| (defaults.metadata.name, defaults.spec))
            .collect::<Vec<_>>();
        let local = specs.get(project_name).unwrap_or(project);
        let mut resolved = Self::from_specs(&hierarchy, &specs, &defaults, project_name, local)?;
        resolved.charter_commit = charter_commit;
        Ok(resolved)
    }
}
