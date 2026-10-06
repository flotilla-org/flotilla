use std::collections::{BTreeMap, BTreeSet};

use crate::{FleetDesignation, Project, ProjectSpec, ResourceBackend, ResourceError, FLEET_DESIGNATION_NAME};

/// A namespace snapshot shared by cascade, supervision and selectors.
/// `new` and `load` validate; `load_for_inspection` permits repair of invalid graphs.
/// Ancestors are nearest-first; descendants are sorted and exclude the parent.
/// Before bootstrap, absent designation leaves omitted parents as roots.
#[derive(Debug, Clone)]
pub struct ProjectHierarchy {
    parents: BTreeMap<String, Option<String>>,
    fleet: Option<String>,
}

impl ProjectHierarchy {
    pub async fn load(backend: &ResourceBackend, namespace: &str) -> Result<Self, ResourceError> {
        let hierarchy = Self::load_for_inspection(backend, namespace).await?;
        Self::new(hierarchy.parents, hierarchy.fleet)
    }

    /// Read resolved edges without global validation, so operators can inspect
    /// and repair a graph made invalid by concurrent federation. Ancestors still
    /// report cycles or missing parents; descendants follow edges safely.
    pub async fn load_for_inspection(backend: &ResourceBackend, namespace: &str) -> Result<Self, ResourceError> {
        let projects = backend.definitions::<Project>(namespace).list().await?;
        let fleet = match backend.definitions::<FleetDesignation>(namespace).get(FLEET_DESIGNATION_NAME).await {
            Ok(designation) => Some(designation.spec.project),
            Err(ResourceError::NotFound { .. }) => None,
            Err(error) => return Err(error),
        };
        let parents = projects
            .into_iter()
            .map(|project| {
                let name = project.metadata.name;
                let parent =
                    if fleet.as_ref() == Some(&name) { project.spec.parent } else { project.spec.parent.or_else(|| fleet.clone()) };
                (name, parent)
            })
            .collect();
        Ok(Self { parents, fleet })
    }

    /// Build and validate a snapshot of declared parents. The fleet must be
    /// declared, have no explicit parent, and be reachable from every member.
    pub fn new(declared: BTreeMap<String, Option<String>>, fleet: Option<String>) -> Result<Self, ResourceError> {
        if let Some(fleet) = &fleet {
            let parent = declared.get(fleet).ok_or_else(|| ResourceError::invalid(format!("fleet Project `{fleet}` is not declared")))?;
            if parent.is_some() {
                return Err(ResourceError::invalid(format!("fleet Project `{fleet}` cannot have a parent")));
            }
        }
        let parents = declared
            .into_iter()
            .map(|(name, parent)| {
                let parent = if fleet.as_ref() == Some(&name) { None } else { parent.or_else(|| fleet.clone()) };
                (name, parent)
            })
            .collect();
        let hierarchy = Self { parents, fleet };
        for name in hierarchy.parents.keys() {
            hierarchy.ancestors(name)?;
        }
        Ok(hierarchy)
    }

    pub fn fleet(&self) -> Option<&str> {
        self.fleet.as_deref()
    }

    pub fn parent(&self, project: &str) -> Result<Option<&str>, ResourceError> {
        self.parents.get(project).map(|parent| parent.as_deref()).ok_or_else(|| ResourceError::not_found(project))
    }

    pub fn ancestors(&self, project: &str) -> Result<Vec<String>, ResourceError> {
        let mut ancestors = Vec::new();
        let mut seen = BTreeSet::from([project.to_string()]);
        let mut current = project;
        while let Some(parent) = self.parent(current)? {
            if !self.parents.contains_key(parent) {
                return Err(ResourceError::invalid(format!("Project `{current}` parent `{parent}` is not declared")));
            }
            if !seen.insert(parent.to_string()) {
                return Err(ResourceError::invalid(format!("Project parent cycle through `{parent}`")));
            }
            ancestors.push(parent.to_string());
            current = parent;
        }
        Ok(ancestors)
    }

    pub fn descendants(&self, parent: &str) -> Result<Vec<String>, ResourceError> {
        self.parent(parent)?;
        let mut children = BTreeMap::<&str, Vec<&str>>::new();
        for (name, declared_parent) in &self.parents {
            if let Some(parent) = declared_parent {
                children.entry(parent).or_default().push(name);
            }
        }
        let mut descendants = BTreeSet::new();
        let mut pending = vec![parent.to_string()];
        while let Some(current) = pending.pop() {
            for name in children.get(current.as_str()).into_iter().flatten() {
                if *name != parent && descendants.insert((*name).to_string()) {
                    pending.push((*name).to_string());
                }
            }
        }
        Ok(descendants.into_iter().collect())
    }
}

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
    } else {
        let spec: crate::FleetDesignationSpec = serde_json::from_value(spec).map_err(|error| ResourceError::decode(error.to_string()))?;
        fleet = Some(spec.project);
    }
    ProjectHierarchy::new(declared, fleet).map(|_| ())
}
