//! Canonical presentation-entity identities.
//!
//! Producers must use these constructors rather than formatting ids at call
//! sites. One entity kind has one id dialect, so two observations of the same
//! thing fold onto the same wire target.

use flotilla_address::{Address, Forge, Kind};
use flotilla_protocol::{HostName, ResourceRef};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct EntityRef {
    /// Connection-scoped provider; Wheelhouse assigns it for published catalogs.
    #[serde(default)]
    pub provider: String,
    pub kind: String,
    pub id: String,
}

impl EntityRef {
    pub fn new(kind: impl Into<String>, id: impl Into<String>) -> Self {
        Self { provider: String::new(), kind: kind.into(), id: id.into() }
    }

    /// Stable value used by action facts and live-tab matching.
    pub fn action_target(&self) -> String {
        format!("{}:{}", self.kind, self.id)
    }
}

/// The record's engine home, the `host:` segment of its address. A record
/// without one has no entity: there is no default home.
pub fn home(resource: &ResourceRef) -> Option<&str> {
    resource.host.as_ref().map(HostName::as_str)
}

fn address(result: Result<Address, flotilla_address::Error>, what: &str) -> Address {
    result.unwrap_or_else(|error| panic!("{what} is not a valid address component: {error}"))
}

fn convoy_address(home: &str, launch: &str) -> Address {
    address(Address::root(Kind::Host, home).and_then(|host| host.child(Kind::Convoy, launch)), "a convoy launch")
}

/// A project: `project:<name>`.
pub fn project(name: &str) -> EntityRef {
    EntityRef::new("project", address(Address::project(name), "a project name").to_string())
}

pub fn repo(forge_slug: &str) -> EntityRef {
    EntityRef::new("repo", forge_slug)
}

/// Length-prefixed components keep repository keys and subpaths unambiguous.
pub fn project_repository(namespace: &str, project: &str, repository_key: &str, subpath: Option<&str>) -> EntityRef {
    let component = |value: &str| format!("{}:{value}", value.len());
    let subpath_presence = if subpath.is_some() { "1" } else { "0" };
    EntityRef::new(
        "project_repository",
        format!(
            "{}{}{}{subpath_presence}{}",
            component(namespace),
            component(project),
            component(repository_key),
            component(subpath.unwrap_or(""))
        ),
    )
}

/// A convoy launch, by the home that holds its record: `host:<home>/convoy:<launch>`.
pub fn convoy(home: &str, launch: &str) -> EntityRef {
    EntityRef::new("convoy", convoy_address(home, launch).to_string())
}

/// A standing project role, stable across the convoy attempts backing it:
/// `project:<project>/role:<role>`.
pub fn role(project: &str, role: &str) -> EntityRef {
    let address = address(Address::project(project).and_then(|project| project.child(Kind::Role, role)), "a standing role");
    EntityRef::new("role", address.to_string())
}

/// A convoy's vessel: `host:<home>/convoy:<launch>/vessel:<name>`.
pub fn vessel(home: &str, launch: &str, vessel_name: &str) -> EntityRef {
    let address = address(convoy_address(home, launch).child(Kind::Vessel, vessel_name), "a vessel name");
    EntityRef::new("vessel", address.to_string())
}

/// An issue: `forge:<id>/repo:<owner/repo>/issue:<number>`. `None` when the
/// scope or number is not one the address grammar admits.
pub fn issue(forge: &Forge, scope: &str, number: &str) -> Option<EntityRef> {
    let number = number.parse().ok()?;
    Some(EntityRef::new("issue", Address::issue(forge, scope, number).ok()?.to_string()))
}

/// A forge-relative change request: `forge:<id>/repo:<owner/repo>/cr:<number>`.
pub fn change_request(forge: &Forge, scope: &str, number: &str) -> Option<EntityRef> {
    let number = number.parse().ok()?;
    Some(EntityRef::new("change_request", Address::change_request(forge, scope, number).ok()?.to_string()))
}

pub fn forge(forge: &Forge) -> EntityRef {
    EntityRef::new("forge", Address::forge(forge).to_string())
}

pub fn session(session_ref: &str) -> EntityRef {
    EntityRef::new("session", session_ref)
}

#[cfg(test)]
mod tests {
    use flotilla_protocol::HostName;

    use super::*;

    #[test]
    fn constructors_build_canonical_address_text() {
        assert_eq!(convoy("feta", "convoy-7da0#8661").id, "host:feta/convoy:convoy-7da0%238661");
        assert_eq!(vessel("feta", "fix#8", "work").id, "host:feta/convoy:fix%238/vessel:work");
        assert_eq!(role("ghostty", "governor").id, "project:ghostty/role:governor");
        assert_eq!(project("flotilla").id, "project:flotilla");
        let github = Forge::github();
        assert_eq!(
            change_request(&github, "flotilla-org/flotilla", "4407").expect("cr").id,
            "forge:github/repo:flotilla-org%2Fflotilla/cr:4407"
        );
        assert_eq!(
            issue(&github, "flotilla-org/flotilla", "4406").expect("issue").id,
            "forge:github/repo:flotilla-org%2Fflotilla/issue:4406"
        );
        assert_eq!(forge(&github).id, "forge:github");
        assert_eq!(repo("github.com:flotilla-org/flotilla").id, "github.com:flotilla-org/flotilla");
        assert_eq!(session("feta/dev/terminal-coder").id, "feta/dev/terminal-coder");
    }

    #[test]
    fn a_standing_launch_name_stays_one_segment() {
        let launch = "project:x/convoy:governor#3";
        let id = convoy("feta", launch).id;
        assert_eq!(id.parse::<Address>().expect("canonical").get(Kind::Convoy), Some(launch));
    }

    #[test]
    fn home_is_the_records_host_and_never_defaults() {
        let local = ResourceRef::new("flotilla.work/v1", "Convoy", "dev", "cutover");
        assert_eq!(home(&local), None);
        assert_eq!(home(&local.on_host(HostName::new("kiwi"))), Some("kiwi"));
    }

    #[test]
    fn entity_kind_is_an_open_wire_string() {
        let entity = EntityRef::new("deployment", "prod/api");
        let json = serde_json::to_string(&entity).expect("serialize novel kind");
        let decoded: EntityRef = serde_json::from_str(&json).expect("deserialize novel kind");

        assert_eq!(json, r#"{"provider":"","kind":"deployment","id":"prod/api"}"#);
        assert_eq!(decoded, entity);
    }
}
