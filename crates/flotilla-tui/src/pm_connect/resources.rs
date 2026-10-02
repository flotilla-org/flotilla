//! Replicated subject observations held by the PM connector.
use std::collections::{BTreeMap, BTreeSet};

use flotilla_manifest::projection::{subject_forge, SubjectCatalogInput};
use flotilla_protocol::{IssueSource, RepositoryAlias, ResourceReadEnvelope, ResourceRecordProvenance, ResourceRecordType};
use flotilla_resources::{
    ChangeRequest, Forge, Issue, K8sResourceObject, Project, Repository, RepositoryIdentity, Resource, ResourceObject,
};
use serde_json::Value;

pub(super) const KINDS: &[&str] = &["changerequests", "issues", "forges", "repositories", "projects"];

#[derive(Default)]
pub(super) struct Records {
    // Retain each source separately: deleting one replica must not erase the
    // same observation still held by another root.
    objects: BTreeMap<(String, String, String, String), Value>,
}

fn decode<T: Resource>(value: &Value) -> Result<ResourceObject<T>, String> {
    let object: K8sResourceObject<T> = serde_json::from_value(value.clone()).map_err(|error| error.to_string())?;
    ResourceObject::from_k8s_object(object).map_err(|error| error.to_string())
}

impl Records {
    pub(super) fn apply(&mut self, envelope: &ResourceReadEnvelope) -> Result<(), String> {
        let mut updates = Vec::new();
        for record in &envelope.records {
            if record.record_type == ResourceRecordType::Bookmark {
                continue;
            }
            let Some(object) = &record.object else { continue };
            let root = match &record.provenance {
                ResourceRecordProvenance::Local { node_id } => node_id.to_string(),
                ResourceRecordProvenance::Replica { origin_root, .. } => origin_root.to_string(),
            };
            if record.record_type == ResourceRecordType::Deleted {
                let name = object.pointer("/metadata/name").and_then(Value::as_str).ok_or("resource tombstone has no name")?;
                let namespace = object.pointer("/metadata/namespace").and_then(Value::as_str).unwrap_or(&envelope.namespace);
                updates.push(((envelope.plural.clone(), namespace.to_owned(), name.to_owned(), root), record.record_type, object.clone()));
                continue;
            }
            let (namespace, name) = match envelope.plural.as_str() {
                "changerequests" => {
                    let object = decode::<ChangeRequest>(object)?;
                    (object.metadata.namespace, object.metadata.name)
                }
                "issues" => {
                    let object = decode::<Issue>(object)?;
                    (object.metadata.namespace, object.metadata.name)
                }
                "forges" => {
                    let object = decode::<Forge>(object)?;
                    (object.metadata.namespace, object.metadata.name)
                }
                "repositories" => {
                    let object = decode::<Repository>(object)?;
                    (object.metadata.namespace, object.metadata.name)
                }
                "projects" => {
                    let object = decode::<Project>(object)?;
                    (object.metadata.namespace, object.metadata.name)
                }
                _ => return Err(format!("unexpected subject catalog resource {}", envelope.plural)),
            };

            updates.push(((envelope.plural.clone(), namespace, name, root), record.record_type, object.clone()));
        }
        for (key, kind, object) in updates {
            match kind {
                ResourceRecordType::Deleted => {
                    self.objects.remove(&key);
                }
                ResourceRecordType::Bookmark => {}
                _ => {
                    self.objects.insert(key, object);
                }
            }
        }
        Ok(())
    }

    fn typed<T: Resource>(&self, kind: &str) -> Vec<ResourceObject<T>> {
        let mut objects = BTreeMap::new();
        for ((plural, namespace, name, _), value) in &self.objects {
            if plural != kind {
                continue;
            }
            // Select consistently across roots, using observation time first.
            // Definitions/convergent facts have already been merged by the
            // resource API; a deterministic serialized tie-break keeps the
            // projection independent of which root is local.
            let key = (namespace.clone(), name.clone());
            let stamp = (value.pointer("/status/state/observed_at").and_then(Value::as_str).unwrap_or(""), value.to_string());
            if objects.get(&key).is_none_or(|(prior, _)| &stamp > prior) {
                objects.insert(key, (stamp, decode::<T>(value).expect("validated resource object")));
            }
        }
        objects.into_values().map(|(_, object)| object).collect()
    }

    pub(super) fn projection(&self) -> SubjectCatalogInput {
        let forges = self.typed::<Forge>("forges");
        let repositories = self.typed::<Repository>("repositories");
        let projects = self.typed::<Project>("projects");
        let change_requests = self.typed::<ChangeRequest>("changerequests");
        let issues = self.typed::<Issue>("issues");
        let sources: BTreeSet<_> = change_requests
            .iter()
            .map(|record| IssueSource { service: record.spec.service.clone(), scope: record.spec.scope.clone() })
            .chain(issues.iter().map(|record| IssueSource { service: record.spec.service.clone(), scope: record.spec.scope.clone() }))
            .collect();
        let mut references = flotilla_protocol::ReferenceContext::default();
        for project in projects {
            for membership in &project.spec.repositories {
                let Some(repo) = repositories
                    .iter()
                    .find(|repo| repo.metadata.namespace == project.metadata.namespace && repo.metadata.name == membership.repo.0)
                else {
                    continue;
                };
                let (service, scope) = match repo.spec.identity() {
                    RepositoryIdentity::Forge { forge_ref, owner, repo_name } => (forge_ref.clone(), format!("{owner}/{repo_name}")),
                    _ => {
                        let Some(identity) = repo.spec.issue_source_forge() else { continue };
                        (identity.service_url, identity.repository)
                    }
                };
                let Some(forge) = subject_forge(&forges, &service) else { continue };
                // Preserve the observation's service spelling: previous-generation
                // records use hosts/URLs rather than the canonical Forge ID.
                let mut observed_sources: Vec<_> = sources
                    .iter()
                    .filter(|source| {
                        source.scope == scope
                            && subject_forge(&forges, &source.service).is_some_and(|owner| owner.spec.forge_id == forge.spec.forge_id)
                    })
                    .cloned()
                    .collect();
                if observed_sources.is_empty() {
                    observed_sources.push(IssueSource { service, scope: scope.clone() });
                }
                for source in observed_sources {
                    let declaration = project.spec.issue_source_bindings.iter().find(|binding| {
                        binding.source.scope == scope
                            && subject_forge(&forges, &binding.source.service)
                                .is_some_and(|owner| owner.spec.forge_id == forge.spec.forge_id)
                    });
                    let alias = declaration
                        .and_then(|binding| binding.alias.clone())
                        .or_else(|| membership.alias.clone())
                        .unwrap_or_else(|| scope.rsplit('/').next().expect("repository scope").to_owned());
                    references.repositories.push(RepositoryAlias {
                        project: Some(project.metadata.name.clone()),
                        alias,
                        source,
                        web_base: forge.spec.https_url.clone(),
                        forge_alias: Some(forge.spec.forge_id.clone()),
                    });
                }
            }
            for binding in &project.spec.issue_source_bindings {
                let Some(alias) = &binding.alias else { continue };
                if references
                    .repositories
                    .iter()
                    .any(|repo| repo.project.as_ref() == Some(&project.metadata.name) && repo.source == binding.source)
                {
                    continue;
                }
                let Some(forge) = subject_forge(&forges, &binding.source.service) else { continue };
                references.repositories.push(RepositoryAlias {
                    project: Some(project.metadata.name.clone()),
                    alias: alias.clone(),
                    source: binding.source.clone(),
                    web_base: forge.spec.https_url.clone(),
                    forge_alias: Some(forge.spec.forge_id.clone()),
                });
            }
        }
        SubjectCatalogInput { change_requests, issues, forges, references, now: Some(chrono::Utc::now()) }
    }
}
