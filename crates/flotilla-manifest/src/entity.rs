//! Canonical presentation-entity identities.
//!
//! Producers must use these constructors rather than formatting ids at call
//! sites. One entity kind has one id dialect, so two observations of the same
//! thing fold onto the same wire target.

use flotilla_address::{Address, Forge, Kind};
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

/// An id the address grammar refuses. The producer skips that record and says
/// why; it never builds a malformed id and never panics.
pub type Error = flotilla_address::Error;

fn convoy_address(home: &str, launch: &str) -> Result<Address, Error> {
    Address::root(Kind::Host, home)?.child(Kind::Convoy, launch)
}

/// A project: `project:<name>`.
pub fn project(name: &str) -> Result<EntityRef, Error> {
    Ok(EntityRef::new("project", Address::project(name)?.to_string()))
}

pub fn repo(forge_slug: &str) -> EntityRef {
    EntityRef::new("repo", forge_slug)
}

/// A repository's membership in a project. The address grammar has no spelling
/// for it, so it is the project's id and the repository key and subpath, each
/// length-prefixed so the components stay unambiguous. No namespace is in it.
pub fn project_repository(project: &str, repository_key: &str, subpath: Option<&str>) -> EntityRef {
    let component = |value: &str| format!("{}:{value}", value.len());
    let subpath_presence = if subpath.is_some() { "1" } else { "0" };
    EntityRef::new(
        "project_repository",
        format!("{}{}{subpath_presence}{}", component(project), component(repository_key), component(subpath.unwrap_or(""))),
    )
}

/// A convoy launch, by the home that holds its record: `host:<home>/convoy:<launch>`.
pub fn convoy(home: &str, launch: &str) -> Result<EntityRef, Error> {
    Ok(EntityRef::new("convoy", convoy_address(home, launch)?.to_string()))
}

/// A standing project role, stable across the convoy attempts backing it:
/// `project:<project>/role:<role>`.
pub fn role(project: &str, role: &str) -> Result<EntityRef, Error> {
    Ok(EntityRef::new("role", Address::project(project)?.child(Kind::Role, role)?.to_string()))
}

/// A convoy's vessel: `host:<home>/convoy:<launch>/vessel:<name>`.
pub fn vessel(home: &str, launch: &str, vessel_name: &str) -> Result<EntityRef, Error> {
    Ok(EntityRef::new("vessel", convoy_address(home, launch)?.child(Kind::Vessel, vessel_name)?.to_string()))
}

/// An issue by its address: `forge:<id>/repo:<owner/repo>/issue:<number>`.
pub fn issue(address: &Address) -> EntityRef {
    EntityRef::new("issue", address.to_string())
}

/// A forge-relative change request by its address: `forge:<id>/repo:<owner/repo>/cr:<number>`.
pub fn change_request(address: &Address) -> EntityRef {
    EntityRef::new("change_request", address.to_string())
}

pub fn forge(forge: &Forge) -> EntityRef {
    EntityRef::new("forge", Address::forge(forge).to_string())
}

/// A terminal session, by the host that acts on it: `<host>/<name>`. The
/// address grammar spells an agent session only under a role, so this is not an
/// address; it carries no namespace.
pub fn session(host: &str, name: &str) -> EntityRef {
    EntityRef::new("session", format!("{host}/{name}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constructors_build_canonical_address_text() {
        assert_eq!(convoy("feta", "convoy-7da0#8661").expect("convoy").id, "host:feta/convoy:convoy-7da0%238661");
        assert_eq!(vessel("feta", "fix#8", "work").expect("vessel").id, "host:feta/convoy:fix%238/vessel:work");
        assert_eq!(role("ghostty", "governor").expect("role").id, "project:ghostty/role:governor");
        assert_eq!(project("flotilla").expect("project").id, "project:flotilla");
        let github = Forge::github();
        let cr = Address::change_request(&github, "flotilla-org/flotilla", 4407).expect("address");
        assert_eq!(change_request(&cr).id, "forge:github/repo:flotilla-org%2Fflotilla/cr:4407");
        let issue_address = Address::issue(&github, "flotilla-org/flotilla", 4406).expect("address");
        assert_eq!(issue(&issue_address).id, "forge:github/repo:flotilla-org%2Fflotilla/issue:4406");
        assert_eq!(forge(&github).id, "forge:github");
        assert_eq!(repo("github.com:flotilla-org/flotilla").id, "github.com:flotilla-org/flotilla");
        assert_eq!(session("feta", "terminal-coder").id, "feta/terminal-coder");
    }

    #[test]
    fn a_standing_launch_name_stays_one_segment() {
        let launch = "project:x/convoy:governor#3";
        let id = convoy("feta", launch).expect("convoy").id;
        assert_eq!(id.parse::<Address>().expect("canonical").get(Kind::Convoy), Some(launch));
    }

    #[test]
    fn a_name_the_grammar_refuses_is_an_error_not_a_panic() {
        assert!(convoy("", "fix").is_err());
        assert!(convoy("feta", "").is_err());
        assert!(vessel("feta", "fix", "").is_err());
        assert!(role("ghostty", "").is_err());
        assert!(project("").is_err());
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
