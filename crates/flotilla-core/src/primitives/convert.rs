//! Old Message shapes onto the engine's (#3317). One mapping serves the
//! producers' sends behind the `[primitives]` switch and the import, so a
//! sent message and an imported one name the same things the same way.
//! Addresses go through the ADR 0066 normaliser with the vessel segment
//! dropped; anything with no ADR 0066 form is refused, never guessed.
//!
//! A person (`principal:<name>`) has no address of their own: they send and
//! receive as the role they hold in the context the message concerns (#3938,
//! owner ruling on #3457). That is the project of the convoy or crew at the
//! other end, or of a convoy the message refers to, and the fleet otherwise.

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

use super::roles::{FleetRoles, OPERATOR, PERSON_ROLES};

/// How long an inbox may stay unbound before a message to it may be dropped
/// as undeliverable: a crew's holder can take this long to provision or
/// restart. A fleet role's inbox has no consumer yet, so replies to it drop
/// once this runs out (owner ruling 4, 2026-10-19).
pub const UNBOUND_AFTER: chrono::Duration = chrono::Duration::hours(24);

/// Resolves the old references that name stored records, which have an
/// ADR 0066 address only through those records, and the declarations a
/// person's context is read from.
pub trait ReferenceAddresses: Send + Sync {
    fn address(&self, reference: &MessageReference) -> Result<Address, String>;

    /// The project a convoy belongs to, if it declares one.
    fn convoy_project(&self, _convoy: &str) -> Option<String> {
        None
    }

    /// The roles `project`'s own charter declares `principal` holds
    /// (`role_definitions.<role>.principal`), by name.
    fn principal_roles(&self, _project: &str, _principal: &str) -> Vec<String> {
        Vec::new()
    }
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
            return Err(format!("`{receiver}` is a person, whose role depends on the message's context"));
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

    /// A person's role in `project`, or under the fleet with no project. A
    /// project's charter may declare the roles a principal holds there; one
    /// such role is theirs, and several are refused rather than guessed
    /// between. With none declared, `principal:<role>` names that person role
    /// and anyone else holds the operator role, as one person consumer holds
    /// every person role at the roll.
    pub fn person(&self, principal: &str, project: Option<&str>) -> Result<Address, String> {
        let name =
            principal.strip_prefix("principal:").filter(|name| !name.is_empty()).ok_or_else(|| format!("`{principal}` is no person"))?;
        let default = PERSON_ROLES.iter().find(|role| role.name == name).unwrap_or(&OPERATOR).name;
        let Some(project) = project else {
            return Ok(self.roles.person(default).expect("a declared person role"));
        };
        let mut held = self.records.principal_roles(project, principal);
        held.sort();
        let role = match held.as_slice() {
            [] => default,
            [role] => role.as_str(),
            several => {
                return Err(format!(
                    "`{principal}` holds roles {} in project `{project}`; the message names no one of them",
                    several.iter().map(|role| format!("`{role}`")).collect::<Vec<_>>().join(", ")
                ))
            }
        };
        Address::project(project).and_then(|project| project.child(Kind::Role, role)).map_err(|error| format!("`{principal}`: {error}"))
    }

    /// The project an old message concerns, for a person at either end: the
    /// project of the role at the other end, else of a convoy it refers to.
    pub fn context(&self, old: &OldSpec) -> Option<String> {
        let party =
            [&old.receiver, &old.sender].into_iter().find_map(|party| self.inbox(party).ok()?.get(Kind::Project).map(str::to_owned));
        party.or_else(|| {
            old.references.iter().find_map(|reference| match reference {
                MessageReference::ControlRecord { resource, .. } if resource.kind == "Convoy" => {
                    self.records.convoy_project(&resource.name)
                }
                _ => None,
            })
        })
    }

    /// An old message's inbox, a person's role read in the message's context.
    pub fn receiver(&self, old: &OldSpec) -> Result<Address, String> {
        self.party(&old.receiver, old)
    }

    /// An old message's sender, a person's role read in the message's context.
    pub fn sender_of(&self, old: &OldSpec) -> Result<Address, String> {
        self.party(&old.sender, old)
    }

    fn party(&self, party: &str, old: &OldSpec) -> Result<Address, String> {
        if party.starts_with("principal:") {
            return self.person(party, self.context(old).as_deref());
        }
        self.inbox(party)
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
        let inbox = self.receiver(old)?;
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
            ..MessageSpec::new(self.sender_of(old)?, inbox, relation(old.relation), body)
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
    // their fleet role; people need a message's context, and topics have no inbox here.
    #[test]
    fn receivers_and_senders_map_to_host_free_role_addresses() {
        let roles = FleetRoles::declared("fleet").expect("roles");
        let map = conversion(&roles);
        assert_eq!(map.inbox("flotilla/demo/work/coder").expect("convoy role").as_str(), "project:flotilla/convoy:demo/role:coder");
        assert_eq!(map.inbox("flotilla/gui-approver").expect("standing role").as_str(), "project:flotilla/role:gui-approver");
        assert_eq!(map.sender("system:turn-rules").expect("fleet role").as_str(), "fleet:fleet/role:turn-rules");
        assert_eq!(map.sender("system:hold").expect("held turns").as_str(), "fleet:fleet/role:turn-rules");
        assert!(map.sender("principal:alice").is_err(), "a person has no address outside a message's context");
        assert!(map.inbox("topic:flotilla/reviews").is_err());
        assert!(map.inbox("system:unknown").is_err());
    }

    /// Charter declarations of the roles principals hold, by project.
    struct Charter(Vec<(&'static str, &'static str, &'static str)>);
    impl ReferenceAddresses for Charter {
        fn address(&self, reference: &MessageReference) -> Result<Address, String> {
            NoRecords.address(reference)
        }
        fn convoy_project(&self, convoy: &str) -> Option<String> {
            (convoy == "standing").then(|| "widgets".to_owned())
        }
        fn principal_roles(&self, project: &str, principal: &str) -> Vec<String> {
            self.0.iter().filter(|(p, who, _)| *p == project && *who == principal).map(|(_, _, role)| (*role).to_owned()).collect()
        }
    }

    fn message(sender: &str, receiver: &str) -> OldSpec {
        OldSpec::builder().sender(sender.into()).receiver(receiver.into()).relation(MessageRelation::Supervisor).body("hi".into()).build()
    }

    // Behaviour (#3938): a person sends and receives as the role they hold in
    // the project the message concerns, the fleet's with no project; a role
    // the charter declares for them is theirs, and several are refused.
    #[test]
    fn a_person_is_the_role_they_hold_in_the_messages_context() {
        let roles = FleetRoles::declared("fleet").expect("roles");
        let charter = Charter(vec![
            ("flotilla", "principal:bob", "owner"),
            ("flotilla", "principal:eve", "owner"),
            ("flotilla", "principal:eve", "lead"),
        ]);
        let map = Conversion { roles: &roles, forges: &[], records: &charter };
        let to_crew = message("principal:alice", "flotilla/demo/work/coder");
        assert_eq!(map.sender_of(&to_crew).expect("operator").as_str(), "project:flotilla/role:operator");
        assert_eq!(map.receiver(&to_crew).expect("crew").as_str(), "project:flotilla/convoy:demo/role:coder");
        let escalated = message("flotilla/demo/work/coder", "principal:operator");
        assert_eq!(map.receiver(&escalated).expect("operator").as_str(), "project:flotilla/role:operator");
        assert_eq!(
            map.sender_of(&message("principal:bob", "flotilla/demo/work/coder")).expect("owner").as_str(),
            "project:flotilla/role:owner"
        );
        let ambiguous = map.sender_of(&message("principal:eve", "flotilla/demo/work/coder")).expect_err("ambiguous");
        assert!(ambiguous.contains("`lead`, `owner`"), "{ambiguous}");
        assert_eq!(map.receiver(&message("system:stall-judge", "principal:alice")).expect("fleet").as_str(), "fleet:fleet/role:operator");
        let about = OldSpec {
            references: vec![MessageReference::ControlRecord {
                resource: flotilla_protocol::ResourceRef::new("flotilla.work/v1", "Convoy", "flotilla", "standing"),
                revision: "1".into(),
            }],
            ..message("system:turn-rules", "principal:implicit")
        };
        assert_eq!(map.receiver(&about).expect("convoy's project").as_str(), "project:widgets/role:operator");
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
