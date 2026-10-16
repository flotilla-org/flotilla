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

/// A seed generated from Repository records, and the records it left out.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GeneratedSeed {
    pub seed: Seed,
    /// One line per repository (once, however many replicas name it) whose
    /// record names no forge repository, or a forge with no mount: these
    /// projects reach no forge in the rehearsal.
    pub skipped: Vec<String>,
}

/// Generate a seed from Repository records, as `flotilla resource list
/// repositories` prints them on each rehearsal host (an envelope with
/// `records[].object`), or as a plain array of Repository objects.
///
/// `mounts` pairs a forge's service URL, as Repository records name it in
/// `spec.forge.service_url`, with the mount it has on the fake. Service URLs
/// compare without case or a trailing `/`; a service or a mount given twice is
/// refused rather than one mapping silently winning. Records are merged
/// across inputs: a repository appears once per mount, compared
/// case-insensitively as Forgejo compares names, with the first default
/// branch a record reports. A listing that seeds nothing is refused.
pub fn seed_from_repository_records(inputs: &[serde_json::Value], mounts: &[(String, String)]) -> Result<GeneratedSeed, String> {
    let mut by_service: BTreeMap<String, &String> = BTreeMap::new();
    let mut mounted = std::collections::BTreeSet::new();
    for (service, mount) in mounts {
        if by_service.insert(normalize_service(service), mount).is_some() {
            return Err(format!("forge {service} is mapped twice"));
        }
        if !mounted.insert(mount) {
            return Err(format!("mount `{mount}` is given to more than one forge"));
        }
    }
    let mut repositories: BTreeMap<(String, String), SeedRepository> = BTreeMap::new();
    let mut skipped = std::collections::BTreeSet::new();
    for input in inputs {
        let objects: Vec<&serde_json::Value> = match input {
            serde_json::Value::Array(objects) => objects.iter().collect(),
            serde_json::Value::Object(envelope) => match envelope.get("records") {
                Some(serde_json::Value::Array(records)) => records.iter().filter_map(|record| record.get("object")).collect(),
                _ => return Err("input is neither a resource list envelope with `records` nor an array of Repository objects".into()),
            },
            _ => return Err("input is neither a resource list envelope with `records` nor an array of Repository objects".into()),
        };
        for object in objects {
            match object.get("kind").and_then(serde_json::Value::as_str) {
                Some("Repository") => {}
                Some(kind) => return Err(format!("input holds a {kind} record; expected Repository records")),
                None => return Err("input holds a record without a kind; expected Repository records".into()),
            }
            let name = object["metadata"]["name"].as_str().unwrap_or("(unnamed)");
            let forge = &object["spec"]["forge"];
            let (Some(service), Some(full_name)) = (forge["service_url"].as_str(), forge["repository"].as_str()) else {
                skipped.insert(format!("Repository {name}: names no forge repository"));
                continue;
            };
            let Some(mount) = by_service.get(&normalize_service(service)) else {
                skipped.insert(format!("{service}/{full_name}: forge {service} has no mount (add --forge {service}=<mount>)"));
                continue;
            };
            let Some((owner, repo)) = full_name.split_once('/').filter(|(owner, repo)| !owner.is_empty() && !repo.is_empty()) else {
                skipped.insert(format!("Repository {name}: forge repository `{full_name}` is not owner/name"));
                continue;
            };
            let branch = object["status"]["default_branch"].as_str().filter(|branch| !branch.trim().is_empty());
            let entry = repositories.entry(((*mount).clone(), full_name.to_ascii_lowercase())).or_insert_with(|| SeedRepository {
                owner: owner.to_string(),
                name: repo.to_string(),
                default_branch: String::new(),
            });
            if entry.default_branch.is_empty() {
                entry.default_branch = branch.unwrap_or_default().to_string();
            }
        }
    }
    if repositories.is_empty() {
        return Err(format!("no repository maps to a mounted forge ({} skipped)", skipped.len()));
    }
    let mut seed = Seed::default();
    for ((mount, _), mut repository) in repositories {
        if repository.default_branch.is_empty() {
            repository.default_branch = default_branch();
        }
        seed.forges.entry(mount).or_default().repositories.push(repository);
    }
    Ok(GeneratedSeed { seed, skipped: skipped.into_iter().collect() })
}

fn normalize_service(service: &str) -> String {
    service.trim().trim_end_matches('/').to_ascii_lowercase()
}
