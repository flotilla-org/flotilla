//! The records the port contract runs with, one set per kind it covers: each run, in memory and
//! against the real port, takes the same ones.

use std::collections::BTreeMap;

use chrono::{TimeZone, Utc};
use flotilla_resources::{
    Artifact, ArtifactSpec, ArtifactStatus, DefaultBranchObservation, DefaultBranchProvenance, Environment, EnvironmentPhase,
    EnvironmentSpec, EnvironmentStatus, HostDirectEnvironmentSpec, Repository, RepositorySpec, RepositoryStatus,
};

use crate::contract::Samples;

/// Artifacts, a plain kind: a whole spec, no status kept.
pub struct Artifacts;

fn artifact(index: usize, digest: char) -> ArtifactSpec {
    ArtifactSpec::builder()
        .convoy(format!("convoy-{index}"))
        .producer("coder".into())
        .kind("decision-ledger".into())
        .subject(format!("convoy-{index}"))
        .summary(BTreeMap::new())
        .digest(digest.to_string().repeat(64))
        .size(43)
        .media_type("text/markdown".into())
        .recorded_at(Utc.timestamp_opt(1_800_000_000, 0).single().expect("time"))
        .expires_at(Utc.timestamp_opt(1_900_000_000, 0).single().expect("time"))
        .build()
}

impl Samples<Artifact> for Artifacts {
    fn sample(&self, index: usize) -> (String, ArtifactSpec) {
        (format!("ledger-{index}"), artifact(index, '1'))
    }
    fn edited(&self, index: usize) -> ArtifactSpec {
        artifact(index, '2')
    }
    fn status(&self) -> Option<ArtifactStatus> {
        Some(ArtifactStatus::default())
    }
    fn selector(&self, index: usize) -> Option<(String, String)> {
        Some(("spec.convoy".into(), format!("convoy-{index}")))
    }
}

/// Repositories, named by their key, with a status written beside the spec.
pub struct Repositories;

fn repository(index: usize) -> RepositorySpec {
    RepositorySpec::remote(format!("https://forge.example/o/app-{index}")).expect("repository")
}

impl Samples<Repository> for Repositories {
    fn sample(&self, index: usize) -> (String, RepositorySpec) {
        let spec = repository(index);
        (spec.key().to_string(), spec)
    }
    fn edited(&self, index: usize) -> RepositorySpec {
        repository(index).with_allow_reviewless_workflows(true)
    }
    fn status(&self) -> Option<RepositoryStatus> {
        Some(RepositoryStatus {
            default_branch: Some("main".into()),
            default_branch_observations: vec![DefaultBranchObservation {
                branch: "main".into(),
                provenance: DefaultBranchProvenance::Forge,
            }],
            ..Default::default()
        })
    }
}

/// Environments, held by the engine as their lifecycle runs and read through its views.
pub struct Environments;

impl Samples<Environment> for Environments {
    fn sample(&self, index: usize) -> (String, EnvironmentSpec) {
        (format!("environment-{index}"), self.edited(index))
    }
    fn edited(&self, index: usize) -> EnvironmentSpec {
        EnvironmentSpec {
            host_direct: Some(HostDirectEnvironmentSpec { host_ref: "alpha".into(), repo_default_dir: format!("/work/{index}") }),
            ..EnvironmentSpec::default()
        }
    }
    fn status(&self) -> Option<EnvironmentStatus> {
        Some(EnvironmentStatus { phase: EnvironmentPhase::Ready, ..Default::default() })
    }
}
