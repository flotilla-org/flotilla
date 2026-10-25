//! Old Message shapes onto the engine's (#3317). One mapping serves the
//! producers' sends behind the `[primitives]` switch and the import, so a
//! sent message and an imported one name the same things the same way.
//! Addresses go through the ADR 0066 normaliser with the vessel segment
//! dropped; anything with no ADR 0066 form is refused, never guessed.

use chrono::{DateTime, Utc};
use flotilla_address::{Address, Condition, Kind};
use flotilla_protocol::{Leaf, LeafAddress, LeafOperator};
use flotilla_resources::{
    address::{leaf_address, stored_change_request_address, stored_issue_address, stored_repository_address},
    ForgeSpec, MessageExpectation, MessageReference, MessageRelation, MessageSpec as OldSpec,
};
use flotilla_state_machines::machines::message::{
    Expectation, MessageBody, MessageRef, MessageReference as Reference, MessageSpec, Relation,
};

use super::roles::FleetRoles;

/// How long an inbox may stay unbound before a message to it may be dropped
/// as undeliverable: a crew's holder can take this long to provision or
/// restart. A fleet role's inbox has no consumer yet, so replies to it drop
/// once this runs out (owner ruling 4, 2026-10-19).
pub const UNBOUND_AFTER: chrono::Duration = chrono::Duration::hours(24);

/// Resolves the old references that name stored records, which have an
/// ADR 0066 address only through those records.
pub trait ReferenceAddresses: Send + Sync {
    fn address(&self, reference: &MessageReference) -> Result<Address, String>;
}

/// References whose records are not available: refused with their kind.
pub struct NoRecords;
impl ReferenceAddresses for NoRecords {
    fn address(&self, reference: &MessageReference) -> Result<Address, String> {
        Err(format!("reference {reference:?} names a record this mapping cannot read"))
    }
}

/// The old-to-new mapping for one designation and forge set.
pub struct Conversion<'a> {
    pub roles: &'a FleetRoles,
    pub forges: &'a [ForgeSpec],
    pub records: &'a dyn ReferenceAddresses,
}

impl Conversion<'_> {
    /// A receiver's inbox: `P/C/V/R` is the convoy role `project:P/convoy:C/role:R`
    /// (the vessel dropped), `P/R` the standing role `project:P/role:R`, and
    /// `system:<name>` the fleet role it names.
    pub fn inbox(&self, receiver: &str) -> Result<Address, String> {
        if receiver.starts_with("system:") {
            return self.roles.system_sender(receiver).ok_or_else(|| format!("`{receiver}` names no declared fleet role"));
        }
        if receiver.starts_with("principal:") {
            return Err(format!("`{receiver}` is a person; the person consumer comes with the principals model (#3457)"));
        }
        if receiver.starts_with("topic:") {
            return Err(format!("`{receiver}` is a topic; resolve its subscriber's role first"));
        }
        let refused = |error: flotilla_address::Error| format!("`{receiver}`: {error}");
        match receiver.split('/').collect::<Vec<_>>().as_slice() {
            [project, convoy, _vessel, role] => Address::project(project)
                .and_then(|project| project.child(Kind::Convoy, convoy))
                .and_then(|convoy| convoy.child(Kind::Role, role))
                .map_err(refused),
            [project, role] => Address::project(project).and_then(|project| project.child(Kind::Role, role)).map_err(refused),
            _ => Err(format!("`{receiver}` is not a role address")),
        }
    }

    /// A stall may concern a whole convoy rather than a role inbox.
    pub fn stall_subject(&self, subject: &str) -> Result<Address, String> {
        if subject.starts_with("project:") {
            let address: Address = subject.parse().map_err(|error| format!("invalid stall subject: {error}"))?;
            if address.kind() == Kind::Convoy {
                return Ok(address);
            }
            return Err(format!("`{subject}` is not a convoy address"));
        }
        self.inbox(subject)
    }

    /// A sender's address: a role as [`Conversion::inbox`] maps it, and a
    /// `system:` producer as the fleet role it sends as.
    pub fn sender(&self, sender: &str) -> Result<Address, String> {
        self.inbox(sender)
    }

    /// A Leaf as an ADR 0066 condition. A crew's progress addresses its role
    /// (ADR 0066 §8): `work/C/V .crew.R.<path>` is `project:P/convoy:C/role:R .<path>`,
    /// under `project` when the receiver's project is known.
    pub fn condition(&self, leaf: &Leaf, project: Option<&str>) -> Result<Condition, String> {
        let refused = |reason: String| format!("condition `{leaf}`: {reason}");
        let (address, path) = match &leaf.address {
            LeafAddress::Work { convoy, .. } => {
                let path = leaf.field_path.strip_prefix(".crew.").ok_or_else(|| refused("a work leaf names a crew role's field".into()))?;
                let (role, rest) = path.split_once('.').ok_or_else(|| refused("a crew field names a role and a field".into()))?;
                let convoy = match project {
                    Some(project) => Address::project(project).and_then(|project| project.child(Kind::Convoy, convoy)),
                    None => Address::convoy(convoy),
                };
                let role = convoy.and_then(|convoy| convoy.child(Kind::Role, role)).map_err(|error| refused(error.to_string()))?;
                (role, format!(".{rest}"))
            }
            other => {
                let address = leaf_address(other, self.forges).map_err(refused)?;
                (address, leaf.field_path.clone())
            }
        };
        let path = if path.starts_with('.') { path } else { format!(".{path}") };
        let operator = match leaf.operator {
            LeafOperator::Equal => "==",
            LeafOperator::NotEqual => "!=",
            LeafOperator::LessThan => "<",
            LeafOperator::LessThanOrEqual => "<=",
            LeafOperator::GreaterThan => ">",
            LeafOperator::GreaterThanOrEqual => ">=",
        };
        let literal = serde_json::to_string(&leaf.literal).expect("a string serializes");
        let lenient = format!("{address} {path} {operator} {literal}");
        let condition: Condition =
            Condition::resolve(&lenient, &flotilla_address::BuiltinForges).map_err(|error| refused(error.to_string()))?;
        // The canonical print is the stored identity; refuse anything the strict parser reads otherwise.
        condition.to_string().parse::<Condition>().map_err(|error| refused(error.to_string()))
    }

    /// An old reference as an ADR 0066 reference at its revision.
    pub fn reference(&self, reference: &MessageReference, subject: bool) -> Result<Reference, String> {
        let (address, revision) = match reference {
            MessageReference::ChangeRequest { service, scope, number, revision } => {
                (stored_change_request_address(service, scope, *number, self.forges)?, revision)
            }
            MessageReference::Issue { service, scope, number, revision } => {
                (stored_issue_address(service, scope, *number, self.forges)?, revision)
            }
            MessageReference::Comment { service, scope, id, revision } => (
                stored_repository_address(service, scope, self.forges)?
                    .child(Kind::Comment, id)
                    .map_err(|error| format!("comment {id}: {error}"))?,
                revision,
            ),
            MessageReference::Commit { revision, .. }
            | MessageReference::Ref { revision, .. }
            | MessageReference::Artifact { revision, .. }
            | MessageReference::ControlRecord { revision, .. } => (self.records.address(reference)?, revision),
        };
        Ok(Reference { address, revision: revision.clone(), subject })
    }

    /// The new spec of an old one. `message` maps an old message name (a
    /// reply's original, a supersession's predecessor) to its reference.
    pub fn spec(&self, old: &OldSpec, message: &dyn Fn(&str) -> Result<MessageRef, String>) -> Result<MessageSpec, String> {
        let inbox = self.inbox(&old.receiver)?;
        let project = inbox.get(Kind::Project).map(str::to_owned);
        let mut references = Vec::with_capacity(old.references.len());
        let mut subject_found = old.subject.is_none();
        for reference in &old.references {
            let subject = !subject_found && old.subject.as_ref() == Some(reference);
            subject_found |= subject;
            references.push(self.reference(reference, subject)?);
        }
        if !subject_found {
            return Err("the subject is not one of the references".into());
        }
        let body = body(&old.body, old.body_digest.as_deref())?;
        let expectation = match &old.expectation {
            MessageExpectation::None => Expectation::Nothing,
            MessageExpectation::Reply => Expectation::Reply,
            MessageExpectation::Outcome { condition } => Expectation::Outcome { condition: self.condition(condition, project.as_deref())? },
        };
        let delivery_condition = old.delivery_condition.as_ref().map(|leaf| self.condition(leaf, project.as_deref())).transpose()?;
        Ok(MessageSpec {
            references,
            in_reply_to: old.in_reply_to.as_deref().map(message).transpose()?,
            supersedes: old.supersedes.as_deref().map(message).transpose()?,
            expectation,
            deadline: old.deadline.as_ref().map(timestamp),
            unbound_after: UNBOUND_AFTER.num_microseconds().expect("bounded"),
            delivery_condition,
            ..MessageSpec::new(self.sender(&old.sender)?, inbox, relation(old.relation), body)
        })
    }
}

pub fn relation(relation: MessageRelation) -> Relation {
    match relation {
        MessageRelation::Supervisor => Relation::Supervisor,
        MessageRelation::Peer => Relation::Peer,
        MessageRelation::Dependency => Relation::Dependency,
        MessageRelation::Dependee => Relation::Dependee,
        MessageRelation::System => Relation::System,
    }
}

/// The body as prose. A terminal body the old audit sweep compacted (empty,
/// its digest kept) says exactly what was lost.
pub fn body(text: &str, digest: Option<&str>) -> Result<flotilla_schema::Frozen<MessageBody>, String> {
    let text = match digest {
        Some(digest) if text.is_empty() => format!("[compacted body sha256:{digest}]"),
        _ => text.to_owned(),
    };
    MessageBody::text(text).map_err(|error| format!("body: {error}"))
}

pub fn timestamp(at: &DateTime<Utc>) -> flotilla_schema::Timestamp {
    flotilla_schema::Timestamp::from_datetime(at)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conversion(roles: &FleetRoles) -> Conversion<'_> {
        Conversion { roles, forges: &[], records: &NoRecords }
    }

    // Behaviour: role addresses drop the vessel; system producers send as
    // their fleet role; people and topics have no inbox here.
    #[test]
    fn receivers_and_senders_map_to_host_free_role_addresses() {
        let roles = FleetRoles::declared("fleet").expect("roles");
        let map = conversion(&roles);
        assert_eq!(map.inbox("flotilla/demo/work/coder").expect("convoy role").as_str(), "project:flotilla/convoy:demo/role:coder");
        assert_eq!(map.inbox("flotilla/gui-approver").expect("standing role").as_str(), "project:flotilla/role:gui-approver");
        assert_eq!(map.sender("system:turn-rules").expect("fleet role").as_str(), "fleet:fleet/role:turn-rules");
        assert_eq!(map.sender("system:hold").expect("held turns").as_str(), "fleet:fleet/role:turn-rules");
        assert!(map.sender("principal:alice").is_err());
        assert!(map.inbox("topic:flotilla/reviews").is_err());
        assert!(map.inbox("system:unknown").is_err());
    }

    // Behaviour: a crew-progress leaf addresses the role (ADR 0066 §8); a
    // change request leaf keeps its address; the print is canonical.
    #[test]
    fn leaves_become_canonical_conditions() {
        let roles = FleetRoles::declared("fleet").expect("roles");
        let map = conversion(&roles);
        let work: Leaf = "work/demo/work .crew.coder.phase == Done".parse().expect("leaf");
        assert_eq!(
            map.condition(&work, Some("flotilla")).expect("work").to_string(),
            "project:flotilla/convoy:demo/role:coder .phase == Done"
        );
        let cr: Leaf = "cr/github.com/flotilla-org/flotilla/42 .checks == passing".parse().expect("leaf");
        assert_eq!(map.condition(&cr, None).expect("cr").to_string(), "forge:github/repo:flotilla-org%2Fflotilla/cr:42 .checks == passing");
        let other_work: Leaf = "work/demo/work .status == Done".parse().expect("leaf");
        assert!(map.condition(&other_work, None).is_err());
    }

    // Behaviour: an emptied terminal body imports as text naming its digest.
    #[test]
    fn a_compacted_body_names_its_digest() {
        let body = body("", Some("abc")).expect("body");
        assert_eq!(body.value(), &MessageBody::Text("[compacted body sha256:abc]".into()));
    }
}
