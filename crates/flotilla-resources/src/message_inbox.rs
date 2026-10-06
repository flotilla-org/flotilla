//! Admission and supersession share one receiver-side seam. Producers never
//! manipulate delivery state or choose a session holder.
use std::sync::Arc;

use chrono::{DateTime, Utc};
use tokio::sync::Mutex;

use crate::{
    apply_status_patch, InputMeta, Message, MessageExpectation, MessagePhase, MessageRelation, MessageSpec, MessageStatusPatch, Resource,
    ResourceBackend, ResourceError, ResourceObject, TypedResolver,
};

/// Creation context for convoy-relative role addresses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageAddressContext {
    pub project: String,
    pub convoy: String,
    pub vessel: String,
}

/// Validate structure without restricting role names to a closed enumeration.
/// Future topic and multicast syntax can extend this parser without changing
/// the serialized field type.
pub fn validate_message_address(address: &str) -> Result<(), ResourceError> {
    if address.is_empty() || address.chars().any(|character| character.is_whitespace() || character.is_control()) {
        return Err(ResourceError::invalid(format!("invalid message address `{address}`")));
    }
    if let Some(principal) = address.strip_prefix("principal:") {
        return if !principal.is_empty() && !principal.contains('/') {
            Ok(())
        } else {
            Err(ResourceError::invalid("invalid principal address"))
        };
    }
    let parts: Vec<_> = address.split('/').collect();
    if parts.iter().any(|part| part.is_empty() || matches!(*part, "." | "..")) || !matches!(parts.len(), 2 | 4) {
        return Err(ResourceError::invalid(format!(
            "invalid role address `{address}`; expected project/role or project/convoy/vessel/role"
        )));
    }
    Ok(())
}

pub fn qualify_message_address(address: &str, context: &MessageAddressContext) -> Result<String, ResourceError> {
    let parts: Vec<_> = address.split('/').collect();
    let qualified = match parts.as_slice() {
        [role] if !address.starts_with("principal:") => format!("{}/{}/{}/{role}", context.project, context.convoy, context.vessel),
        // Two segments already name a project holder. Convoy-relative targets
        // in another vessel use convoy/vessel/role to avoid this ambiguity.
        [convoy, vessel, role] => format!("{}/{convoy}/{vessel}/{role}", context.project),
        _ => address.to_string(),
    };
    validate_message_address(&qualified)?;
    Ok(qualified)
}

#[derive(Debug, Clone)]
pub enum MessageAdmission {
    Accepted(ResourceObject<Message>),
    Suppressed { predecessor: ResourceObject<Message> },
}

/// Clones share the admission lock. The host keeps one inbox per namespace;
/// cross-host senders call that home's ordinary resource mutation path.
#[derive(Debug, Clone)]
pub struct MessageInbox {
    messages: TypedResolver<Message>,
    admission: Arc<Mutex<()>>,
}

impl MessageInbox {
    pub fn new(backend: ResourceBackend, namespace: &str) -> Self {
        Self { messages: backend.using::<Message>(namespace), admission: Arc::new(Mutex::new(())) }
    }

    pub async fn accept(&self, meta: &InputMeta, spec: &MessageSpec, now: DateTime<Utc>) -> Result<MessageAdmission, ResourceError> {
        let _guard = self.admission.lock().await;
        Message::validate_spec(meta, spec)?;
        if spec.interrupting && spec.relation != MessageRelation::Supervisor {
            return Err(ResourceError::invalid("only supervisor messages may interrupt an active turn"));
        }
        match self.messages.get(&meta.name).await {
            Ok(existing) if existing.spec == *spec => return Ok(MessageAdmission::Accepted(existing)),
            Ok(_) => return Err(ResourceError::conflict(&meta.name, "message ID already names different intent")),
            Err(ResourceError::NotFound { .. }) => {}
            Err(error) => return Err(error),
        }
        let existing = self.messages.list().await?.items;
        if let Some(id) = &spec.supersedes {
            let predecessor = existing
                .iter()
                .find(|message| message.metadata.name == *id)
                .ok_or_else(|| ResourceError::invalid(format!("superseded message `{id}` is absent from the receiver's store")))?;
            if predecessor.spec.sender != spec.sender || predecessor.spec.receiver != spec.receiver {
                return Err(ResourceError::invalid("explicit supersession requires the same sender and receiver"));
            }
        }
        let predecessors: Vec<_> = existing.iter().filter(|message| message_supersedes(spec, message)).collect();
        if let Some(predecessor) = predecessors.iter().find(|message| message_expectation_open(message)) {
            return Ok(MessageAdmission::Suppressed { predecessor: (*predecessor).clone() });
        }
        let message = self.messages.create(meta, spec).await?;
        let status = crate::MessageStatus::builder()
            .phase(MessagePhase::Accepted)
            .since(message.metadata.creation_timestamp)
            .reason("waiting for receiver resolution".into())
            .build();
        let message = self.messages.update_status(&meta.name, &message.metadata.resource_version, &status).await?;
        for predecessor in predecessors {
            let phase = predecessor.status.as_ref().map_or(MessagePhase::Accepted, |status| status.phase);
            if !phase.has_delivery_evidence() && !phase.is_terminal() {
                apply_status_patch(&self.messages, &predecessor.metadata.name, &MessageStatusPatch::Finish {
                    phase: MessagePhase::Superseded,
                    reason: format!("replaced by {}", message.metadata.name),
                    at: now,
                })
                .await?;
            }
        }
        Ok(MessageAdmission::Accepted(message))
    }
}

pub fn message_expectation_open(message: &ResourceObject<Message>) -> bool {
    message.status.as_ref().is_some_and(|status| status.phase == MessagePhase::Delivered)
        && message.spec.expectation != MessageExpectation::None
}

pub fn message_supersedes(successor: &MessageSpec, predecessor: &ResourceObject<Message>) -> bool {
    successor.sender == predecessor.spec.sender
        && successor.receiver == predecessor.spec.receiver
        && (successor.supersedes.as_deref() == Some(predecessor.metadata.name.as_str())
            || successor.subject.as_ref().is_some_and(|subject| predecessor.spec.subject.as_ref() == Some(subject)))
}

/// Resolve a role against current durable convoy and terminal records. This
/// reads the federated view for routing; the delivery host additionally checks
/// that the returned terminal is locally authored.
pub async fn resolve_message_receiver(
    backend: &ResourceBackend,
    namespace: &str,
    address: &str,
) -> Result<Option<crate::ReadResourceObject<crate::TerminalSession>>, ResourceError> {
    use crate::{Convoy, ResourceProvenance, TerminalSession, TerminalSessionSource, CONVOY_LABEL, ROLE_LABEL, VESSEL_LABEL};
    validate_message_address(address)?;
    let parts: Vec<_> = address.split('/').collect();
    let convoys = backend.including_replicas::<Convoy>(namespace).list().await?.items;
    let (project, convoy, vessel, role) = match parts.as_slice() {
        [project, convoy, vessel, role] => (*project, Some(*convoy), Some(*vessel), *role),
        [project, role] if *project != "fleet" => (*project, None, None, *role),
        // Fleet and principal holders will be declared by #2658. Until then,
        // an address with no declared holder waits; never guess a terminal.
        _ => return Ok(None),
    };
    let mut candidates: Vec<_> = convoys
        .into_iter()
        .filter(|source| {
            let object = &source.object;
            object.spec.project_ref.as_deref() == Some(project)
                && object.status.as_ref().is_none_or(|status| !status.phase.is_terminal())
                && convoy.map_or(object.spec.role == role, |name| object.metadata.name == name)
        })
        .collect();
    candidates.sort_by_key(|source| std::cmp::Reverse(source.object.spec.generation));
    let Some(convoy) = candidates.first() else {
        return Ok(None);
    };
    let terminals = backend.including_replicas::<TerminalSession>(namespace).list().await?.items;
    let mut holders = terminals
        .into_iter()
        .filter(|source| {
            let object = &source.object;
            object.metadata.labels.get(CONVOY_LABEL) == Some(&convoy.object.metadata.name)
                && object.metadata.labels.get(ROLE_LABEL).is_some_and(|value| value == role)
                && vessel.is_none_or(|vessel| object.metadata.labels.get(VESSEL_LABEL).is_some_and(|value| value == vessel))
                && matches!(object.spec.source, TerminalSessionSource::Agent { .. })
        })
        .collect::<Vec<_>>();
    // Local authority shadows a self-origin replica; the read resolver already
    // deduplicates that pair. Multiple independent holders are not a guess.
    holders.sort_by_key(|source| (matches!(source.provenance, ResourceProvenance::Replica { .. }), source.object.metadata.name.clone()));
    match holders.len() {
        0 => Ok(None),
        1 => Ok(holders.pop()),
        _ => Err(ResourceError::invalid(format!("role address `{address}` has multiple current holders"))),
    }
}
