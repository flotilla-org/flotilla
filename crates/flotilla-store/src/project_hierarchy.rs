use std::collections::BTreeMap;

use crate::ResourceBackend;

pub(crate) fn is_hierarchy_kind(kind: &str) -> bool {
    matches!(kind, "Project" | "FleetDesignation")
}

/// Validate the candidate against merged definitions before any authoring write.
/// The generic DefinitionResolver erases the concrete spec through serde, as it
/// already does for causal field merging; only these two registered kinds enter.
pub(crate) async fn validate_hierarchy_write(
    backend: &ResourceBackend,
    namespace: &str,
    kind: &str,
    name: &str,
    spec: serde_json::Value,
) -> Result<(), ResourceError> {
    let projects = backend.definitions::<Project>(namespace).list().await?;
    let mut declared = projects.into_iter().map(|project| (project.metadata.name, project.spec.parent)).collect::<BTreeMap<_, _>>();
    let mut fleet = match backend.definitions::<FleetDesignation>(namespace).get(FLEET_DESIGNATION_NAME).await {
        Ok(designation) => Some(designation.spec.project),
        Err(ResourceError::NotFound { .. }) => None,
        Err(error) => return Err(error),
    };
    if kind == "Project" {
        let spec: ProjectSpec = serde_json::from_value(spec).map_err(|error| ResourceError::decode(error.to_string()))?;
        declared.insert(name.to_string(), spec.parent);
        return ProjectHierarchy::from_declared(declared, fleet).validate_project_chain(name);
    } else {
        let spec: crate::FleetDesignationSpec = serde_json::from_value(spec).map_err(|error| ResourceError::decode(error.to_string()))?;
        fleet = Some(spec.project);
    }
    ProjectHierarchy::new(declared, fleet).map(|_| ())
}
use crate::*;
#[allow(async_fn_in_trait)]
pub trait ProjectHierarchyStoreExt: Sized {
    async fn load(backend: &ResourceBackend, namespace: &str) -> Result<Self, ResourceError>;
    /// Read resolved edges without global validation, so operators can inspect
    /// and repair a graph made invalid by concurrent federation. Ancestors still
    /// report cycles or missing parents; descendants follow edges safely.
    async fn load_for_inspection(backend: &ResourceBackend, namespace: &str) -> Result<Self, ResourceError>;
}
impl ProjectHierarchyStoreExt for ProjectHierarchy {
    async fn load(backend: &ResourceBackend, namespace: &str) -> Result<Self, ResourceError> {
        let hierarchy = Self::load_for_inspection(backend, namespace).await?;
        hierarchy.validate()
    }
    async fn load_for_inspection(backend: &ResourceBackend, namespace: &str) -> Result<Self, ResourceError> {
        let projects = backend.definitions::<Project>(namespace).list().await?;
        let fleet = match backend.definitions::<FleetDesignation>(namespace).get(FLEET_DESIGNATION_NAME).await {
            Ok(designation) => Some(designation.spec.project),
            Err(ResourceError::NotFound { .. }) => None,
            Err(error) => return Err(error),
        };
        let declared = projects.into_iter().map(|project| (project.metadata.name, project.spec.parent)).collect();
        let hierarchy = Self::from_declared(declared, fleet);
        if let Some(fleet) = hierarchy.fleet() {
            if !hierarchy.parent(fleet).is_ok() {
                tracing::warn!(namespace, fleet_project = %fleet, "FleetDesignation references an undeclared Project");
            }
        }
        Ok(hierarchy)
    }
}
