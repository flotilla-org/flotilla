//! Live subscription routing. Role names carry no routing semantics: local
//! presence, inherited shape, ancestry and subscription priority determine reach.
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Write,
};

use serde::{Deserialize, Serialize};

use crate::{
    resolve_message_receiver, Convoy, Project, ProjectHierarchy, ReadResourceObject, ResolvedCascade, ResourceBackend, ResourceError,
    TerminalSession, CONVOY_LABEL, ROLE_LABEL, VESSEL_LABEL,
};

pub const BRIEF_CHARTER_REVISION_ANNOTATION: &str = "flotilla.work/brief-charter-revision";
pub const FLEET_STORE_SENDER: &str = "system:fleet-store";

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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routing_issue: Option<String>,
}

impl CrewAddressBook {
    pub fn render(&self) -> String {
        let mut text = format!("## Address book\n\nOwn address: `{}`\n\nSupervision path:\n", self.own_address);
        for contact in &self.supervision {
            writeln!(text, "- `{}` (supervisor, project `{}`)", contact.address, contact.project).expect("String write");
        }
        if let Some(issue) = &self.routing_issue {
            writeln!(text, "- Routing configuration: {issue}").expect("String write");
        } else if self.supervision.is_empty() {
            text.push_str("- No current subscriber; inspect the charter's subscriptions and holder declarations.\n");
        }
        for contact in &self.contacts {
            writeln!(text, "- `{}` ({:?}, project `{}`)", contact.address, contact.relation, contact.project).expect("String write");
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
    topic_contacts(
        backend,
        namespace,
        TopicRouting::builder().project(project).topic("supervision").sender(sender).reach(TopicReach::Ancestors).build(),
    )
    .await
}

/// Ordinary topics reach subtree subscribers; supervision can fall back through
/// every ancestor regardless of subtree reach.
#[derive(Debug, Clone, Copy, Default)]
pub enum TopicReach {
    #[default]
    SubtreeSubscriptions,
    Ancestors,
}

#[derive(Debug, Clone, Copy, bon::Builder)]
pub struct TopicRouting<'a> {
    pub project: &'a str,
    pub topic: &'a str,
    pub sender: Option<&'a str>,
    #[builder(default)]
    pub reach: TopicReach,
}

pub async fn topic_contacts(
    backend: &ResourceBackend,
    namespace: &str,
    routing: TopicRouting<'_>,
) -> Result<Vec<RoleContact>, ResourceError> {
    let TopicRouting { project, topic, sender, reach } = routing;
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
                if subscription.topic != topic || (matches!(reach, TopicReach::SubtreeSubscriptions) && depth > 0 && !subscription.subtree)
                {
                    continue;
                }
                // Shape inherits, presence stays local: an inherited owner
                // principal must not invent a recipient in every child Project.
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
    let routing_issue = (!projects.iter().any(|object| object.metadata.name == project)).then(|| format!("unknown Project `{project}`"));
    Ok(CrewAddressBook { own_address: own_address.into(), supervision, contacts, routing_issue })
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
    let reach = if topic == "supervision" { TopicReach::Ancestors } else { TopicReach::SubtreeSubscriptions };
    let contacts =
        topic_contacts(backend, namespace, TopicRouting::builder().project(project).topic(topic).sender(sender).reach(reach).build())
            .await?;
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
    pub cascade: &'a ResolvedCascade,
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
    // Keep charter passes ordered without blocking unrelated Message producers.
    let _charter_pass = inbox.charter_notifications.lock().await;
    let backend = &inbox.backend;
    let namespace = inbox.namespace.as_str();
    let projects = backend.definitions::<Project>(namespace).list().await?;
    let convoys = backend.including_replicas::<Convoy>(namespace).list().await?.items;
    let mut cascades = BTreeMap::new();
    for holder in backend.including_replicas::<TerminalSession>(namespace).list().await?.items {
        if !matches!(holder.provenance, ResourceProvenance::Local) {
            continue;
        }
        let TerminalSessionSource::Agent { context, .. } = &holder.object.spec.source else { continue };
        let labels = &holder.object.metadata.labels;
        let convoy = convoys.iter().find(|convoy| convoy.object.metadata.name == context.convoy).map(|source| &source.object);
        let adopted = holder.object.metadata.annotations.get(crate::ROLE_ADDRESS_ANNOTATION);
        let (project_name, role, receiver) = if let Some(address) = adopted {
            let parts = address.split('/').collect::<Vec<_>>();
            let [project, role] = parts.as_slice() else {
                tracing::warn!(terminal = %holder.object.metadata.name, claim = %address, "ignoring malformed adopted role claim");
                continue;
            };
            if crate::validate_message_address(address).is_err() {
                tracing::warn!(terminal = %holder.object.metadata.name, claim = %address, "ignoring malformed adopted role claim");
                continue;
            }
            let current = match resolve_message_receiver(backend, namespace, address).await {
                Ok(Some(current)) => current,
                Ok(None) => continue,
                Err(error @ (ResourceError::Invalid { .. } | ResourceError::NotFound { .. })) => {
                    tracing::warn!(terminal = %holder.object.metadata.name, %error, "ignoring invalid adopted role claim");
                    continue;
                }
                Err(error) => return Err(error),
            };
            if current.object.metadata.name != holder.object.metadata.name {
                continue;
            }
            (project.to_string(), role.to_string(), address.clone())
        } else {
            let Some(project) = convoy.and_then(|convoy| convoy.spec.project_ref.as_ref()) else { continue };
            let (Some(vessel), Some(role)) = (labels.get(VESSEL_LABEL), labels.get(ROLE_LABEL)) else { continue };
            (project.clone(), role.clone(), format!("{project}/{}/{vessel}/{role}", context.convoy))
        };
        let Some(project) = projects.iter().find(|project| project.metadata.name == project_name) else { continue };
        let Some(revision) = crate::role_cascade::project_charter_revision(project) else { continue };
        if holder.object.metadata.annotations.get(BRIEF_CHARTER_REVISION_ANNOTATION) == Some(&revision) {
            continue;
        }
        let name = format!("charter-{:x}", Sha256::digest(format!("{receiver}\0{revision}").as_bytes()));
        {
            let _admission = inbox.admission.lock().await;
            if repair_existing_charter_notification(inbox, &name, now).await? {
                continue;
            }
        }
        if !cascades.contains_key(&project_name) {
            let cascade = ResolvedCascade::load(backend, namespace, &project_name, &project.spec).await?;
            cascades.insert(project_name.clone(), cascade);
        }
        let cascade = cascades.get(&project_name).expect("loaded Project cascade");
        let subject = MessageReference::ControlRecord {
            resource: flotilla_protocol::ResourceRef::new("flotilla.work/v1", "Project", namespace, &project_name),
            revision: revision.clone(),
        };
        let rendered = renderer
            .render(
                CharterBriefInput::builder()
                    .project(project)
                    .maybe_convoy(if adopted.is_some() { None } else { convoy })
                    .holder(&holder.object)
                    .role(&role)
                    .revision(&revision)
                    .cascade(cascade)
                    .build(),
            )
            .await?;
        // Only admission and predecessor selection serialize. Rendering and
        // snapshot reads never block ordinary inbox producers.
        let _admission = inbox.admission.lock().await;
        if repair_existing_charter_notification(inbox, &name, now).await? {
            continue;
        }
        let predecessor = inbox.messages.query(&crate::MessageQuery::Active { receiver: Some(receiver.clone()) }).await?
            .into_iter().filter(|message| message.spec.sender == FLEET_STORE_SENDER && message.spec.subject.as_ref().is_some_and(|subject| {
                matches!(subject, MessageReference::ControlRecord { resource, .. } if resource.kind == "Project" && resource.namespace == namespace && resource.name == project_name)
            })).max_by(|a, b| (a.metadata.creation_timestamp, &a.metadata.name).cmp(&(b.metadata.creation_timestamp, &b.metadata.name))).map(|message| message.metadata.name);
        let spec = MessageSpec::builder()
            .sender(FLEET_STORE_SENDER.into())
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

// Caller owns the admission lock. A concurrent/restarted pass uses the stored
// intent, even if rendering live contacts would now produce different prose.
async fn repair_existing_charter_notification(
    inbox: &crate::MessageInbox,
    name: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<bool, ResourceError> {
    match inbox.messages.get(name).await {
        Ok(existing) => {
            if existing.status.is_none() {
                inbox.accept_locked(&crate::InputMeta::from(&existing.metadata), &existing.spec, now).await?;
            }
            Ok(true)
        }
        Err(ResourceError::NotFound { .. }) => Ok(false),
        Err(error) => Err(error),
    }
}
