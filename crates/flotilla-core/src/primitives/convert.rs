//! The old store's Message records in the engine's terms (#3108), for the import alone. A record
//! becomes the producer's [`Intent`] it would be sent as today, then the engine's spec through the
//! mapping admission uses, so an imported message and a sent one are stored alike. Addresses go
//! through the ADR 0066 normaliser with the vessel segment dropped; anything with no ADR 0066 form
//! is refused, never guessed.
//!
//! A person (`principal:<name>`) has no address of their own: they send and receive as the role
//! they hold in the context the message concerns (#3938, owner ruling on #3457). That is the
//! project of the convoy or crew at the other end, or of a convoy the message refers to, and the
//! fleet otherwise.

use flotilla_address::{Address, Condition, Kind, Revision};
use flotilla_engine_messages::{admission::engine_spec, roles::FleetRoles, OldStoreRecords};
use flotilla_messaging::{Intent, IntentExpectation, IntentReference};
use flotilla_protocol::Leaf;
use flotilla_resources::{
    address::{leaf_condition, stored_change_request_address, stored_issue_address, stored_repository_address},
    convoy::engine::ImportedLaunches,
    ForgeSpec, MessageExpectation, MessageReference, MessageSpec as OldSpec,
};
use flotilla_state_machines::machines::message::{MessageRef, MessageReference as Reference, MessageSpec};

/// Resolves the old references that name stored records, which have an ADR 0066 address only
/// through those records, and the declarations a person's context is read from.
pub trait ReferenceAddresses: Send + Sync {
    fn address(&self, reference: &MessageReference) -> Result<Address, String>;

    /// Why `reference` cannot be addressed when the record it names is not
    /// held or its kind has no address; `None` when only its value is at fault.
    fn missing(&self, _reference: &MessageReference) -> Option<&'static str> {
        None
    }

    /// The project a convoy belongs to, if it declares one.
    fn convoy_project(&self, _convoy: &str) -> Option<String> {
        None
    }

    /// Whether `convoy` has ended: Landed, Abandoned or Failed, or no longer
    /// held. `false` while it is live or when this port cannot tell.
    fn convoy_ended(&self, _convoy: &str) -> bool {
        false
    }

    /// The roles `project`'s own charter declares `principal` holds
    /// (`role_definitions.<role>.principal`), by name.
    fn principal_roles(&self, _project: &str, _principal: &str) -> Vec<String> {
        Vec::new()
    }

    /// `address` naming the launch its convoy imports as (#3108). With no launches to read, an
    /// address naming a convoy is refused: an imported record names a convoy only by its launch.
    fn launched(&self, address: Address) -> Result<Address, String> {
        match address.get(Kind::Convoy) {
            Some(convoy) => Err(format!("`{address}` names convoy `{convoy}`, and this mapping knows no launches")),
            None => Ok(address),
        }
    }
}

/// `records`, with each convoy an address names read as the launch it imports as in `namespace`.
pub struct AtLaunches<'a> {
    pub records: &'a dyn ReferenceAddresses,
    pub launches: &'a ImportedLaunches,
    pub namespace: &'a str,
}

impl ReferenceAddresses for AtLaunches<'_> {
    fn address(&self, reference: &MessageReference) -> Result<Address, String> {
        self.records.address(reference)
    }
    fn missing(&self, reference: &MessageReference) -> Option<&'static str> {
        self.records.missing(reference)
    }
    fn convoy_project(&self, convoy: &str) -> Option<String> {
        self.records.convoy_project(convoy)
    }
    fn convoy_ended(&self, convoy: &str) -> bool {
        self.records.convoy_ended(convoy)
    }
    fn principal_roles(&self, project: &str, principal: &str) -> Vec<String> {
        self.records.principal_roles(project, principal)
    }
    fn launched(&self, address: Address) -> Result<Address, String> {
        self.launches.address(self.namespace, &address)
    }
}

/// References whose records are not available: refused with their kind.
pub struct NoRecords;
impl ReferenceAddresses for NoRecords {
    fn address(&self, reference: &MessageReference) -> Result<Address, String> {
        Err(format!("reference {reference:?} names a record this mapping cannot read"))
    }
}

impl ReferenceAddresses for OldStoreRecords {
    fn address(&self, reference: &MessageReference) -> Result<Address, String> {
        self.reference_address(reference)
    }
    fn missing(&self, reference: &MessageReference) -> Option<&'static str> {
        self.reference_missing(reference)
    }
    fn convoy_project(&self, convoy: &str) -> Option<String> {
        OldStoreRecords::convoy_project(self, convoy)
    }
    fn convoy_ended(&self, convoy: &str) -> bool {
        OldStoreRecords::convoy_ended(self, convoy)
    }
    fn principal_roles(&self, project: &str, principal: &str) -> Vec<String> {
        OldStoreRecords::principal_roles(self, project, principal)
    }
}

/// The old-to-new mapping for one designation and forge set.
pub struct Conversion<'a> {
    pub roles: &'a FleetRoles,
    pub forges: &'a [ForgeSpec],
    pub records: &'a dyn ReferenceAddresses,
}

impl Conversion<'_> {
    /// A receiver's inbox, as [`FleetRoles::inbox`] reads it.
    pub fn inbox(&self, receiver: &str) -> Result<Address, String> {
        self.roles.inbox(receiver)
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

    /// A person's role in `project`, or under the fleet with no project, as
    /// [`FleetRoles::principal_role`] reads the roles the project's charter declares they hold.
    pub fn person(&self, principal: &str, project: Option<&str>) -> Result<Address, String> {
        let held = project.map(|project| self.records.principal_roles(project, principal)).unwrap_or_default();
        self.roles.principal_role(principal, project, held)
    }

    fn party(&self, party: &str, old: &OldSpec) -> Result<Address, String> {
        if party.starts_with("principal:") {
            return self.person(party, self.context(old).as_deref());
        }
        self.records.launched(self.inbox(party)?)
    }

    /// Why an old message's receiver or sender cannot be named: the convoy role it names is of a
    /// convoy that imports as no launch. `None` when both name one, or no convoy.
    pub fn unlaunched(&self, old: &OldSpec) -> Option<String> {
        [&old.receiver, &old.sender]
            .into_iter()
            .filter_map(|party| self.inbox(party).ok().filter(|inbox| inbox.get(Kind::Convoy).is_some()))
            .find_map(|inbox| self.records.launched(inbox).err())
    }

    /// A Leaf as an ADR 0066 condition. A crew's progress addresses its role
    /// (ADR 0066 §8): `work/C/V .crew.R.<path>` is `project:P/convoy:C/role:R .<path>`,
    /// under `project` when the receiver's project is known, with the convoy as its launch.
    pub fn condition(&self, leaf: &Leaf, project: Option<&str>) -> Result<Condition, String> {
        let mut condition = leaf_condition(leaf, project, self.forges)?;
        condition.address = self.records.launched(condition.address)?;
        Ok(condition)
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
            | MessageReference::ControlRecord { revision, .. } => (self.records.launched(self.records.address(reference)?)?, revision),
        };
        Ok(Reference { address, revision: revision.clone(), subject })
    }

    /// An old record as the producer's intent it would be sent as.
    pub fn intent(&self, old: &OldSpec) -> Result<Intent, String> {
        let inbox = self.receiver(old)?;
        let project = inbox.get(Kind::Project).map(str::to_owned);
        let mut references = Vec::with_capacity(old.references.len());
        let mut subject_found = old.subject.is_none();
        for reference in &old.references {
            let subject = !subject_found && old.subject.as_ref() == Some(reference);
            subject_found |= subject;
            let Reference { address, revision, subject } = self.reference(reference, subject)?;
            let revision = Revision::new(revision).map_err(|error| format!("reference {address}: {error}"))?;
            references.push(IntentReference { address, revision, subject });
        }
        if !subject_found {
            return Err("the subject is not one of the references".into());
        }
        let expectation = match &old.expectation {
            MessageExpectation::None => IntentExpectation::Nothing,
            MessageExpectation::Reply => IntentExpectation::Reply,
            MessageExpectation::Outcome { condition } => {
                IntentExpectation::Outcome { condition: self.condition(condition, project.as_deref())? }
            }
        };
        let delivery_condition = old.delivery_condition.as_ref().map(|leaf| self.condition(leaf, project.as_deref())).transpose()?;
        Ok(Intent {
            sender: self.sender_of(old)?,
            inbox,
            relation: old.relation,
            body: body_text(&old.body, old.body_digest.as_deref()),
            references,
            in_reply_to: old.in_reply_to.clone(),
            supersedes: old.supersedes.clone(),
            expectation,
            deadline: old.deadline,
            delivery_condition,
            principal: old.sender.strip_prefix("principal:").map(str::to_owned),
        })
    }

    /// The new spec of an old one. `message` maps an old message name (a
    /// reply's original, a supersession's predecessor) to its reference.
    pub fn spec(&self, old: &OldSpec, message: &dyn Fn(&str) -> Result<MessageRef, String>) -> Result<MessageSpec, String> {
        let intent = self.intent(old)?;
        let in_reply_to = intent.in_reply_to.as_deref().map(message).transpose()?;
        let supersedes = intent.supersedes.as_deref().map(message).transpose()?;
        engine_spec(&intent, in_reply_to, supersedes)
    }
}

/// The body as prose. A terminal body the old audit sweep compacted (empty,
/// its digest kept) says exactly what was lost.
fn body_text(text: &str, digest: Option<&str>) -> String {
    match digest {
        Some(digest) if text.is_empty() => format!("[compacted body sha256:{digest}]"),
        _ => text.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use flotilla_resources::MessageRelation;

    use super::*;

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
        let launches = demo_launch();
        let launched = AtLaunches { records: &charter, launches: &launches, namespace: "flotilla" };
        let map = Conversion { roles: &roles, forges: &[], records: &launched };
        let to_crew = message("principal:alice", "flotilla/demo/work/coder");
        assert_eq!(map.sender_of(&to_crew).expect("operator").as_str(), "project:flotilla/role:operator");
        assert_eq!(map.receiver(&to_crew).expect("crew").as_str(), "project:flotilla/convoy:demo%232/role:coder");
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

    /// Convoy `demo` imports as its second launch.
    fn demo_launch() -> ImportedLaunches {
        let launch =
            flotilla_resources::convoy::engine::ImportedLaunch { home: "home".into(), project: "flotilla".into(), of: "demo".into(), n: 2 };
        ImportedLaunches::default().with("flotilla", "demo", launch)
    }

    // Behaviour: a crew-progress leaf addresses the role (ADR 0066 §8) of the launch its convoy
    // imports as (#3108), and without launches to read it is refused; a change request leaf keeps
    // its address; the print is canonical.
    #[test]
    fn leaves_become_canonical_conditions() {
        let roles = FleetRoles::declared("fleet").expect("roles");
        let launches = demo_launch();
        let launched = AtLaunches { records: &NoRecords, launches: &launches, namespace: "flotilla" };
        let map = Conversion { roles: &roles, forges: &[], records: &launched };
        let work: Leaf = "work/demo/work .crew.coder.phase == Done".parse().expect("leaf");
        assert_eq!(
            map.condition(&work, Some("flotilla")).expect("work").to_string(),
            "project:flotilla/convoy:demo%232/role:coder .phase == Done"
        );
        let unlaunched = Conversion { roles: &roles, forges: &[], records: &NoRecords };
        assert!(unlaunched.condition(&work, Some("flotilla")).expect_err("no launches").contains("knows no launches"));
        let cr: Leaf = "cr/github.com/flotilla-org/flotilla/42 .checks == passing".parse().expect("leaf");
        assert_eq!(map.condition(&cr, None).expect("cr").to_string(), "forge:github/repo:flotilla-org%2Fflotilla/cr:42 .checks == passing");
        let other_work: Leaf = "work/demo/work .status == Done".parse().expect("leaf");
        assert!(map.condition(&other_work, None).is_err());
    }

    // Behaviour: an emptied terminal body imports as text naming its digest.
    #[test]
    fn a_compacted_body_names_its_digest() {
        assert_eq!(body_text("", Some("abc")), "[compacted body sha256:abc]");
        assert_eq!(body_text("kept", Some("abc")), "kept");
    }
}
