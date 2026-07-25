//! Canonical presentation-entity identities.
//!
//! Producers must use these constructors rather than formatting ids at call
//! sites. One entity kind has one id dialect, so two observations of the same
//! thing fold onto the same wire target.

use std::fmt;

use flotilla_protocol::{IssueRef, ResourceRef};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EntityKind {
    Project,
    Repo,
    Convoy,
    Vessel,
    Issue,
    Session,
    Checkout,
}

impl EntityKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::Repo => "repo",
            Self::Convoy => "convoy",
            Self::Vessel => "vessel",
            Self::Issue => "issue",
            Self::Session => "session",
            Self::Checkout => "checkout",
        }
    }
}

impl fmt::Display for EntityKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct EntityRef {
    pub kind: EntityKind,
    pub id: String,
}

impl EntityRef {
    pub fn new(kind: EntityKind, id: impl Into<String>) -> Self {
        Self { kind, id: id.into() }
    }

    /// Stable value used by action facts and live-tab matching.
    pub fn action_target(&self) -> String {
        format!("{}:{}", self.kind, self.id)
    }
}

/// The origin component of control-plane resource identities.
///
/// Fleet rows pin remote resources to their host. Locally-merged rows may
/// omit the host; `fleet` is the canonical origin for that shared store.
pub fn resource_origin(resource: &ResourceRef) -> String {
    resource.host.as_ref().map(ToString::to_string).unwrap_or_else(|| "fleet".to_owned())
}

pub fn project(namespace: &str, name: &str, origin: &str) -> EntityRef {
    EntityRef::new(EntityKind::Project, format!("{namespace}/{name}@{origin}"))
}

pub fn repo(forge_slug: &str) -> EntityRef {
    EntityRef::new(EntityKind::Repo, forge_slug)
}

pub fn convoy(namespace: &str, name: &str, origin: &str) -> EntityRef {
    EntityRef::new(EntityKind::Convoy, format!("{namespace}/{name}@{origin}"))
}

pub fn vessel(namespace: &str, convoy_name: &str, vessel_name: &str, origin: &str) -> EntityRef {
    EntityRef::new(EntityKind::Vessel, format!("{namespace}/{convoy_name}/{vessel_name}@{origin}"))
}

pub fn issue(reference: &IssueRef) -> EntityRef {
    EntityRef::new(EntityKind::Issue, format!("{}/{}#{}", reference.source.service, reference.source.scope, reference.id))
}

pub fn session(session_ref: &str) -> EntityRef {
    EntityRef::new(EntityKind::Session, session_ref)
}

pub fn checkout(checkout_ref: &str) -> EntityRef {
    EntityRef::new(EntityKind::Checkout, checkout_ref)
}

#[cfg(test)]
mod tests {
    use flotilla_protocol::{HostName, IssueSource};

    use super::*;

    #[test]
    fn constructors_pin_one_id_dialect_per_kind() {
        assert_eq!(convoy("dev", "cutover", "kiwi").id, "dev/cutover@kiwi");
        assert_eq!(vessel("dev", "cutover", "coder", "kiwi").id, "dev/cutover/coder@kiwi");
        assert_eq!(repo("github.com:flotilla-org/flotilla").id, "github.com:flotilla-org/flotilla");
        assert_eq!(
            issue(&IssueRef {
                source: IssueSource { service: "https://github.com".to_owned(), scope: "flotilla-org/flotilla".to_owned() },
                id: "982".to_owned(),
            })
            .id,
            "https://github.com/flotilla-org/flotilla#982"
        );
        assert_eq!(session("feta/dev/terminal-coder").id, "feta/dev/terminal-coder");
    }

    #[test]
    fn resource_origin_prefers_pinned_host_and_has_a_stable_fleet_fallback() {
        let local = ResourceRef::new("flotilla.work/v1", "Convoy", "dev", "cutover");
        let remote = local.clone().on_host(HostName::new("kiwi"));
        assert_eq!(resource_origin(&local), "fleet");
        assert_eq!(resource_origin(&remote), "kiwi");
    }
}
