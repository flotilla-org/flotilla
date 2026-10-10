use flotilla_resources::*;

use std::collections::BTreeMap;

// Admission and supersession share one receiver-side seam. Producers never
// manipulate delivery state or choose a session holder.
use std::sync::Arc;

use chrono::{DateTime, Utc};
use tokio::sync::Mutex;

use crate::{
    apply_status_patch, InputMeta, Message, MessagePhase, MessageQuery, MessageSpec, MessageStatusPatch, Resource, ResourceBackend,
    ResourceError, ResourceObject, TypedResolver,
};

/// Adoption controllers claim a live terminal for an adoptable Project role.
/// Removing the claim or stopping the terminal retires that local presence.
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
    pub(crate) charter_notifications: Arc<Mutex<()>>,
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
            charter_notifications: Arc::new(Mutex::new(())),
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

/// Receiver evidence frozen for one batch, never cached between passes.
/// Resolution after loading is pure, including adopted-role validation.
#[derive(Debug)]
pub struct MessageReceiverSnapshot {
    namespace: String,
    convoys: Vec<crate::ReadResourceObject<Convoy>>,
    terminals: Vec<crate::ReadResourceObject<TerminalSession>>,
    ensures: Vec<crate::ReadResourceObject<ConvoyEnsure>>,
    projects: BTreeMap<String, ProjectSpec>,
    hierarchy: ProjectHierarchy,
    defaults: Vec<(String, CrewDefaultsSpec)>,
}

impl MessageReceiverSnapshot {
    pub async fn load(backend: &ResourceBackend, namespace: &str) -> Result<Self, ResourceError> {
        let convoys = backend.including_replicas::<Convoy>(namespace).list().await?.items;
        let terminals = backend.including_replicas::<TerminalSession>(namespace).list().await?.items;
        Self::load_from_observations(backend, namespace, convoys, terminals).await
    }

    /// Reuse receiver observations already taken by a larger projection.
    pub async fn load_from_observations(
        backend: &ResourceBackend,
        namespace: &str,
        convoys: Vec<crate::ReadResourceObject<Convoy>>,
        terminals: Vec<crate::ReadResourceObject<TerminalSession>>,
    ) -> Result<Self, ResourceError> {
        let ensures = backend.including_replicas::<ConvoyEnsure>(namespace).list().await?.items;
        let mut projects = BTreeMap::new();
        let mut fleet = None;
        let mut defaults = Vec::new();
        if terminals.iter().any(|holder| holder.object.metadata.annotations.contains_key(ROLE_ADDRESS_ANNOTATION)) {
            projects = backend
                .definitions::<Project>(namespace)
                .list()
                .await?
                .into_iter()
                .map(|object| (object.metadata.name, object.spec))
                .collect();
            fleet = match backend.definitions::<FleetDesignation>(namespace).get(FLEET_DESIGNATION_NAME).await {
                Ok(object) => Some(object.spec.project),
                Err(ResourceError::NotFound { .. }) => None,
                Err(error) => return Err(error),
            };
            defaults = backend
                .definitions::<CrewDefaults>(namespace)
                .list()
                .await?
                .into_iter()
                .map(|object| (object.metadata.name, object.spec))
                .collect();
        }
        let hierarchy =
            ProjectHierarchy::from_declared(projects.iter().map(|(name, spec)| (name.clone(), spec.parent.clone())).collect(), fleet);
        Ok(Self { namespace: namespace.into(), convoys, terminals, ensures, projects, hierarchy, defaults })
    }

    /// Cheap candidate filter before resolving message addresses. Two-part roles
    /// remain candidates because their standing or adopted holder can belong here.
    pub fn may_address_convoy(&self, address: &str, project: &str, convoy: &str) -> bool {
        let parts = address.split('/').collect::<Vec<_>>();
        match parts.as_slice() {
            [p, c, _, _] => *p == project && *c == convoy,
            [p, role] if *p == project => {
                self.ensures.iter().any(|source| {
                    let declaration = &source.object;
                    declaration.spec.project_ref == project
                        && declaration.spec.role == *role
                        && declaration.status.as_ref().and_then(|status| status.convoy_ref.as_deref()) == Some(convoy)
                }) || self.terminals.iter().any(|holder| {
                    holder.object.metadata.annotations.get(ROLE_ADDRESS_ANNOTATION).is_some_and(|claim| claim == address)
                        && holder.object.metadata.labels.get(CONVOY_LABEL).is_some_and(|name| name == convoy)
                })
            }
            _ => false,
        }
    }

    pub(crate) fn convoys(&self) -> &[crate::ReadResourceObject<Convoy>] {
        &self.convoys
    }
    /// Terminal evidence from this batch, including replicas and local precedence.
    pub fn terminals(&self) -> &[crate::ReadResourceObject<TerminalSession>] {
        &self.terminals
    }

    pub fn resolve(&self, address: &str) -> Result<Option<crate::ReadResourceObject<TerminalSession>>, ResourceError> {
        validate_message_address(address)?;
        self.resolve_validated(address)
    }

    fn resolve_validated(&self, address: &str) -> Result<Option<crate::ReadResourceObject<TerminalSession>>, ResourceError> {
        use crate::{TerminalSessionSource, CONVOY_LABEL, ROLE_LABEL, VESSEL_LABEL};
        let parts: Vec<_> = address.split('/').collect();
        let convoys = &self.convoys;
        let (project, convoy, vessel, role) = match parts.as_slice() {
            [project, convoy, vessel, role] => (*project, Some(*convoy), Some(*vessel), *role),
            [project, role] => (*project, None, None, *role),
            // Principal and system addresses never guess an agent terminal.
            _ => return Ok(None),
        };
        if convoy.is_none() {
            let adopted = self
                .terminals
                .iter()
                .filter(|holder| {
                    holder.object.metadata.annotations.get(ROLE_ADDRESS_ANNOTATION).is_some_and(|claimed| claimed == address)
                        && holder.object.status.as_ref().is_some_and(|status| status.phase == crate::TerminalSessionPhase::Running)
                        && matches!(holder.object.spec.source, TerminalSessionSource::Agent { .. })
                })
                .collect::<Vec<_>>();
            if !adopted.is_empty() {
                let declaration = self.projects.get(project).ok_or_else(|| ResourceError::not_found(project))?;
                let cascade = ResolvedCascade::from_specs(&self.hierarchy, &self.projects, &self.defaults, project, declaration)?;
                if !cascade.roles.get(role).is_some_and(|definition| definition.adoptable == Some(true)) {
                    return Err(ResourceError::invalid("terminal claims a role that is not adoptable"));
                }
                if adopted.len() != 1 {
                    return Err(ResourceError::invalid("adoptable role has multiple current terminal claims"));
                }
                return Ok(adopted.into_iter().next().cloned());
            }
        }
        let declared_convoy = if convoy.is_none() {
            let declarations = &self.ensures;
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
            .iter()
            .filter(|source| {
                let object = &source.object;
                object.spec.project_ref.as_deref().unwrap_or(&self.namespace) == project
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
        let terminals = &self.terminals;
        let mut holders = terminals
            .iter()
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
            1 => Ok(holders.pop().cloned()),
            _ => Err(ResourceError::invalid(format!("role address `{address}` has multiple current holders"))),
        }
    }
}

/// Single-address compatibility seam; batch callers load one snapshot instead.
pub async fn resolve_message_receiver(
    backend: &ResourceBackend,
    namespace: &str,
    address: &str,
) -> Result<Option<crate::ReadResourceObject<TerminalSession>>, ResourceError> {
    validate_message_address(address)?;
    MessageReceiverSnapshot::load(backend, namespace).await?.resolve_validated(address)
}
