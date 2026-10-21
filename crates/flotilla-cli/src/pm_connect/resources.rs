//! Replicated subject observations held by the PM connector.
use std::collections::{BTreeMap, BTreeSet};

use flotilla_manifest::projection::{subject_forge, SubjectCatalogInput};
use flotilla_protocol::{IssueSource, RepositoryAlias, ResourceReadEnvelope, ResourceRecordProvenance, ResourceRecordType};
use flotilla_resources::{
    Artifact, ChangeRequest, Convoy, Forge, Issue, K8sResourceObject, Message, Project, Repository, RepositoryIdentity, Resource,
    ResourceObject,
};
use serde_json::Value;
use tracing::warn;

pub(super) const KINDS: &[&str] = &["changerequests", "issues", "forges", "repositories", "projects", "convoys", "artifacts", "messages"];

// Decode at admission, retaining typed records so projection cannot panic
// while trying to decode an already accepted JSON value again.
#[derive(Clone)]
enum SubjectRecord {
    Convoy(Box<ResourceObject<Convoy>>),
    Artifact(ResourceObject<Artifact>),
    Message(Box<ResourceObject<Message>>),
    ChangeRequest(ResourceObject<ChangeRequest>),
    Issue(ResourceObject<Issue>),
    Forge(ResourceObject<Forge>),
    Repository(ResourceObject<Repository>),
    Project(Box<ResourceObject<Project>>),
}
impl SubjectRecord {
    fn observed_at(&self) -> Option<chrono::DateTime<chrono::Utc>> {
        match self {
            Self::Artifact(record) => record.spec.recorded_at,
            Self::Message(record) => record.status.as_ref().map(|status| status.since),
            Self::ChangeRequest(record) => record.status.as_ref().and_then(|status| {
                [
                    status.title.observed_at,
                    status.author.observed_at,
                    status.state.observed_at,
                    status.head_sha.observed_at,
                    status.checks.observed_at,
                    status.mergeable.observed_at,
                    status.review.actionable_at_head.observed_at,
                    status.review_decision.observed_at,
                    status.review_requested_from_owner.observed_at,
                ]
                .into_iter()
                .max()
            }),
            Self::Issue(record) => record.status.as_ref().and_then(|status| {
                [
                    status.title.observed_at,
                    status.state.observed_at,
                    status.labels.observed_at,
                    status.assignees.observed_at,
                    status.updated_at.observed_at,
                ]
                .into_iter()
                .max()
            }),
            _ => None,
        }
    }
}
trait RecordType: Resource + Clone {
    fn from_record(record: &SubjectRecord) -> Option<&ResourceObject<Self>>;
}
macro_rules! record_type {
    ($type:ty, $variant:ident) => {
        impl RecordType for $type {
            fn from_record(record: &SubjectRecord) -> Option<&ResourceObject<Self>> {
                match record {
                    SubjectRecord::$variant(object) => Some(object),
                    _ => None,
                }
            }
        }
    };
}
record_type!(Convoy, Convoy);
record_type!(Artifact, Artifact);
record_type!(Message, Message);
record_type!(ChangeRequest, ChangeRequest);
record_type!(Issue, Issue);
record_type!(Forge, Forge);
record_type!(Repository, Repository);
record_type!(Project, Project);

pub(super) type RecordKey = (String, String, String, String);

#[derive(Default)]
pub(super) struct Records {
    // Retain each source separately: deleting one replica must not erase the
    // same observation still held by another root.
    objects: BTreeMap<(String, String, String, String), SubjectRecord>,
    local_roots: BTreeSet<String>,
    changed: BTreeSet<RecordKey>,
    configuration: Option<std::sync::Arc<SubjectCatalogInput>>,
    source_configuration: BTreeMap<IssueSource, (Vec<ResourceObject<Forge>>, Vec<RepositoryAlias>)>,
}

fn decode<T: Resource>(value: &Value) -> Result<ResourceObject<T>, String> {
    let object: K8sResourceObject<T> = serde_json::from_value(value.clone()).map_err(|error| error.to_string())?;
    ResourceObject::from_k8s_object(object).map_err(|error| error.to_string())
}

impl Records {
    pub(super) fn clear_scope(&mut self, namespace: &str, kind: &str) {
        // The ordered key puts this scope in one contiguous range. Recovery
        // need not inspect records held by unrelated kinds or namespaces.
        let keys: Vec<_> = self
            .objects
            .range((kind.to_owned(), namespace.to_owned(), String::new(), String::new())..)
            .take_while(|((plural, ns, _, _), _)| plural == kind && ns == namespace)
            .map(|(key, _)| key.clone())
            .collect();
        for key in keys {
            self.objects.remove(&key);
            self.changed.insert(key);
        }
        if matches!(kind, "forges" | "repositories" | "projects") {
            self.configuration = None;
            self.source_configuration.clear();
        }
    }

    pub(super) fn apply(&mut self, envelope: &ResourceReadEnvelope) -> Result<(), String> {
        if !KINDS.contains(&envelope.plural.as_str()) {
            return Err(format!("unexpected subject catalog resource {}", envelope.plural));
        }
        for record in &envelope.records {
            if record.record_type == ResourceRecordType::Bookmark {
                continue;
            }
            let Some(object) = &record.object else { continue };
            let root = match &record.provenance {
                ResourceRecordProvenance::Local { node_id } => node_id.to_string(),
                ResourceRecordProvenance::Replica { origin_root, .. } => origin_root.to_string(),
            };
            if matches!(record.provenance, ResourceRecordProvenance::Local { .. }) {
                self.local_roots.insert(root.clone());
            }
            let Some(name) = object.pointer("/metadata/name").and_then(Value::as_str) else {
                warn!(kind = %envelope.plural, "ignoring subject resource without a name");
                continue;
            };
            let namespace = object.pointer("/metadata/namespace").and_then(Value::as_str).unwrap_or(&envelope.namespace);
            let key = (envelope.plural.clone(), namespace.to_owned(), name.to_owned(), root);
            self.changed.insert(key.clone());
            if matches!(envelope.plural.as_str(), "forges" | "repositories" | "projects") {
                self.configuration = None;
                self.source_configuration.clear();
            }
            if record.record_type == ResourceRecordType::Deleted {
                self.objects.remove(&key);
                continue;
            }
            let decoded = match envelope.plural.as_str() {
                "convoys" => decode::<Convoy>(object).map(Box::new).map(SubjectRecord::Convoy),
                "artifacts" => decode::<Artifact>(object).map(SubjectRecord::Artifact),
                "messages" => decode::<Message>(object).map(Box::new).map(SubjectRecord::Message),
                "changerequests" => decode::<ChangeRequest>(object).map(SubjectRecord::ChangeRequest),
                "issues" => decode::<Issue>(object).map(SubjectRecord::Issue),
                "forges" => decode::<Forge>(object).map(SubjectRecord::Forge),
                "repositories" => decode::<Repository>(object).map(SubjectRecord::Repository),
                "projects" => decode::<Project>(object).map(Box::new).map(SubjectRecord::Project),
                _ => Err(format!("unexpected subject catalog resource {}", envelope.plural)),
            };
            match decoded {
                Ok(object) => {
                    self.objects.insert(key, object);
                }
                Err(error) => {
                    warn!(kind = %envelope.plural, %name, %error, "ignoring undecodable subject resource");
                    // A broken update must not keep reasserting an older value
                    // from this root as if it were still a valid observation.
                    self.objects.remove(&key);
                }
            }
        }
        Ok(())
    }

    pub(super) fn forget_source(&mut self, source: &IssueSource) {
        self.source_configuration.remove(source);
    }

    pub(super) fn take_changes(&mut self) -> BTreeSet<RecordKey> {
        std::mem::take(&mut self.changed)
    }

    pub(super) fn links(&self, key: &RecordKey) -> Option<Vec<super::projection::Dependency>> {
        use super::projection::Dependency;
        use flotilla_protocol::{IssueSource, Subject, SubjectKind};
        use flotilla_resources::MessageReference;
        let record = self.objects.get(key)?;
        let convoy = |namespace: &str, name: &str| Dependency::Convoy(namespace.to_owned(), name.to_owned());
        let mut links = Vec::new();
        match record {
            SubjectRecord::Convoy(row) => links.push(convoy(&row.metadata.namespace, &row.metadata.name)),
            SubjectRecord::Artifact(row) => links.push(convoy(&row.metadata.namespace, &row.spec.convoy)),
            SubjectRecord::Message(row) => {
                for address in [&row.spec.sender, &row.spec.receiver] {
                    let parts: Vec<_> = address.split('/').collect();
                    if parts.len() == 4 {
                        links.push(convoy(&row.metadata.namespace, parts[1]));
                    }
                }
                for reference in &row.spec.references {
                    if let MessageReference::ControlRecord { resource, .. } = reference {
                        if resource.kind == "Convoy" {
                            links.push(convoy(&resource.namespace, &resource.name));
                        }
                    }
                }
            }
            SubjectRecord::ChangeRequest(row) => {
                links.push(Dependency::Subject(Subject {
                    kind: SubjectKind::ChangeRequest,
                    source: IssueSource { service: row.spec.service.clone(), scope: row.spec.scope.clone() },
                    id: row.spec.number.to_string(),
                }));
                for history in &row.spec.subject_of {
                    links.push(convoy(&history.namespace, &history.convoy));
                }
            }
            SubjectRecord::Issue(row) => links.push(Dependency::Subject(Subject {
                kind: SubjectKind::Issue,
                source: IssueSource { service: row.spec.service.clone(), scope: row.spec.scope.clone() },
                id: row.spec.number.to_string(),
            })),
            SubjectRecord::Forge(_) | SubjectRecord::Repository(_) | SubjectRecord::Project(_) => return None,
        }
        Some(links)
    }

    pub(super) fn configuration(&mut self) -> std::sync::Arc<SubjectCatalogInput> {
        if self.configuration.is_none() {
            let mut definitions = Self { local_roots: self.local_roots.clone(), ..Self::default() };
            for kind in ["forges", "repositories", "projects"] {
                for (key, value) in self
                    .objects
                    .range((kind.to_owned(), String::new(), String::new(), String::new())..)
                    .take_while(|(key, _)| key.0 == kind)
                {
                    definitions.objects.insert(key.clone(), value.clone());
                }
            }
            self.configuration = Some(std::sync::Arc::new(definitions.projection()));
        }
        self.configuration.as_ref().expect("configuration just populated").clone()
    }

    pub(super) fn projection_selected(
        &mut self,
        keys: &[RecordKey],
        sources: &BTreeSet<IssueSource>,
        configuration: &SubjectCatalogInput,
    ) -> SubjectCatalogInput {
        let mut selected = Self {
            local_roots: keys.iter().filter_map(|key| self.local_roots.contains(&key.3).then_some(key.3.clone())).collect(),
            ..Self::default()
        };
        for key in keys {
            if let Some(record) = self.objects.get(key) {
                selected.objects.insert(key.clone(), record.clone());
            }
        }
        let mut result = selected.projection();
        for source in sources {
            let (forges, references) = self.source_configuration.entry(source.clone()).or_insert_with(|| {
                let owner = subject_forge(&configuration.forges, &source.service);
                let forges = owner.cloned().into_iter().collect();
                let references = configuration
                    .references
                    .repositories
                    .iter()
                    .filter(|reference| {
                        reference.source.scope == source.scope
                            && owner.is_some_and(|forge| {
                                subject_forge(&configuration.forges, &reference.source.service)
                                    .is_some_and(|other| other.spec.forge_id == forge.spec.forge_id)
                            })
                    })
                    .map(|reference| {
                        let mut reference = reference.clone();
                        reference.source = source.clone();
                        reference
                    })
                    .collect();
                (forges, references)
            });
            for forge in forges {
                if !result.forges.iter().any(|existing| {
                    existing.metadata.name == forge.metadata.name && existing.metadata.namespace == forge.metadata.namespace
                }) {
                    result.forges.push(forge.clone());
                }
            }
            result.references.repositories.extend(references.iter().cloned());
        }
        result
    }

    fn typed<T: RecordType>(&self, kind: &str) -> Vec<ResourceObject<T>> {
        let mut objects = BTreeMap::new();
        for ((plural, namespace, name, root), record) in &self.objects {
            if plural != kind {
                continue;
            }
            let Some(object) = T::from_record(record) else { continue };
            // Admission parsed timestamps into UTC. Choose the freshest observed
            // field, then origin root; never use receiving-host resourceVersion
            // or serialized object fields. Definitions use the root tie-break.
            let key = (namespace.clone(), name.clone());
            let authoritative = matches!(record, SubjectRecord::Convoy(_) | SubjectRecord::Artifact(_) | SubjectRecord::Message(_))
                && self.local_roots.contains(root);
            let stamp = (authoritative, record.observed_at(), root);
            if objects.get(&key).is_none_or(|(prior, _)| &stamp > prior) {
                objects.insert(key, (stamp, object.clone()));
            }
        }
        objects.into_values().map(|(_, object)| object).collect()
    }

    pub(super) fn projection(&self) -> SubjectCatalogInput {
        let forges = self.typed::<Forge>("forges");
        let repositories = self.typed::<Repository>("repositories");
        let projects = self.typed::<Project>("projects");
        let change_requests = flotilla_resources::select_change_requests(self.objects.values().filter_map(|record| match record {
            SubjectRecord::ChangeRequest(object) => Some(object),
            _ => None,
        }))
        .into_values()
        .collect::<Vec<_>>();
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
        SubjectCatalogInput {
            convoys: self.typed::<Convoy>("convoys"),
            artifacts: self.typed::<Artifact>("artifacts"),
            messages: self.typed::<Message>("messages"),
            change_requests,
            issues,
            forges,
            references,
            now: None,
        }
    }
}
