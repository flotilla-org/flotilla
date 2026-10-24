//! Fleet roles use the fleet identity, independently of its charter Project.
//! Handler bindings can change without changing a role address.

use std::collections::BTreeMap;

use flotilla_address::{Address, Kind};
use flotilla_resources::{FleetDesignation, FleetDesignationSpec, ResourceError, FLEET_DESIGNATION_NAME};
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
pub const CAPABILITIES: FleetRole =
    FleetRole { name: "capabilities", purpose: "Tells a crew when the credentials and tools it may use change." };
pub const FLEET_STORE: FleetRole =
    FleetRole { name: "fleet-store", purpose: "Tells a crew when the charter of the project it works under changes." };

/// The defaults the system sends as, in declaration order.
pub const FLEET_ROLES: [FleetRole; 5] = [TURN_RULES, NUDGE, STALL_JUDGE, CAPABILITIES, FLEET_STORE];

/// The fleet roles of one fleet. The defaults can be replaced
/// or extended ([`FleetRoles::declare`]); a sender keeps sending as the same
/// role whichever handler is later bound to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FleetRoles {
    fleet: Address,
    purposes: BTreeMap<String, String>,
}

impl FleetRoles {
    /// The five default roles under the fleet's own name, preserving its exact
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
}

/// The fleet identity comes only from the replicated designation.
pub async fn designated_fleet_roles(backend: &ResourceBackend, namespace: &str) -> Result<FleetRoles, ResourceError> {
    let designation = backend.definitions::<FleetDesignation>(namespace).get(FLEET_DESIGNATION_NAME).await?;
    FleetRoles::from_designation(&designation.spec).map_err(ResourceError::invalid)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Behaviour: five code-declared roles live under the fleet,
    // each with a purpose; overrides keep the same address form.
    #[test]
    fn fleet_roles_are_roles_of_the_fleet() {
        let roles = FleetRoles::declared("fleet").expect("roles");
        let names: Vec<_> = roles.roles().map(|(name, _)| name).collect();
        assert_eq!(names, ["capabilities", "fleet-store", "nudge", "stall-judge", "turn-rules"]);
        assert!(roles.roles().all(|(_, purpose)| !purpose.is_empty() && !purpose.contains('\n')));
        let turn_rules = roles.address("turn-rules").expect("declared");
        assert_eq!(turn_rules.as_str(), "fleet:fleet/role:turn-rules");
        assert_eq!(roles.role_of(&turn_rules), Some("turn-rules"));
        assert_eq!(roles.system_sender("system:hold"), Some(turn_rules.clone()));
        assert_eq!(roles.system_sender("system:nudge").map(|a| a.to_string()), Some("fleet:fleet/role:nudge".into()));
        assert_eq!(roles.system_sender("system:unknown"), None);
        assert_eq!(roles.system_sender("principal:alice"), None);
        let other = FleetRoles::declared("ops").expect("roles");
        assert_eq!(other.address("nudge").map(|a| a.to_string()), Some("fleet:ops/role:nudge".into()));
        assert_eq!(other.role_of(&turn_rules), None, "another fleet names other inboxes");
        let overridden = roles.declare("checks", "Reports check results.").expect("override");
        assert_eq!(overridden.address("checks").map(|a| a.to_string()), Some("fleet:fleet/role:checks".into()));
    }
}
