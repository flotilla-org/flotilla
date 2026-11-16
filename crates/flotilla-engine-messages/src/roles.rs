//! Fleet roles use the fleet identity, independently of its charter Project.
//! Handler bindings can change without changing a role address.

use std::collections::BTreeMap;

use flotilla_address::{Address, Kind};
use flotilla_resources::{convoy::engine::role_inbox, FleetDesignation, FleetDesignationSpec, ResourceError, FLEET_DESIGNATION_NAME};
use flotilla_store::ResourceBackend;

/// A declared fleet role: its name under the fleet, and what it does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FleetRole {
    pub name: &'static str,
    pub purpose: &'static str,
}

pub const TURN_RULES: FleetRole = FleetRole {
    name: "turn-rules",
    purpose: "Delivers crew turns and held turns, and withdraws a turn once its delivery condition lapses.",
};
pub const NUDGE: FleetRole = FleetRole { name: "nudge", purpose: "Nudges a quiet crew toward the obligation it has not yet met." };
pub const STALL_JUDGE: FleetRole =
    FleetRole { name: "stall-judge", purpose: "Judges a quiet crew stalled and escalates it to its supervisor." };
pub const CAPABILITIES: FleetRole = FleetRole {
    name: "capabilities",
    purpose: "Tells a crew when the credentials and tools it may use change, and tells a person when a credential stops refreshing.",
};
pub const FLEET_STORE: FleetRole =
    FleetRole { name: "fleet-store", purpose: "Tells a crew when the charter of the project it works under changes." };

pub const STANDING: FleetRole = FleetRole {
    name: "standing",
    purpose: "Keeps each standing role's convoy running, and tells a person when one is held or its configuration has drifted from what was admitted.",
};
pub const TEARDOWN: FleetRole =
    FleetRole { name: "teardown", purpose: "Reclaims a finished convoy's resources, and tells a person when reclaim is refused." };

/// The checkout inspector (#3108): each host's service that inspects the checkouts homed there
/// when a gate asks, through its host's inbox `host:<host>/role:checkout-inspector`.
pub const CHECKOUT_INSPECTOR: &str = "checkout-inspector";

/// Host `host`'s checkout inspector's inbox: `host:<host>/role:checkout-inspector`, a role under
/// its host (ADR 0066 amendment). It names the host, not the fleet: a gate at any home asks the
/// checkout's home.
pub fn checkout_inspector(host: &str) -> Result<Address, String> {
    Address::root(Kind::Host, host)
        .and_then(|place| place.child(Kind::Role, CHECKOUT_INSPECTOR))
        .map_err(|error| format!("checkout inspector of {host}: {error}"))
}

/// The forge observer (#3108): each host's service that looks up its repositories' change
/// requests by branch when branch validation asks, through `host:<host>/role:forge-observer`.
pub const FORGE_OBSERVER: &str = "forge-observer";

/// Host `host`'s forge observer's inbox: `host:<host>/role:forge-observer`, a role under its host.
pub fn forge_observer(host: &str) -> Result<Address, String> {
    Address::root(Kind::Host, host)
        .and_then(|place| place.child(Kind::Role, FORGE_OBSERVER))
        .map_err(|error| format!("forge observer of {host}: {error}"))
}

/// The defaults the system sends as, in declaration order.
pub const FLEET_ROLES: [FleetRole; 7] = [TURN_RULES, NUDGE, STALL_JUDGE, CAPABILITIES, FLEET_STORE, STANDING, TEARDOWN];

/// A person's role for the matters of its context (#3938): crew stalls
/// escalated past every supervisor, held turns waiting on a ruling, and the
/// rulings and messages a person sends to crews.
pub const OPERATOR: FleetRole = FleetRole {
    name: flotilla_protocol::PrincipalRef::OPERATOR_ROLE,
    purpose: "Hears crew stalls escalated past every supervisor and held turns, and sends a person's rulings and messages to crews.",
};

/// The person roles, declared under the fleet and under every project:
/// `fleet:<fleet>/role:<name>` and `project:<project>/role:<name>`. The person
/// consumer is bound to each of them; rebinding one later hands it to another
/// person or to an agent without re-addressing any message.
pub const PERSON_ROLES: [FleetRole; 1] = [OPERATOR];

/// The fleet roles of one fleet. The defaults can be replaced
/// or extended ([`FleetRoles::declare`]); a sender keeps sending as the same
/// role whichever handler is later bound to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FleetRoles {
    fleet: Address,
    purposes: BTreeMap<String, String>,
}

impl FleetRoles {
    /// The default roles under the fleet's own name, preserving its exact
    /// spelling. Changing that identity changes the inbox addresses and strands
    /// messages pending under the previous name; changing the charter Project does not.
    pub fn declared(fleet: &str) -> Result<Self, String> {
        if fleet.trim().is_empty() {
            return Err("FleetDesignation.spec.fleet must name a nonempty fleet".into());
        }
        let fleet = Address::fleet(fleet).map_err(|error| format!("fleet `{fleet}`: {error}"))?;
        let purposes = FLEET_ROLES.iter().map(|role| (role.name.to_owned(), role.purpose.to_owned())).collect();
        Ok(Self { fleet, purposes })
    }

    /// Resolve only the name carried by the replicated designation.
    pub fn from_designation(spec: &FleetDesignationSpec) -> Result<Self, String> {
        let fleet = spec.fleet.as_deref().ok_or_else(|| "FleetDesignation.spec.fleet is missing".to_owned())?;
        Self::declared(fleet)
    }

    /// Declare, or redeclare, one role with its purpose.
    pub fn declare(mut self, name: &str, purpose: &str) -> Result<Self, String> {
        self.fleet.child(Kind::Role, name).map_err(|error| format!("fleet role `{name}`: {error}"))?;
        self.purposes.insert(name.to_owned(), purpose.to_owned());
        Ok(self)
    }

    pub fn fleet(&self) -> &Address {
        &self.fleet
    }

    /// The declared role's inbox: `fleet:<fleet>/role:<name>`.
    pub fn address(&self, name: &str) -> Option<Address> {
        self.purposes.contains_key(name).then(|| self.fleet.child(Kind::Role, name).expect("declared role names are valid"))
    }

    pub fn purpose(&self, name: &str) -> Option<&str> {
        self.purposes.get(name).map(String::as_str)
    }

    /// Every declared role, by name.
    pub fn roles(&self) -> impl Iterator<Item = (&str, &str)> {
        self.purposes.iter().map(|(name, purpose)| (name.as_str(), purpose.as_str()))
    }

    /// The declared role an inbox names, if it is one of these.
    pub fn role_of<'a>(&self, inbox: &'a Address) -> Option<&'a str> {
        (inbox.kind() == Kind::Role && inbox.parent().as_ref() == Some(&self.fleet) && self.purposes.contains_key(inbox.name()))
            .then(|| inbox.name())
    }

    /// A person role's inbox under the fleet: `fleet:<fleet>/role:<name>`.
    pub fn person(&self, name: &str) -> Option<Address> {
        PERSON_ROLES.iter().any(|role| role.name == name).then(|| self.fleet.child(Kind::Role, name).expect("person role names are valid"))
    }

    /// The person role an inbox names, under the fleet or directly under a project.
    pub fn person_role_of<'a>(&self, inbox: &'a Address) -> Option<&'a str> {
        let parent = inbox.parent()?;
        (inbox.kind() == Kind::Role
            && (parent == self.fleet || parent.kind() == Kind::Project)
            && PERSON_ROLES.iter().any(|role| role.name == inbox.name()))
        .then(|| inbox.name())
    }

    /// The role an old `system:<name>` sender sends as. Held turns and
    /// adopted legacy messages send as turn-rules; `system:hold` survives
    /// only in their de-duplication key.
    pub fn system_sender(&self, sender: &str) -> Option<Address> {
        let name = sender.strip_prefix("system:")?;
        match name {
            "hold" | "legacy" => self.address(TURN_RULES.name),
            name => self.address(name),
        }
    }

    /// The inbox a crew seat or a `system:` producer names: `P/C/V/R` is the convoy role
    /// `project:P/convoy:C/role:R` (the vessel dropped), `P/R` the standing role
    /// `project:P/role:R`, and `system:<name>` the fleet role it names. A person and a topic have
    /// no inbox of their own here.
    pub fn inbox(&self, receiver: &str) -> Result<Address, String> {
        if receiver.starts_with("system:") {
            return self.system_sender(receiver).ok_or_else(|| format!("`{receiver}` names no declared fleet role"));
        }
        if receiver.starts_with("principal:") {
            return Err(format!("`{receiver}` is a person, whose role depends on the message's context"));
        }
        if receiver.starts_with("topic:") {
            return Err(format!("`{receiver}` is a topic; resolve its subscriber's role first"));
        }
        role_inbox(receiver)
    }

    /// What a stall is about: a whole convoy by its address, or a seat as [`Self::inbox`] reads it.
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

    /// A person's role in `project`, or under the fleet with no project (#3938). `held` are the
    /// roles `project`'s own charter declares `principal` holds: one is theirs, and several are
    /// refused rather than guessed between. With none declared, `principal:<role>` names that
    /// person role and anyone else holds the operator role, as one person consumer holds every
    /// person role at the roll.
    pub fn principal_role(&self, principal: &str, project: Option<&str>, mut held: Vec<String>) -> Result<Address, String> {
        let name =
            principal.strip_prefix("principal:").filter(|name| !name.is_empty()).ok_or_else(|| format!("`{principal}` is no person"))?;
        let default = PERSON_ROLES.iter().find(|role| role.name == name).unwrap_or(&OPERATOR).name;
        let Some(project) = project else {
            return Ok(self.person(default).expect("a declared person role"));
        };
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
}

/// The fleet identity comes only from the replicated designation.
pub async fn designated_fleet_roles(backend: &ResourceBackend, namespace: &str) -> Result<FleetRoles, ResourceError> {
    let designation = backend.definitions::<FleetDesignation>(namespace).get(FLEET_DESIGNATION_NAME).await?;
    FleetRoles::from_designation(&designation.spec).map_err(ResourceError::invalid)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Behaviour: the code-declared roles live under the fleet,
    // each with a purpose; overrides keep the same address form.
    #[test]
    fn fleet_roles_are_roles_of_the_fleet() {
        let roles = FleetRoles::declared("fleet").expect("roles");
        let names: Vec<_> = roles.roles().map(|(name, _)| name).collect();
        assert_eq!(names, ["capabilities", "fleet-store", "nudge", "stall-judge", "standing", "teardown", "turn-rules"]);
        assert!(roles.roles().all(|(_, purpose)| !purpose.is_empty() && !purpose.contains('\n')));
        let turn_rules = roles.address("turn-rules").expect("declared");
        assert_eq!(turn_rules.as_str(), "fleet:fleet/role:turn-rules");
        assert_eq!(roles.role_of(&turn_rules), Some("turn-rules"));
        assert_eq!(roles.system_sender("system:hold"), Some(turn_rules.clone()));
        // A stored legacy controller sender folds into turn rules (#3513).
        assert_eq!(roles.system_sender("system:legacy"), Some(turn_rules.clone()));
        assert_eq!(roles.system_sender("system:nudge").map(|a| a.to_string()), Some("fleet:fleet/role:nudge".into()));
        assert_eq!(roles.system_sender("system:unknown"), None);
        assert_eq!(roles.system_sender("principal:alice"), None);
        let other = FleetRoles::declared("ops").expect("roles");
        assert_eq!(other.address("nudge").map(|a| a.to_string()), Some("fleet:ops/role:nudge".into()));
        assert_eq!(other.role_of(&turn_rules), None, "another fleet names other inboxes");
        let operator = roles.person("operator").expect("person role");
        assert_eq!(operator.as_str(), "fleet:fleet/role:operator");
        assert_eq!(roles.person_role_of(&operator), Some("operator"));
        assert_eq!(roles.role_of(&operator), None, "a person role is not a system sender");
        assert_eq!(roles.system_sender("system:operator"), None);
        let project: Address = "project:flotilla/role:operator".parse().expect("project role");
        assert_eq!(roles.person_role_of(&project), Some("operator"));
        let crew: Address = "project:flotilla/convoy:demo/role:operator".parse().expect("convoy role");
        assert_eq!(roles.person_role_of(&crew), None, "a convoy's crew role is no person role");
        let overridden = roles.declare("checks", "Reports check results.").expect("override");
        assert_eq!(overridden.address("checks").map(|a| a.to_string()), Some("fleet:fleet/role:checks".into()));
    }

    // Behaviour: role addresses drop the vessel; system producers send as
    // their fleet role; people need a message's context, and topics have no inbox here.
    #[test]
    fn receivers_and_senders_map_to_host_free_role_addresses() {
        let roles = FleetRoles::declared("fleet").expect("roles");
        assert_eq!(roles.inbox("flotilla/demo/work/coder").expect("convoy role").as_str(), "project:flotilla/convoy:demo/role:coder");
        assert_eq!(roles.inbox("flotilla/gui-approver").expect("standing role").as_str(), "project:flotilla/role:gui-approver");
        assert_eq!(roles.inbox("system:turn-rules").expect("fleet role").as_str(), "fleet:fleet/role:turn-rules");
        assert_eq!(roles.inbox("system:hold").expect("held turns").as_str(), "fleet:fleet/role:turn-rules");
        assert!(roles.inbox("principal:alice").is_err(), "a person has no address outside a message's context");
        assert!(roles.inbox("topic:flotilla/reviews").is_err());
        assert!(roles.inbox("system:unknown").is_err());
    }

    // Behaviour: a standing role's launch is named by its own address, escaped in the old
    // receiver's segment; the inbox is that launch's role, not an address of the escaped text.
    #[test]
    fn a_standing_launch_receiver_is_its_launch_role() {
        let roles = FleetRoles::declared("fleet").expect("roles");
        let launch = format!("{}#5", flotilla_resources::convoy::engine::standing_name("flotilla", "governor").expect("standing name"));
        let receiver = flotilla_resources::crew_role_address("flotilla", &launch, "govern", "governor");
        let project = Address::project("flotilla").expect("project");
        let expected = flotilla_resources::convoy::engine::launch_address(&project, &launch)
            .expect("launch")
            .child(Kind::Role, "governor")
            .expect("role");
        assert_eq!(roles.inbox(&receiver).expect("standing launch role").as_str(), expected.as_str());
    }

    // Behaviour (#3938): a person is the role they hold in a project, the fleet's with no
    // project; a role the charter declares for them is theirs, and several are refused.
    #[test]
    fn a_person_is_the_role_they_hold_in_a_project() {
        let roles = FleetRoles::declared("fleet").expect("roles");
        let role = |principal: &str, project: Option<&str>, held: &[&str]| {
            roles.principal_role(principal, project, held.iter().map(|role| (*role).to_owned()).collect())
        };
        assert_eq!(role("principal:alice", Some("flotilla"), &[]).expect("operator").as_str(), "project:flotilla/role:operator");
        assert_eq!(role("principal:operator", Some("flotilla"), &[]).expect("operator").as_str(), "project:flotilla/role:operator");
        assert_eq!(role("principal:bob", Some("flotilla"), &["owner"]).expect("owner").as_str(), "project:flotilla/role:owner");
        let ambiguous = role("principal:eve", Some("flotilla"), &["owner", "lead"]).expect_err("ambiguous");
        assert!(ambiguous.contains("`lead`, `owner`"), "{ambiguous}");
        assert_eq!(role("principal:alice", None, &[]).expect("fleet").as_str(), "fleet:fleet/role:operator");
        assert!(role("flotilla/demo/work/coder", None, &[]).is_err(), "no person");
    }
}
