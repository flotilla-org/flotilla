//! The seed file: repositories to create before the first request, so their
//! default branches match the copied projects. Rehearsal tooling generates it
//! from the copied stores' Repository records.
//!
//! ```json
//! {"forges": {"github": {"repositories": [{"owner": "flotilla-org", "name": "flotilla", "default_branch": "main"}]}}}
//! ```

use std::{collections::BTreeMap, path::Path};

use serde::{Deserialize, Serialize};

use crate::{
    config::StartupError,
    model::{FakeForge, RepoRef},
};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Seed {
    /// Keyed by the forge name the configuration declares.
    pub forges: BTreeMap<String, SeedForge>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SeedForge {
    pub repositories: Vec<SeedRepository>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SeedRepository {
    pub owner: String,
    pub name: String,
    #[serde(default = "default_branch")]
    pub default_branch: String,
}

fn default_branch() -> String {
    "main".to_string()
}

impl Seed {
    pub fn parse(text: &str) -> Result<Self, StartupError> {
        serde_json::from_str(text).map_err(|error| StartupError::new(format!("seed is not valid: {error}")))
    }

    pub fn load(path: &Path) -> Result<Self, StartupError> {
        let text =
            std::fs::read_to_string(path).map_err(|error| StartupError::new(format!("cannot read seed {}: {error}", path.display())))?;
        Self::parse(&text).map_err(|error| StartupError::new(format!("seed {}: {error}", path.display())))
    }

    /// Create every listed repository. A forge the configuration does not
    /// declare is an error, so a seed for another fleet cannot half-apply:
    /// every forge is checked before any repository is created.
    pub fn apply(&self, forge: &FakeForge) -> Result<usize, StartupError> {
        let configured = forge.forge_names();
        if let Some(unknown) = self.forges.keys().find(|name| !configured.contains(name)) {
            return Err(StartupError::new(format!(
                "seed names forge `{unknown}`, which the config does not declare (declared: {})",
                configured.join(", ")
            )));
        }
        let mut seeded = 0;
        for (name, seed) in &self.forges {
            for repository in &seed.repositories {
                if repository.default_branch.trim().is_empty() {
                    return Err(StartupError::new(format!(
                        "seed repository {name}:{}/{} has an empty default branch",
                        repository.owner, repository.name
                    )));
                }
                let repo = RepoRef::new(name, &repository.owner, &repository.name);
                forge
                    .seed_repository(&repo, &repository.default_branch)
                    .map_err(|error| StartupError::new(format!("seed repository {repo}: {error}")))?;
                seeded += 1;
            }
        }
        Ok(seeded)
    }
}
