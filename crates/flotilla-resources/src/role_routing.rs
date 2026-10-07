//! Live subscription routing. Role names carry no routing semantics: local
//! presence, inherited shape, ancestry and subscription priority determine reach.
use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::{
    resolve_message_receiver, Convoy, Project, ProjectHierarchy, ReadResourceObject, ResolvedCascade, ResourceBackend, ResourceError,
    TerminalSession, CONVOY_LABEL, ROLE_LABEL, VESSEL_LABEL,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleContact {
    pub address: String,
    pub relation: crate::MessageRelation,
    pub project: String,
    pub terminal: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrewAddressBook {
    pub own_address: String,
    pub supervision: Vec<RoleContact>,
    pub contacts: Vec<RoleContact>,
}

impl CrewAddressBook {
    pub fn render(&self) -> String {
        let mut text = format!("## Address book\n\nOwn address: `{}`\n\nSupervision path:\n", self.own_address);
        for contact in &self.supervision {
            text.push_str(&format!("- `{}` (supervisor, project `{}`)\n", contact.address, contact.project));
        }
        if self.supervision.is_empty() {
            text.push_str("- No current subscriber; inspect the charter's subscriptions and holder declarations.\n");
        }
        for contact in &self.contacts {
            text.push_str(&format!("- `{}` ({:?}, project `{}`)\n", contact.address, contact.relation, contact.project));
        }
        text.push_str("\nRefresh with `flotilla message contacts`.\n");
        text
    }
}

/// `topic:<project>/<name>` is an address, independent of any role spelling.
pub fn parse_topic_address(address: &str) -> Option<(&str, &str)> {
    let (project, topic) = address.strip_prefix("topic:")?.split_once('/')?;
    (!project.is_empty()
        && !topic.is_empty()
        && !topic.contains('/')
        && !topic.contains(':')
        && !project.contains(':')
        && !matches!(project, "." | "..")
        && !matches!(topic, "." | ".."))
    .then_some((project, topic))
}

/// Resolve nearest-first supervision recipients, skipping absent holders. Within
/// a Project, priorities express the charter's governor/operator/owner sequence.
/// A source holder is excluded so a stalled governor advances rather than loops.
pub async fn supervision_path(
    backend: &ResourceBackend,
    namespace: &str,
    project: &str,
    sender: &str,
) -> Result<Vec<RoleContact>, ResourceError> {
    topic_contacts(backend, namespace, project, "supervision", Some(sender), true).await
}

pub async fn topic_contacts(
    backend: &ResourceBackend,
    namespace: &str,
    project: &str,
    topic: &str,
    sender: Option<&str>,
    fallback: bool,
) -> Result<Vec<RoleContact>, ResourceError> {
    let hierarchy = ProjectHierarchy::load_for_inspection(backend, namespace).await?;
    let projects = backend.definitions::<Project>(namespace).list().await?;
    if !projects.iter().any(|object| object.metadata.name == project) {
        return Ok(Vec::new());
    }
    let ancestors = hierarchy.ancestors(project)?;
    let mut result = Vec::new();
    let mut seen = BTreeSet::new();
    for (depth, name) in std::iter::once(project.to_string()).chain(ancestors).enumerate() {
        let object = projects.iter().find(|object| object.metadata.name == name).ok_or_else(|| ResourceError::not_found(&name))?;
        let cascade = ResolvedCascade::load(backend, namespace, &name, &object.spec).await?;
        let mut local = Vec::new();
        for (role, definition) in &cascade.roles {
            for subscription in definition.subscriptions.iter().flatten() {
                if subscription.topic != topic || (!fallback && depth > 0 && !subscription.subtree) {
                    continue;
                }
                if definition.principal.is_some()
                    && object.spec.role_definitions.get(role).and_then(|role| role.principal.as_ref()).is_none()
                {
                    continue;
                }
                let address = definition.principal.clone().unwrap_or_else(|| format!("{name}/{role}"));
                let holder =
                    if definition.principal.is_some() { None } else { resolve_message_receiver(backend, namespace, &address).await? };
                // Principal roles are declared terminal recipients. Automated roles
                // exist only with current holders, not merely inherited definitions.
                if holder.is_none() && definition.principal.is_none() {
                    continue;
                }
                if sender.is_some_and(|sender| {
                    sender == address || holder.as_ref().is_some_and(|holder| terminal_has_address(&holder.object, &name, sender))
                }) {
                    continue;
                }
                local.push((
                    subscription.priority,
                    RoleContact {
                        address,
                        relation: crate::MessageRelation::Supervisor,
                        project: name.clone(),
                        terminal: holder.map(|holder| holder.object.metadata.name),
                    },
                ));
            }
        }
        local.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.address.cmp(&b.1.address)));
        result.extend(local.into_iter().map(|(_, contact)| contact).filter(|contact| seen.insert(contact.address.clone())));
    }
    Ok(result)
}

fn terminal_has_address(terminal: &crate::ResourceObject<TerminalSession>, project: &str, address: &str) -> bool {
    let parts: Vec<_> = address.split('/').collect();
    matches!(parts.as_slice(), [p, convoy, vessel, role]
        if *p == project
        && terminal.metadata.labels.get(CONVOY_LABEL).is_some_and(|value| value == convoy)
        && terminal.metadata.labels.get(VESSEL_LABEL).is_some_and(|value| value == vessel)
        && terminal.metadata.labels.get(ROLE_LABEL).is_some_and(|value| value == role))
}

pub async fn crew_address_book(backend: &ResourceBackend, namespace: &str, own_address: &str) -> Result<CrewAddressBook, ResourceError> {
    crate::validate_message_address(own_address)?;
    let parts: Vec<_> = own_address.split('/').collect();
    let project = parts.first().copied().ok_or_else(|| ResourceError::invalid("crew address requires a Project"))?;
    let supervision = supervision_path(backend, namespace, project, own_address).await?;
    let mut contacts = Vec::new();
    let convoys = backend.including_replicas::<Convoy>(namespace).list().await?.items;
    let hierarchy = ProjectHierarchy::load_for_inspection(backend, namespace).await?;
    let projects = backend.definitions::<Project>(namespace).list().await?;
    let subtree = if let Some(object) = projects.iter().find(|object| object.metadata.name == project) {
        let cascade = ResolvedCascade::load(backend, namespace, project, &object.spec).await?;
        hierarchy.fleet() == Some(project)
            || parts.last().is_some_and(|role| {
                cascade.roles.get(*role).is_some_and(|role| role.subscriptions.iter().flatten().any(|subscription| subscription.subtree))
            })
    } else {
        false
    };
    let mut visible_projects = BTreeSet::from([project.to_string()]);
    if subtree {
        visible_projects.extend(hierarchy.descendants(project)?);
    }
    for source in convoys {
        let convoy = source.object;
        let candidate_project = convoy.spec.project_ref.as_deref().unwrap_or(namespace);
        if !visible_projects.contains(candidate_project) || convoy.status.as_ref().is_some_and(|status| status.phase.is_terminal()) {
            continue;
        }
        if !subtree && parts.len() == 4 && parts[1] != convoy.metadata.name {
            continue;
        }
        if let Some(status) = &convoy.status {
            for (vessel, crew) in &status.crew_work {
                for role in crew.keys() {
                    let address = format!("{candidate_project}/{}/{vessel}/{role}", convoy.metadata.name);
                    if address != own_address {
                        let holder = resolve_message_receiver(backend, namespace, &address).await?;
                        contacts.push(RoleContact {
                            address,
                            relation: crate::MessageRelation::Peer,
                            project: candidate_project.into(),
                            terminal: holder.map(|holder| holder.object.metadata.name),
                        });
                    }
                }
            }
        }
    }
    contacts.sort_by(|a, b| a.address.cmp(&b.address));
    Ok(CrewAddressBook { own_address: own_address.into(), supervision, contacts })
}

/// Topic delivery uses this seam; direct address resolution never calls back
/// into it, avoiding recursive graph walks and stale holder caches.
pub async fn resolve_topic_receiver(
    backend: &ResourceBackend,
    namespace: &str,
    address: &str,
    sender: &str,
) -> Result<Option<ReadResourceObject<TerminalSession>>, ResourceError> {
    let (project, topic) = parse_topic_address(address).ok_or_else(|| ResourceError::invalid("invalid topic address"))?;
    let contacts = topic_contacts(backend, namespace, project, topic, Some(sender), topic == "supervision").await?;
    if let Some(contact) = contacts.first() {
        resolve_message_receiver(backend, namespace, &contact.address).await
    } else {
        Ok(None)
    }
}

/// Rendering belongs to the brief owner; resource reconciliation owns durable
/// notification identity and supersession. The inputs are one observed revision.
#[derive(Debug, Clone, Copy, bon::Builder)]
pub struct CharterBriefInput<'a> {
    pub project: &'a crate::ResourceObject<Project>,
    pub convoy: Option<&'a crate::ResourceObject<Convoy>>,
    pub holder: &'a crate::ResourceObject<TerminalSession>,
    pub role: &'a str,
    pub revision: &'a str,
}

#[async_trait::async_trait]
pub trait CharterBriefRenderer: Send + Sync {
    async fn render(&self, input: CharterBriefInput<'_>) -> Result<String, ResourceError>;
}

/// The storage contract's renderer is useful for holders whose first-turn
/// lifecycle is owned externally (for example an adopted attended session).
pub struct CharterProseRenderer;

#[async_trait::async_trait]
impl CharterBriefRenderer for CharterProseRenderer {
    async fn render(&self, input: CharterBriefInput<'_>) -> Result<String, ResourceError> {
        let prose = input
            .project
            .spec
            .charter_prose
            .get("*")
            .into_iter()
            .chain(input.project.spec.charter_prose.get(input.role))
            .cloned()
            .collect::<Vec<_>>()
            .join("\n\n");
        Ok(format!("Charter revision: `{}`\n\n{prose}", input.revision))
    }
}

/// Applied revisions generate receiver-homed notifications. Deterministic IDs
/// make a restart idempotent; ordinary subject supersession coalesces revisions
/// until the holder reaches a turn boundary.
pub async fn reconcile_charter_notifications(inbox: &crate::MessageInbox, now: chrono::DateTime<chrono::Utc>) -> Result<(), ResourceError> {
    reconcile_charter_notifications_with_renderer(inbox, &CharterProseRenderer, now).await
}

pub async fn reconcile_charter_notifications_with_renderer(
    inbox: &crate::MessageInbox,
    renderer: &dyn CharterBriefRenderer,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<(), ResourceError> {
    use crate::{InputMeta, MessageExpectation, MessageReference, MessageRelation, MessageSpec, ResourceProvenance, TerminalSessionSource};
    use sha2::{Digest, Sha256};
    let _admission = inbox.admission.lock().await;
    let backend = &inbox.backend;
    let namespace = inbox.namespace.as_str();
    let projects = backend.definitions::<Project>(namespace).list().await?;
    let convoys = backend.including_replicas::<Convoy>(namespace).list().await?.items;
    for holder in backend.including_replicas::<TerminalSession>(namespace).list().await?.items {
        if !matches!(holder.provenance, ResourceProvenance::Local) {
            continue;
        }
        let TerminalSessionSource::Agent { context, brief, .. } = &holder.object.spec.source else { continue };
        let labels = &holder.object.metadata.labels;
        let convoy = convoys.iter().find(|convoy| convoy.object.metadata.name == context.convoy).map(|source| &source.object);
        let adopted = holder.object.metadata.annotations.get(crate::ROLE_ADDRESS_ANNOTATION).and_then(|address| address.split_once('/'));
        let (project_name, role, receiver) = if let Some((project, role)) = adopted {
            if role.contains('/') {
                return Err(ResourceError::invalid("adopted role claim must be a Project role address"));
            }
            let address = format!("{project}/{role}");
            let Some(current) = resolve_message_receiver(backend, namespace, &address).await? else { continue };
            if current.object.metadata.name != holder.object.metadata.name {
                continue;
            }
            (project.to_string(), role.to_string(), address)
        } else {
            let Some(project) = convoy.and_then(|convoy| convoy.spec.project_ref.as_ref()) else { continue };
            let (Some(vessel), Some(role)) = (labels.get(VESSEL_LABEL), labels.get(ROLE_LABEL)) else { continue };
            (project.clone(), role.clone(), format!("{project}/{}/{vessel}/{role}", context.convoy))
        };
        let Some(project) = projects.iter().find(|project| project.metadata.name == project_name) else { continue };
        let cascade = ResolvedCascade::load(backend, namespace, &project_name, &project.spec).await?;
        let Some(revision) = cascade.charter_commit else { continue };
        if holder.object.metadata.annotations.get("flotilla.work/brief-charter-revision") == Some(&revision)
            || brief.content.contains(&format!("Charter revision: `{revision}`"))
        {
            continue;
        }
        let name = format!("charter-{:x}", Sha256::digest(format!("{receiver}\0{revision}").as_bytes()));
        match inbox.messages.get(&name).await {
            Ok(existing) => {
                if existing.status.is_none() {
                    inbox.accept_locked(&InputMeta::from(&existing.metadata), &existing.spec, now).await?;
                }
                continue;
            }
            Err(ResourceError::NotFound { .. }) => {}
            Err(error) => return Err(error),
        }
        let predecessor = inbox.messages.query(&crate::MessageQuery::Active { receiver: Some(receiver.clone()) }).await?
            .into_iter().filter(|message| message.spec.sender == "system:fleet-store" && message.spec.subject.as_ref().is_some_and(|subject| {
                matches!(subject, MessageReference::ControlRecord { resource, .. } if resource.kind == "Project" && resource.namespace == namespace && resource.name == project_name)
            })).max_by_key(|message| message.metadata.creation_timestamp).map(|message| message.metadata.name);
        let subject = MessageReference::ControlRecord {
            resource: flotilla_protocol::ResourceRef::new("flotilla.work/v1", "Project", namespace, &project_name),
            revision: revision.clone(),
        };
        let rendered = renderer
            .render(
                CharterBriefInput::builder()
                    .project(project)
                    .maybe_convoy(convoy)
                    .holder(&holder.object)
                    .role(&role)
                    .revision(&revision)
                    .build(),
            )
            .await?;
        let spec = MessageSpec::builder()
            .sender("system:fleet-store".into())
            .receiver(receiver)
            .relation(MessageRelation::System)
            .body(format!("Project charter updated: charter@{revision}\n\n{rendered}"))
            .references(vec![subject.clone()])
            .subject(subject)
            .expectation(MessageExpectation::None)
            .maybe_supersedes(predecessor)
            .build();
        inbox.accept_locked(&InputMeta::builder().name(name).build(), &spec, now).await?;
    }
    Ok(())
}
