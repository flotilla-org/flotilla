//! An attention: a fleet role telling a person about one record. It is an ordinary Message from
//! that role to the person's role in the record's context, about that record as its one subject,
//! expecting nothing; it is open while Pending, and is known by its sender and subject.

use flotilla_address::Address;
use flotilla_state_machines::machines::message::{MessageReference, MessageSpec, Relation};

use super::{
    convert::{body, UNBOUND_AFTER},
    roles::{FleetRole, FleetRoles, CAPABILITIES, STANDING, TEARDOWN, TURN_RULES},
};

/// A record an attention names, at the revision its sender read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Read {
    pub address: Address,
    pub revision: String,
}

/// What an attention is about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cause {
    /// A standing role's convoy is held.
    Hold { convoy: Read },
    /// A standing role's declaration differs from what its convoy was admitted with.
    Drift { role: Read, convoy: Read },
    /// Reclaiming a finished convoy's resources is refused.
    ReclaimRefused { convoy: Read },
    /// A vessel's credentials stop refreshing.
    CredentialRefresh { vessel: Read },
    /// Turns to a convoy role are held until a person rules.
    DeliveryHeld { role: Read },
}

impl Cause {
    /// The fleet role that sends it.
    pub fn sender(&self) -> FleetRole {
        match self {
            Cause::Hold { .. } | Cause::Drift { .. } => STANDING,
            Cause::ReclaimRefused { .. } => TEARDOWN,
            Cause::CredentialRefresh { .. } => CAPABILITIES,
            Cause::DeliveryHeld { .. } => TURN_RULES,
        }
    }

    pub fn subject(&self) -> &Read {
        match self {
            Cause::Hold { convoy } | Cause::ReclaimRefused { convoy } => convoy,
            Cause::Drift { role, .. } => role,
            Cause::CredentialRefresh { vessel } => vessel,
            Cause::DeliveryHeld { role } => role,
        }
    }

    fn context(&self) -> Option<&Read> {
        match self {
            Cause::Drift { convoy, .. } => Some(convoy),
            _ => None,
        }
    }
}

/// One attention: what it is about, and the reason a person reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attention {
    pub cause: Cause,
    pub reason: String,
}

impl Attention {
    /// The message it is sent as, to the person role `inbox`.
    pub fn spec(&self, roles: &FleetRoles, inbox: Address) -> Result<MessageSpec, String> {
        let sender = self.cause.sender();
        let sender = roles.address(sender.name).ok_or_else(|| format!("the fleet declares no `{}` role", sender.name))?;
        let reference = |read: &Read, subject| {
            if read.revision.is_empty() {
                return Err(format!("{} is read at no revision", read.address));
            }
            Ok(MessageReference { address: read.address.clone(), revision: read.revision.clone(), subject })
        };
        let mut references = vec![reference(self.cause.subject(), true)?];
        if let Some(context) = self.cause.context() {
            references.push(reference(context, false)?);
        }
        Ok(MessageSpec {
            references,
            unbound_after: UNBOUND_AFTER.num_microseconds().expect("bounded"),
            ..MessageSpec::new(sender, inbox, Relation::System, body(&self.reason, None)?)
        })
    }
}

#[cfg(test)]
mod tests {
    use flotilla_state_machines::machines::message::Expectation;

    use super::*;

    fn read(address: &str) -> Read {
        Read { address: address.parse().expect("address"), revision: "7".into() }
    }

    #[test]
    fn each_cause_is_sent_by_its_role_about_its_subject() {
        let roles = FleetRoles::declared("lab").expect("roles");
        let inbox: Address = "project:flotilla/role:operator".parse().expect("inbox");
        let convoy = read("project:flotilla/convoy:standing-gui");
        let cases = [
            (Cause::Hold { convoy: convoy.clone() }, "fleet:lab/role:standing", "project:flotilla/convoy:standing-gui"),
            (
                Cause::Drift { role: read("project:flotilla/role:gui"), convoy: convoy.clone() },
                "fleet:lab/role:standing",
                "project:flotilla/role:gui",
            ),
            (Cause::ReclaimRefused { convoy: convoy.clone() }, "fleet:lab/role:teardown", "project:flotilla/convoy:standing-gui"),
            (
                Cause::CredentialRefresh { vessel: read("project:flotilla/convoy:demo/vessel:work") },
                "fleet:lab/role:capabilities",
                "project:flotilla/convoy:demo/vessel:work",
            ),
            (
                Cause::DeliveryHeld { role: read("project:flotilla/convoy:demo/role:coder") },
                "fleet:lab/role:turn-rules",
                "project:flotilla/convoy:demo/role:coder",
            ),
        ];
        for (cause, sender, subject) in cases {
            let spec = Attention { cause: cause.clone(), reason: "why".into() }.spec(&roles, inbox.clone()).expect("spec");
            assert_eq!(spec.sender.as_str(), sender, "{cause:?}");
            assert_eq!(spec.inbox, inbox);
            assert_eq!(spec.relation, Relation::System);
            assert_eq!(spec.expectation, Expectation::Nothing);
            assert_eq!(spec.deadline, None);
            assert_eq!(spec.subject().map(|reference| reference.address.as_str()), Some(subject), "{cause:?}");
            let others: Vec<_> = spec.references.iter().filter(|reference| !reference.subject).map(|r| r.address.as_str()).collect();
            let expected: &[&str] = if matches!(cause, Cause::Drift { .. }) { &["project:flotilla/convoy:standing-gui"] } else { &[] };
            assert_eq!(others, expected, "{cause:?}");
        }
    }

    #[test]
    fn a_record_read_at_no_revision_is_refused() {
        let roles = FleetRoles::declared("lab").expect("roles");
        let cause = Cause::Hold { convoy: Read { revision: String::new(), ..read("project:flotilla/convoy:c") } };
        let refused = Attention { cause, reason: "why".into() }.spec(&roles, "fleet:lab/role:operator".parse().expect("inbox"));
        assert!(refused.is_err());
    }
}
