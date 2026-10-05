//! Revisioned inputs to charter reconciliation. Branch bindings also provide the
//! identity needed by future write-through and branch promotion.
use std::path::{Component, Path};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CharterSource {
    Repository {
        repo: String,
        branch: String,
        #[serde(default)]
        path: String,
    },
    LocalDirectory {
        directory: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CharterStoreBinding {
    pub host: String,
    pub source: CharterSource,
}

impl CharterSource {
    pub fn validate(&self) -> Result<(), String> {
        match self {
            Self::Repository { repo, branch, path } => {
                if repo.trim().is_empty() || repo.starts_with('-') {
                    return Err("charter repository must be nonempty and cannot start with '-'".into());
                }
                if branch.trim().is_empty() || branch.starts_with('-') || branch.starts_with("refs/") {
                    return Err("charter branch must be a branch name".into());
                }
                if Path::new(path).components().any(|part| !matches!(part, Component::Normal(_) | Component::CurDir)) {
                    return Err("charter path must stay within the repository".into());
                }
            }
            Self::LocalDirectory { directory } => {
                if !Path::new(directory).is_absolute() {
                    return Err("local charter directory must be absolute".into());
                }
            }
        }
        Ok(())
    }
}
