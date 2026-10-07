//! Admission and supersession share one receiver-side seam. Producers never
//! manipulate delivery state or choose a session holder.
use std::sync::Arc;

use chrono::{DateTime, Utc};
use tokio::sync::Mutex;

use crate::{
    apply_status_patch, InputMeta, Message, MessageExpectation, MessagePhase, MessageQuery, MessageSpec, MessageStatusPatch, Resource,
    ResourceBackend, ResourceError, ResourceObject, TypedResolver,
};

/// Adoption controllers claim a live terminal for an adoptable Project role.
/// Removing the claim or stopping the terminal retires that local presence.
pub const ROLE_ADDRESS_ANNOTATION: &str = "flotilla.work/role-address";

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
    if address.starts_with("topic:") {
        return if crate::parse_topic_address(address).is_some() { Ok(()) } else { Err(ResourceError::invalid("invalid topic address")) };
    }
    if let Some(principal) = address.strip_prefix("principal:").or_else(|| address.strip_prefix("system:")) {
        return if !principal.is_empty() && !principal.contains('/') {
            Ok(())
        } else {
            Err(ResourceError::invalid("invalid principal or system address"))
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
        [role] if !address.starts_with("principal:") && !address.starts_with("system:") => {
            format!("{}/{}/{}/{role}", context.project, context.convoy, context.vessel)
        }
        // Two segments already name a project holder. Convoy-relative targets
        // in another vessel use convoy/vessel/role to avoid this ambiguity.
        [convoy, vessel, role] => format!("{}/{convoy}/{vessel}/{role}", context.project),
        _ => address.to_string(),
    };
    validate_message_address(&qualified)?;
    Ok(qualified)
}

/// Ordinary resource mutations can qualify a relative receiver when the sender
/// already carries its convoy context. Bare sender roles require accept_in_context.
pub fn qualify_message_spec(mut spec: MessageSpec) -> Result<MessageSpec, ResourceError> {
    validate_message_address(&spec.sender)?;
    if let [project, convoy, vessel, _] = spec.sender.split('/').collect::<Vec<_>>().as_slice() {
        spec.receiver = qualify_message_address(
            &spec.receiver,
            &MessageAddressContext { project: (*project).into(), convoy: (*convoy).into(), vessel: (*vessel).into() },
        )?;
    }
    validate_message_address(&spec.receiver)?;
    Ok(spec)
}

#[derive(Debug, Clone)]
pub enum MessageAdmission {
    Accepted(ResourceObject<Message>),
    Suppressed { predecessor: ResourceObject<Message> },
}

/// Clones share the admission lock. The host keeps one inbox per namespace;
/// cross-host senders call that home's ordinary resource mutation path. The
/// routing authority must have a single writer; this lock is process-local.
/// Namespace inboxes live for the daemon lifetime and are reused by mutations.
#[derive(Debug, Clone)]
pub struct MessageInbox {
    pub(crate) backend: ResourceBackend,
    pub(crate) namespace: String,
    pub(crate) audit_retention_days: u64,
    pub(crate) change_request_stale_after: std::time::Duration,
    pub(crate) issue_stale_after: std::time::Duration,
    pub(crate) messages: TypedResolver<Message>,
    pub(crate) admission: Arc<Mutex<()>>,
    pub(crate) delivery: Arc<Mutex<()>>,
}

impl MessageInbox {
    pub fn new(backend: ResourceBackend, namespace: &str) -> Self {
        Self {
            messages: backend.using::<Message>(namespace),
            backend,
            namespace: namespace.into(),
            audit_retention_days: 30,
            admission: Arc::new(Mutex::new(())),
            delivery: Arc::new(Mutex::new(())),
            change_request_stale_after: std::time::Duration::from_secs(300),
            issue_stale_after: std::time::Duration::from_secs(300),
        }
    }

    pub fn with_audit_retention_days(mut self, days: u64) -> Self {
        self.audit_retention_days = days;
        self
    }

    pub fn with_observation_staleness(mut self, change_request: std::time::Duration, issue: std::time::Duration) -> Self {
        self.change_request_stale_after = change_request;
        self.issue_stale_after = issue;
        self
    }

    /// Qualify relative addresses with the sender's explicit creation context.
    pub async fn accept_in_context(
        &self,
        meta: &InputMeta,
        spec: &MessageSpec,
        context: &MessageAddressContext,
        now: DateTime<Utc>,
    ) -> Result<MessageAdmission, ResourceError> {
        let mut qualified = spec.clone();
        qualified.sender = qualify_message_address(&qualified.sender, context)?;
        qualified.receiver = qualify_message_address(&qualified.receiver, context)?;
        self.accept(meta, &qualified, now).await
    }

    /// Admit an immutable intent. Bare senders require `accept_in_context`; a fully
    /// qualified sender supplies context for a relative receiver.
    pub async fn accept(&self, meta: &InputMeta, spec: &MessageSpec, now: DateTime<Utc>) -> Result<MessageAdmission, ResourceError> {
        let _guard = self.admission.lock().await;
        self.accept_locked(meta, spec, now).await
    }

    // Recovery during reconciliation already owns the shared admission lock.
    pub(crate) async fn accept_locked(
        &self,
        meta: &InputMeta,
        spec: &MessageSpec,
        now: DateTime<Utc>,
    ) -> Result<MessageAdmission, ResourceError> {
        Message::validate_spec(meta, spec)?;
        let partial = match self.messages.get(&meta.name).await {
            Ok(existing) if existing.spec.same_intent(spec) => {
                // Status is written only after predecessor cleanup. A missing
                // status is an unfinished admission, so replay must repair it.
                if let Some(reference) = existing.status.as_ref().and_then(|status| status.canonical_predecessor.as_ref()) {
                    if reference.namespace != self.messages.namespace {
                        return Err(ResourceError::invalid("canonical predecessor is outside the receiver inbox"));
                    }
                    let predecessor = self.messages.get(&reference.name).await?;
                    return Ok(MessageAdmission::Suppressed { predecessor });
                }
                if existing.status.is_some() {
                    return Ok(MessageAdmission::Accepted(existing));
                }
                Some(existing)
            }
            Ok(_) => return Err(ResourceError::conflict(&meta.name, "message ID already names different intent")),
            Err(ResourceError::NotFound { .. }) => None,
            Err(error) => return Err(error),
        };
        let mut existing = self.messages.query(&MessageQuery::Active { receiver: Some(spec.receiver.clone()) }).await?;
        if let Some(id) = &spec.supersedes {
            if !existing.iter().any(|message| message.metadata.name == *id) {
                existing.push(self.messages.get(id).await.map_err(|error| match error {
                    ResourceError::NotFound { .. } => {
                        ResourceError::invalid(format!("superseded message `{id}` is absent from the receiver's store"))
                    }
                    error => error,
                })?);
            }
        }
        let predecessors: Vec<_> = existing
            .iter()
            .filter(|message| {
                message.metadata.name != meta.name
                    && message_supersedes(spec, message)
                    && partial.as_ref().is_none_or(|partial| message.metadata.creation_timestamp <= partial.metadata.creation_timestamp)
            })
            .collect();
        if let Some(predecessor) = predecessors.iter().find(|message| spec.supersedes.is_none() && message_expectation_open(message)) {
            if let Some(partial) = &partial {
                apply_status_patch(
                    &self.messages,
                    &partial.metadata.name,
                    &MessageStatusPatch::Suppressed {
                        predecessor: flotilla_protocol::ResourceRef::new(
                            "flotilla.work/v1",
                            "Message",
                            &self.messages.namespace,
                            &predecessor.metadata.name,
                        ),
                        at: now,
                    },
                )
                .await?;
            }
            return Ok(MessageAdmission::Suppressed { predecessor: (*predecessor).clone() });
        }
        let message = match partial {
            Some(message) => message,
            None => self.messages.create(meta, spec).await?,
        };
        for predecessor in predecessors {
            let phase = predecessor.status.as_ref().map_or(MessagePhase::Accepted, |status| status.phase);
            if !phase.is_terminal()
                && (spec.supersedes.as_deref() == Some(predecessor.metadata.name.as_str())
                    || (!phase.has_delivery_evidence() && predecessor.status.as_ref().is_none_or(|status| status.submission.is_none())))
            {
                apply_status_patch(
                    &self.messages,
                    &predecessor.metadata.name,
                    &MessageStatusPatch::Finish {
                        phase: MessagePhase::Superseded,
                        reason: format!("replaced by {}", message.metadata.name),
                        at: now,
                    },
                )
                .await?;
            }
        }
        let status = crate::MessageStatus::builder()
            .phase(MessagePhase::Accepted)
            .since(message.metadata.creation_timestamp)
            // Kubernetes resource versions are opaque on older or extension
            // API servers and can exceed u64. Their creation timestamps and
            // immutable names give a deterministic fallback without refusing admission.
            .maybe_accepted_sequence(match &self.backend {
                ResourceBackend::Http(_) => None,
                _ => message.metadata.resource_version.parse().ok(),
            })
            .reason("waiting for receiver resolution".into())
            .build();
        let message = self.messages.update_status(&meta.name, &message.metadata.resource_version, &status).await?;
        Ok(MessageAdmission::Accepted(message))
    }
}

pub fn message_expectation_open(message: &ResourceObject<Message>) -> bool {
    message.status.as_ref().is_some_and(|status| status.phase == MessagePhase::Delivered)
        && message.spec.expectation != MessageExpectation::None
}

pub fn message_supersedes(successor: &MessageSpec, predecessor: &ResourceObject<Message>) -> bool {
    successor.supersedes.as_deref() == Some(predecessor.metadata.name.as_str())
        || (successor.sender == predecessor.spec.sender
            && successor.receiver == predecessor.spec.receiver
            && successor.subject.as_ref().is_some_and(|subject| predecessor.spec.subject.as_ref() == Some(subject)))
}

/// Resolve a role against current durable convoy and terminal records. This
/// reads the federated view for routing; the delivery host additionally checks
/// that the returned terminal is locally authored.
pub async fn resolve_message_receiver(
    backend: &ResourceBackend,
    namespace: &str,
    address: &str,
) -> Result<Option<crate::ReadResourceObject<crate::TerminalSession>>, ResourceError> {
    use crate::{Convoy, TerminalSession, TerminalSessionSource, CONVOY_LABEL, ROLE_LABEL, VESSEL_LABEL};
    validate_message_address(address)?;
    let parts: Vec<_> = address.split('/').collect();
    let convoys = backend.including_replicas::<Convoy>(namespace).list().await?.items;
    let (project, convoy, vessel, role) = match parts.as_slice() {
        [project, convoy, vessel, role] => (*project, Some(*convoy), Some(*vessel), *role),
        [project, role] => (*project, None, None, *role),
        // Principal and system addresses never guess an agent terminal.
        _ => return Ok(None),
    };
    if convoy.is_none() {
        let adopted = backend
            .including_replicas::<TerminalSession>(namespace)
            .list()
            .await?
            .items
            .into_iter()
            .filter(|holder| {
                holder.object.metadata.annotations.get(ROLE_ADDRESS_ANNOTATION).is_some_and(|claimed| claimed == address)
                    && holder.object.status.as_ref().is_some_and(|status| status.phase == crate::TerminalSessionPhase::Running)
                    && matches!(holder.object.spec.source, TerminalSessionSource::Agent { .. })
            })
            .collect::<Vec<_>>();
        if !adopted.is_empty() {
            let declaration = backend.definitions::<crate::Project>(namespace).get(project).await?;
            let cascade = crate::ResolvedCascade::load(backend, namespace, project, &declaration.spec).await?;
            if !cascade.roles.get(role).is_some_and(|definition| definition.adoptable == Some(true)) {
                return Err(ResourceError::invalid("terminal claims a role that is not adoptable"));
            }
            if adopted.len() != 1 {
                return Err(ResourceError::invalid("adoptable role has multiple current terminal claims"));
            }
            return Ok(adopted.into_iter().next());
        }
    }
    let declared_convoy = if convoy.is_none() {
        let declarations = backend.including_replicas::<crate::ConvoyEnsure>(namespace).list().await?.items;
        let mut holders = declarations
            .iter()
            .filter(|declaration| declaration.object.spec.project_ref == project && declaration.object.spec.role == role);
        let Some(declaration) = holders.next() else { return Ok(None) };
        if holders.next().is_some() {
            return Err(ResourceError::invalid("project role has multiple holder declarations"));
        }
        let Some(name) = declaration.object.status.as_ref().and_then(|status| status.convoy_ref.clone()) else { return Ok(None) };
        Some(name)
    } else {
        None
    };
    let mut candidates: Vec<_> = convoys
        .into_iter()
        .filter(|source| {
            let object = &source.object;
            object.spec.project_ref.as_deref().unwrap_or(namespace) == project
                && object.status.as_ref().is_none_or(|status| !status.phase.is_terminal())
                && convoy.map_or_else(|| declared_convoy.as_ref() == Some(&object.metadata.name), |name| object.metadata.name == name)
        })
        .collect();
    candidates.sort_by_key(|source| std::cmp::Reverse(source.object.spec.generation));
    let Some(convoy) = candidates.first() else {
        return Ok(None);
    };
    if candidates.get(1).is_some_and(|other| other.object.spec.generation == convoy.object.spec.generation) {
        return Err(ResourceError::invalid("role address has multiple current convoy holders"));
    }
    let terminals = backend.including_replicas::<TerminalSession>(namespace).list().await?.items;
    let mut holders = terminals
        .into_iter()
        .filter(|source| {
            let object = &source.object;
            object.metadata.labels.get(CONVOY_LABEL) == Some(&convoy.object.metadata.name)
                // Four-part addresses constrain both vessel and role. A project role
                // names the declared standing convoy's unique agent holder.
                && (vessel.is_none() || object.metadata.labels.get(ROLE_LABEL).is_some_and(|value| value == role))
                && vessel.is_none_or(|vessel| object.metadata.labels.get(VESSEL_LABEL).is_some_and(|value| value == vessel))
                && matches!(object.spec.source, TerminalSessionSource::Agent { .. })
        })
        .collect::<Vec<_>>();
    // Local authority shadows a self-origin replica; the read resolver already
    // deduplicates that pair. Multiple independent holders are not a guess.
    match holders.len() {
        0 => Ok(None),
        1 => Ok(holders.pop()),
        _ => Err(ResourceError::invalid(format!("role address `{address}` has multiple current holders"))),
    }
}
