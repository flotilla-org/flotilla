//! The fleet roles the system sends as (#3317): code-declared destinations
//! under the fleet's designated project (ADR 0066 §4, owner ruling
//! 2026-10-19). A role is an address that can be bound to any handler;
//! which host or handler takes its messages is a binding, never part of the
//! address, so rebinding a role touches no stored message.

use std::collections::BTreeMap;

use flotilla_address::{Address, Kind};
use flotilla_resources::{FleetDesignation, ResourceError, FLEET_DESIGNATION_NAME};
use flotilla_store::ResourceBackend;

/// A declared fleet role: its name under the designated project, and what it does.
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

/// The fleet roles of one designated project. The defaults can be replaced
/// or extended ([`FleetRoles::declare`]); a sender keeps sending as the same
/// role whichever handler is later bound to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FleetRoles {
    project: Address,
    purposes: BTreeMap<String, String>,
}

impl FleetRoles {
    /// The five default roles under `project`, the designated fleet project.
    /// Changing the designation changes these addresses and strands the
    /// messages pending in the old inboxes (a known limit).
    pub fn declared(project: &str) -> Result<Self, String> {
        let project = Address::project(project).map_err(|error| format!("fleet project `{project}`: {error}"))?;
        let purposes = FLEET_ROLES.iter().map(|role| (role.name.to_owned(), role.purpose.to_owned())).collect();
        Ok(Self { project, purposes })
    }

    /// Declare, or redeclare, one role with its purpose.
    pub fn declare(mut self, name: &str, purpose: &str) -> Result<Self, String> {
        self.project.child(Kind::Role, name).map_err(|error| format!("fleet role `{name}`: {error}"))?;
        self.purposes.insert(name.to_owned(), purpose.to_owned());
        Ok(self)
    }

    pub fn project(&self) -> &Address {
        &self.project
    }

    /// The declared role's inbox: `project:<designated>/role:<name>`.
    pub fn address(&self, name: &str) -> Option<Address> {
        self.purposes.contains_key(name).then(|| self.project.child(Kind::Role, name).expect("declared role names are valid"))
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
        (inbox.kind() == Kind::Role && inbox.parent().as_ref() == Some(&self.project) && self.purposes.contains_key(inbox.name()))
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

/// The fleet roles under the designated project, read from the
/// `FleetDesignation` record; never a project name fixed in code.
pub async fn designated_fleet_roles(backend: &ResourceBackend, namespace: &str) -> Result<FleetRoles, ResourceError> {
    let designation = backend.definitions::<FleetDesignation>(namespace).get(FLEET_DESIGNATION_NAME).await?;
    FleetRoles::declared(&designation.spec.project).map_err(ResourceError::invalid)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Behaviour: five code-declared roles live under the designated project,
    // each with a purpose; overrides keep the same address form.
    #[test]
    fn fleet_roles_are_roles_of_the_designated_project() {
        let roles = FleetRoles::declared("fleet").expect("roles");
        let names: Vec<_> = roles.roles().map(|(name, _)| name).collect();
        assert_eq!(names, ["capabilities", "fleet-store", "nudge", "stall-judge", "turn-rules"]);
        assert!(roles.roles().all(|(_, purpose)| !purpose.is_empty() && !purpose.contains('\n')));
        let turn_rules = roles.address("turn-rules").expect("declared");
        assert_eq!(turn_rules.as_str(), "project:fleet/role:turn-rules");
        assert_eq!(roles.role_of(&turn_rules), Some("turn-rules"));
        assert_eq!(roles.system_sender("system:hold"), Some(turn_rules.clone()));
        assert_eq!(roles.system_sender("system:nudge").map(|a| a.to_string()), Some("project:fleet/role:nudge".into()));
        assert_eq!(roles.system_sender("system:unknown"), None);
        assert_eq!(roles.system_sender("principal:alice"), None);
        let other = FleetRoles::declared("ops").expect("roles");
        assert_eq!(other.address("nudge").map(|a| a.to_string()), Some("project:ops/role:nudge".into()));
        assert_eq!(other.role_of(&turn_rules), None, "another designation names other inboxes");
        let overridden = roles.declare("checks", "Reports check results.").expect("override");
        assert_eq!(overridden.address("checks").map(|a| a.to_string()), Some("project:fleet/role:checks".into()));
    }
}
