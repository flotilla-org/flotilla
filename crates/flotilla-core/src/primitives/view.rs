//! Engine messages seen in the old Message shape (#3317). While Message is
//! the only kind on the engine, the convoy and crew code that judges stalls,
//! confirms turns and deduplicates sends still reads `flotilla_resources`
//! Messages; this is the read half of that transition port. Each field is
//! the old spelling of what the engine stores, so a judgement reads the same
//! whichever store answered. Fields the engine does not keep (a submission
//! batch, retry state) are absent, as for a message never submitted.

use chrono::{DateTime, Utc};
use flotilla_address::{Address, Condition, Kind};
use flotilla_protocol::{Leaf, LeafAddress, LeafOperator, ResourceRef};
use flotilla_resources::{
    address::{leaf_for, stored_service_scope},
    Message as OldMessage, MessageExpectation, MessagePhase, MessageReference as OldReference, MessageRelation, MessageSpec as OldSpec,
    MessageStatus, ObjectMeta, ResolvedMessageReceiver, ResourceObject,
};
use flotilla_state_machines::{
    machines::{
        consumer::Receiver,
        message::{DropReason, Expectation, Message, MessageBody, MessageReference, MessageState, Relation},
    },
    Resource,
};

use super::roles::FleetRoles;

/// What a view needs beyond the message: names the engine drops.
pub trait ViewLookups {
    /// The vessel holding `role` in `convoy`, which the old receiver spelling names.
    fn vessel(&self, convoy: &str, role: &str) -> Option<String>;
    /// The crew id and session id of the session a receipt names.
    fn session(&self, receiver: &Receiver) -> Option<(String, String)>;
    /// The old reference to a record-backed address (a commit, ref, artifact or convoy).
    fn record(&self, address: &Address, revision: &str) -> Option<OldReference>;
    /// The principal `project`'s charter declares holds `role`, if one does.
    fn principal_of(&self, _project: &str, _role: &str) -> Option<String> {
        None
    }
}

/// An engine message's id under its inbox: the old record name.
pub fn record_name(message: &str) -> Option<String> {
    let address: Address = message.parse().ok()?;
    (address.kind() == Kind::Message).then(|| address.name().to_owned())
}

/// The old spelling of an inbox or sender address. A person's role is spelled
/// as the principal its charter declares, else as `principal:<role>`, which
/// the conversion reads back to the same role in the same context.
pub fn old_address(address: &Address, roles: &FleetRoles, lookups: &dyn ViewLookups) -> Result<String, String> {
    if let Some(role) = roles.role_of(address) {
        return Ok(format!("system:{role}"));
    }
    if address.kind() != Kind::Role {
        return Err(format!("`{address}` is not a role"));
    }
    if address.get(Kind::Convoy).is_none() {
        if let Some(principal) = address.get(Kind::Project).and_then(|project| lookups.principal_of(project, address.name())) {
            return Ok(principal);
        }
        if let Some(role) = roles.person_role_of(address) {
            return Ok(format!("principal:{role}"));
        }
    }
    let project = address.get(Kind::Project).ok_or_else(|| format!("`{address}` has no project"))?;
    match address.get(Kind::Convoy) {
        Some(convoy) => {
            let vessel = lookups
                .vessel(convoy, address.name())
                .ok_or_else(|| format!("no vessel holds `{}` in convoy `{convoy}`", address.name()))?;
            Ok(format!("{project}/{convoy}/{vessel}/{}", address.name()))
        }
        None => Ok(format!("{project}/{}", address.name())),
    }
}

/// A condition as the Leaf it was converted from.
pub fn old_leaf(condition: &Condition, lookups: &dyn ViewLookups) -> Result<Leaf, String> {
    let operator = match condition.operator {
        flotilla_address::Operator::Equal => LeafOperator::Equal,
        flotilla_address::Operator::NotEqual => LeafOperator::NotEqual,
        flotilla_address::Operator::LessThan => LeafOperator::LessThan,
        flotilla_address::Operator::LessThanOrEqual => LeafOperator::LessThanOrEqual,
        flotilla_address::Operator::GreaterThan => LeafOperator::GreaterThan,
        flotilla_address::Operator::GreaterThanOrEqual => LeafOperator::GreaterThanOrEqual,
    };
    let address = &condition.address;
    let path = condition.path.to_string();
    let (leaf, field_path) = match address.kind() {
        Kind::ChangeRequest | Kind::Issue => (leaf_for(address)?, path),
        Kind::Convoy => (LeafAddress::Convoy { name: address.name().to_owned() }, path),
        Kind::Role if address.get(Kind::Convoy).is_some() => {
            let convoy = address.get(Kind::Convoy).expect("checked");
            let vessel = lookups
                .vessel(convoy, address.name())
                .ok_or_else(|| format!("no vessel holds `{}` in convoy `{convoy}`", address.name()))?;
            let rest = path.strip_prefix('.').unwrap_or(&path);
            (LeafAddress::Work { convoy: convoy.to_owned(), work: vessel }, format!(".crew.{}.{rest}", address.name()))
        }
        Kind::Account => (
            LeafAddress::Usage { provider: address.get(Kind::Usage).unwrap_or_default().to_owned(), account: address.name().to_owned() },
            path,
        ),
        Kind::Artifact => {
            let convoy = address.get(Kind::Convoy).ok_or("an artifact condition names its convoy")?;
            let producer = address.get(Kind::Role).ok_or("an artifact condition names its role")?;
            (
                LeafAddress::Artifact {
                    convoy: convoy.to_owned(),
                    producer: producer.to_owned(),
                    kind: address.name().to_owned(),
                    subject: address.of().unwrap_or_default().to_owned(),
                },
                path,
            )
        }
        kind => return Err(format!("a {kind} condition has no leaf")),
    };
    Ok(Leaf { address: leaf, field_path, operator, literal: condition.literal.clone() })
}

/// A reference in its old spelling.
pub fn old_reference(reference: &MessageReference, lookups: &dyn ViewLookups) -> Result<OldReference, String> {
    let address = &reference.address;
    let revision = reference.revision.clone();
    let numbered = |address: &Address| -> Result<(String, String, u64), String> {
        let (service, scope) = stored_service_scope(address).ok_or_else(|| format!("`{address}` names no repository"))?;
        let number = address.name().parse().map_err(|_| format!("`{address}` has no number"))?;
        Ok((service, scope, number))
    };
    match address.kind() {
        Kind::ChangeRequest => {
            let (service, scope, number) = numbered(address)?;
            Ok(OldReference::ChangeRequest { service, scope, number, revision })
        }
        Kind::Issue => {
            let (service, scope, number) = numbered(address)?;
            Ok(OldReference::Issue { service, scope, number, revision })
        }
        Kind::Comment => {
            let (service, scope) = stored_service_scope(address).ok_or_else(|| format!("`{address}` names no repository"))?;
            Ok(OldReference::Comment { service, scope, id: address.name().to_owned(), revision })
        }
        _ => lookups.record(address, &revision).ok_or_else(|| format!("`{address}` names no record this view can spell")),
    }
}

/// An engine message as the old Message the convoy and crew code reads.
pub fn old_view(
    resource: &Resource<Message>,
    namespace: &str,
    roles: &FleetRoles,
    lookups: &dyn ViewLookups,
) -> Result<ResourceObject<OldMessage>, String> {
    let name = record_name(resource.name()).ok_or_else(|| format!("`{}` is not a message address", resource.name()))?;
    let spec = resource.spec();
    let receiver = old_address(&spec.inbox, roles, lookups)?;
    let sender = old_address(&spec.sender, roles, lookups)?;
    let mut references = Vec::with_capacity(spec.references.len());
    let mut subject = None;
    for reference in &spec.references {
        let old = old_reference(reference, lookups)?;
        if reference.subject {
            subject = Some(old.clone());
        }
        references.push(old);
    }
    let body = match spec.body.value() {
        MessageBody::Text(text) => text.clone(),
        MessageBody::Request(request) => format!("{request:?}"),
    };
    let expectation = match &spec.expectation {
        Expectation::Nothing => MessageExpectation::None,
        Expectation::Reply => MessageExpectation::Reply,
        Expectation::Outcome { condition } => MessageExpectation::Outcome { condition: old_leaf(condition, lookups)? },
    };
    let old_message = |reference: &flotilla_state_machines::machines::message::MessageRef| {
        record_name(&reference.name).ok_or_else(|| format!("`{}` is not a message address", reference.name))
    };
    let old_spec = OldSpec {
        sender,
        receiver: receiver.clone(),
        relation: match spec.relation {
            Relation::Supervisor => MessageRelation::Supervisor,
            Relation::Peer => MessageRelation::Peer,
            Relation::Dependency => MessageRelation::Dependency,
            Relation::Dependee => MessageRelation::Dependee,
            Relation::System => MessageRelation::System,
        },
        body,
        body_digest: None,
        references,
        subject,
        expectation,
        in_reply_to: spec.in_reply_to.as_ref().map(old_message).transpose()?,
        deadline: spec.deadline.and_then(|deadline| deadline.to_datetime()),
        supersedes: spec.supersedes.as_ref().map(old_message).transpose()?,
        interrupting: false,
        delivery_condition: spec.delivery_condition.as_ref().map(|condition| old_leaf(condition, lookups)).transpose()?,
    };
    let since = resource.since().to_datetime().unwrap_or(DateTime::<Utc>::UNIX_EPOCH);
    let state = resource.state();
    let phase = match state {
        MessageState::Pending => MessagePhase::Deliverable,
        MessageState::Awaiting { .. } => MessagePhase::Delivered,
        MessageState::Settled { .. } => match spec.expectation {
            Expectation::Nothing => MessagePhase::Satisfied,
            Expectation::Reply => MessagePhase::Answered,
            Expectation::Outcome { .. } => MessagePhase::OutcomeMet,
        },
        MessageState::Dropped { reason, .. } => match reason {
            DropReason::Expired | DropReason::Withdrawn { .. } => MessagePhase::Expired,
            DropReason::Superseded { .. } => MessagePhase::Superseded,
            DropReason::Undeliverable | DropReason::ConsumerUnreachable { .. } => MessagePhase::DeadLettered,
        },
    };
    // The delivered receiver is the inbox's holder; a receipt's time is the
    // state's `since` (Awaiting, or Settled straight from delivery).
    let resolved_receiver = state.receiver().map(|consumer| {
        let (crew_id, session) = lookups.session(consumer).unwrap_or_else(|| (consumer.id.clone(), consumer.id.clone()));
        ResolvedMessageReceiver {
            role_address: Some(receiver.clone()),
            crew_id,
            session,
            delivered_at: since,
            evidence: format!("Receive on AgentSession {}", consumer.id),
            // The engine path types through cleat outside the transport router (#3545).
            transport: None,
        }
    });
    let reason = match state {
        MessageState::Dropped { reason, .. } => Some(format!("{reason:?}")),
        MessageState::Pending => Some("waiting for its consumer".into()),
        _ => None,
    };
    let canonical_predecessor = match state {
        MessageState::Dropped { reason: DropReason::Superseded { by }, .. } => {
            Some(ResourceRef::new("flotilla.work/v1", "Message", namespace, by.name()))
        }
        _ => None,
    };
    Ok(ResourceObject {
        metadata: ObjectMeta {
            name,
            namespace: namespace.to_owned(),
            resource_version: resource.version().0.to_string(),
            labels: Default::default(),
            annotations: Default::default(),
            owner_references: Vec::new(),
            finalizers: Vec::new(),
            deletion_timestamp: None,
            creation_timestamp: since,
            merge: None,
        },
        spec: old_spec,
        status: Some(MessageStatus {
            phase,
            accepted_sequence: Some(spec.admitted_seq),
            since,
            reason,
            canonical_predecessor,
            resolved_receiver,
            retry: None,
            submission: None,
            failed_batch_members: Vec::new(),
            operator_delivery: None,
            delivery_history: Vec::new(),
            fence_recheck_at: None,
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::primitives::convert::{Conversion, NoRecords};

    struct Charter;
    impl ViewLookups for Charter {
        fn vessel(&self, _: &str, _: &str) -> Option<String> {
            None
        }
        fn session(&self, _: &Receiver) -> Option<(String, String)> {
            None
        }
        fn record(&self, _: &Address, _: &str) -> Option<OldReference> {
            None
        }
        fn principal_of(&self, project: &str, role: &str) -> Option<String> {
            (project == "flotilla" && role == "owner").then(|| "principal:bob".into())
        }
    }

    // Behaviour (#3938): a person role is spelled as the principal its charter
    // declares, else as `principal:<role>`, which converts back to the same
    // role in the same context.
    #[test]
    fn person_roles_spell_as_principals_that_convert_back() {
        let roles = FleetRoles::declared("fleet").expect("roles");
        let spell = |address: &str| old_address(&address.parse().expect("address"), &roles, &Charter).expect("spelled");
        assert_eq!(spell("project:flotilla/role:owner"), "principal:bob");
        assert_eq!(spell("project:flotilla/role:operator"), "principal:operator");
        assert_eq!(spell("fleet:fleet/role:operator"), "principal:operator");
        assert_eq!(spell("project:flotilla/role:gui-approver"), "flotilla/gui-approver");
        let conversion = Conversion { roles: &roles, forges: &[], records: &NoRecords };
        let reply = OldSpec::builder()
            .sender(spell("project:flotilla/role:operator"))
            .receiver("flotilla/gui-approver".into())
            .relation(MessageRelation::Supervisor)
            .body("approved".into())
            .build();
        assert_eq!(conversion.sender_of(&reply).expect("operator").as_str(), "project:flotilla/role:operator");
    }
}
