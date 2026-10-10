use std::collections::{BTreeMap, BTreeSet};

use crate::ResourceError;

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
    pub fn from_declared(declared: BTreeMap<String, Option<String>>, fleet: Option<String>) -> Self {
        let parents = declared
            .into_iter()
            .map(|(name, parent)| {
                let parent = if fleet.as_ref() == Some(&name) { parent } else { parent.or_else(|| fleet.clone()) };
                (name, parent)
            })
            .collect();
        Self { parents, fleet }
    }

    /// Authoring a Project requires its resulting ancestry to be valid, even if
    /// independent chains remain broken by federation. This permits incremental
    /// repairs without introducing dangling edges or closing a cycle.
    pub fn validate_project_chain(&self, name: &str) -> Result<(), ResourceError> {
        if self.fleet.as_deref() == Some(name) && self.parent(name)?.is_some() {
            return Err(ResourceError::invalid(format!("fleet Project `{name}` cannot have a parent")));
        }
        self.ancestors(name).map(|_| ())
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
        let hierarchy = Self::from_declared(declared, fleet);
        for name in hierarchy.parents.keys() {
            hierarchy.validate_project_chain(name)?;
        }
        Ok(hierarchy)
    }

    pub fn validate(self) -> Result<Self, ResourceError> {
        Self::new(self.parents, self.fleet)
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
