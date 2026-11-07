//! A client's context, and completing the convoy addresses its caller types from it (#4272).
//!
//! Every client (`flotilla`, `flotilla-table`, `flotilla-mcp` as each takes addresses) completes
//! what its caller typed through [`complete_convoy`] before sending it, so the address on the
//! wire is exact: the daemon's lookup never completes anything. The context is an ADR 0066
//! address assembled by [`ContextSources`]; which environment variables feed it is still open
//! (#4105), so they are read only in [`ContextSources::from_env`].
use flotilla_address::{complete, Address, Kind};
use flotilla_daemon_api::daemon::DaemonHandle;
use flotilla_protocol::{Command, CommandAction, CommandValue, RepoSelector};
use flotilla_resources::{PROJECT_LABEL, ROLE_LABEL};

use crate::resource::{ResourceClient, ResourceListRequest};

/// What a caller's context is assembled from: whatever sources exist.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContextSources {
    /// The project of the checkout the caller is in.
    pub checkout_project: Option<String>,
    /// The record name of the vessel's own convoy (`FLOTILLA_CONVOY`).
    pub convoy: Option<String>,
    /// The vessel's own role (`FLOTILLA_CREW_ROLE`).
    pub role: Option<String>,
}

impl ContextSources {
    /// The vessel's convoy and role from `env`; an empty variable is unset. The checkout's
    /// project is the caller's to find, and only when [`Self::convoy`] is `None`: a vessel's own
    /// convoy is the deeper context.
    pub fn from_env(env: impl Fn(&str) -> Option<String>) -> Self {
        let set = |name| env(name).filter(|value: &String| !value.is_empty());
        Self { checkout_project: None, convoy: set("FLOTILLA_CONVOY"), role: set("FLOTILLA_CREW_ROLE") }
    }

    /// The context address: the vessel's own convoy, under its project, with its role; or the
    /// checkout's project; or none.
    pub fn address(&self, known: &[KnownConvoy]) -> Option<Address> {
        if let Some(record) = &self.convoy {
            let project = known.iter().find(|convoy| &convoy.record == record).and_then(|convoy| convoy.project.as_deref());
            let convoy = KnownConvoy::address(project, record)?;
            return Some(self.role.as_deref().and_then(|role| convoy.child(Kind::Role, role).ok()).unwrap_or(convoy));
        }
        Address::project(self.checkout_project.as_deref()?).ok()
    }
}

/// A convoy the daemon holds: its record, and the role and project it is addressed by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnownConvoy {
    pub record: String,
    pub role: Option<String>,
    pub project: Option<String>,
}

impl KnownConvoy {
    fn address(project: Option<&str>, name: &str) -> Option<Address> {
        match project {
            Some(project) => Address::project(project).and_then(|project| project.child(Kind::Convoy, name)).ok(),
            None => Address::convoy(name).ok(),
        }
    }

    /// The addresses naming this convoy exactly: its role's, and its record's.
    fn addresses(&self) -> impl Iterator<Item = Address> + '_ {
        self.role.iter().chain([&self.record]).filter_map(|name| Self::address(self.project.as_deref(), name))
    }
}

/// Completes the convoy reference `typed` from the caller's context, as the daemon's exact
/// lookup reads it: `<name>@<project>`, or `<name>` for a convoy of no project.
///
/// `typed` is an ADR 0066 address (`convoy:lander`, `project:flotilla/convoy:lander`), the
/// CLI's `<name>@<project>` or `<name>@` (of no project), which are whole, or a bare `<name>`,
/// which is `convoy:<name>`.
/// `known` gives the vessel's own convoy its project, and the candidates when the context does
/// not place the reference.
pub fn complete_convoy(typed: &str, sources: &ContextSources, known: &[KnownConvoy]) -> Result<String, String> {
    let partial = match typed.split_once('@') {
        _ if typed.contains(':') => typed.parse::<Address>().map_err(|error| format!("convoy `{typed}`: {error}"))?,
        Some((name, project)) if !project.is_empty() => {
            KnownConvoy::address(Some(project), name).ok_or_else(|| format!("convoy `{typed}` is not an address"))?
        }
        // `<name>@` names the convoy of no project explicitly: whole, so it goes as typed.
        Some((name, _)) => {
            KnownConvoy::address(None, name).ok_or_else(|| format!("convoy `{typed}` is not an address"))?;
            return Ok(typed.to_owned());
        }
        None => KnownConvoy::address(None, typed).ok_or_else(|| format!("convoy `{typed}` is not an address"))?,
    };
    if partial.kind() != Kind::Convoy {
        return Err(format!("`{typed}` is not a convoy address"));
    }
    let candidates: Vec<Address> = known.iter().flat_map(KnownConvoy::addresses).collect();
    let context = sources.address(known);
    let mut address = complete(&partial, context.as_ref(), &candidates).map_err(|error| error.to_string())?;
    // A record name is its record's key, so a bare one the context placed where no convoy answers
    // names that record under its own project (`convoy explain convoy-<uuid>` from any checkout).
    if partial.segments().len() == 1 && !candidates.contains(&address) {
        if let Some(record) = known.iter().find(|convoy| convoy.record == partial.name()) {
            address = KnownConvoy::address(record.project.as_deref(), &record.record).unwrap_or(address);
        }
    }
    Ok(match address.get(Kind::Project) {
        Some(project) => format!("{}@{project}", address.name()),
        None => address.name().to_owned(),
    })
}

/// The convoys of `namespace`, for completion. Reading them is best effort: a caller that may
/// not list convoys completes from its context alone.
pub async fn known_convoys(daemon: std::sync::Arc<dyn DaemonHandle>, namespace: &str) -> Vec<KnownConvoy> {
    let request = ResourceListRequest::builder().kind("convoys".into()).namespace(namespace.into()).include_replicas(true).build();
    let Ok(listed) = ResourceClient::new(daemon).list(request).await else { return Vec::new() };
    listed
        .records
        .iter()
        .filter_map(|record| record.object.as_ref())
        .filter_map(|object| {
            let metadata = object.get("metadata")?;
            let label = |name: &str| metadata["labels"][name].as_str().filter(|value| !value.is_empty()).map(str::to_owned);
            Some(KnownConvoy { record: metadata.get("name")?.as_str()?.to_owned(), role: label(ROLE_LABEL), project: label(PROJECT_LABEL) })
        })
        .collect()
}

/// The project of the checkout at `path`: the one project listing its repository, if exactly one
/// does. Best effort, like [`known_convoys`].
pub async fn checkout_project(daemon: &dyn DaemonHandle, path: std::path::PathBuf) -> Option<String> {
    let resolve = Command::builder().action(CommandAction::QueryResolveRepository { repo: RepoSelector::Path(path) }).build();
    let CommandValue::RepositoryResolved { key: Some(key) } = daemon.execute_query(resolve, uuid::Uuid::new_v4()).await.ok()? else {
        return None;
    };
    let list = Command::builder().action(CommandAction::QueryProjectList {}).build();
    let CommandValue::ProjectList(projects) = daemon.execute_query(list, uuid::Uuid::new_v4()).await.ok()? else { return None };
    let mut owning = projects.projects.into_iter().filter(|project| project.repositories.iter().any(|repository| repository.key == key));
    let project = owning.next()?;
    owning.next().is_none().then_some(project.name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn known(record: &str, role: &str, project: Option<&str>) -> KnownConvoy {
        KnownConvoy { record: record.into(), role: Some(role.into()), project: project.map(str::to_owned) }
    }

    fn convoys() -> Vec<KnownConvoy> {
        vec![
            known("convoy-1", "lander", Some("flotilla")),
            known("convoy-2", "lander", Some("other")),
            known("convoy-3", "coder", Some("flotilla")),
            known("convoy-4", "loner", None),
        ]
    }

    // #4272 done-when: in a flotilla checkout `convoy:lander` (or `lander`) is flotilla's lander;
    // inside a vessel the crew's own convoy completes under its project, even with no checkout;
    // with no context `lander` in two projects lists both, and a unique match is taken. The
    // context is injected, never read from the real environment.
    #[test]
    fn convoy_references_complete_from_the_callers_context() {
        let checkout = ContextSources { checkout_project: Some("flotilla".into()), ..ContextSources::default() };
        for typed in ["convoy:lander", "lander"] {
            assert_eq!(complete_convoy(typed, &checkout, &convoys()), Ok("lander@flotilla".into()), "{typed}");
        }
        let vessel = ContextSources::from_env(|name| match name {
            "FLOTILLA_CONVOY" => Some("convoy-3".into()),
            "FLOTILLA_CREW_ROLE" => Some("coder".into()),
            _ => None,
        });
        assert_eq!(
            vessel.address(&convoys()).map(|address| address.to_string()).as_deref(),
            Some("project:flotilla/convoy:convoy-3/role:coder")
        );
        assert_eq!(complete_convoy("convoy-3", &vessel, &convoys()), Ok("convoy-3@flotilla".into()));
        assert_eq!(complete_convoy("lander", &vessel, &convoys()), Ok("lander@flotilla".into()));
        // An empty `FLOTILLA_CONVOY` is no vessel: the checkout is asked, and places the reference.
        let mut unset = ContextSources::from_env(|name| (name == "FLOTILLA_CONVOY").then(String::new));
        assert_eq!(unset.convoy, None);
        unset.checkout_project = Some("other".into());
        assert_eq!(complete_convoy("lander", &unset, &convoys()), Ok("lander@other".into()));

        let none = ContextSources::default();
        let ambiguous = complete_convoy("lander", &none, &convoys()).unwrap_err();
        assert!(ambiguous.contains("project:flotilla/convoy:lander") && ambiguous.contains("project:other/convoy:lander"), "{ambiguous}");
        assert_eq!(complete_convoy("coder", &none, &convoys()), Ok("coder@flotilla".into()));
        assert_eq!(complete_convoy("loner", &none, &convoys()), Ok("loner".into()));
        // Whole references go as typed; an unknown one is left for the exact lookup to answer.
        assert_eq!(complete_convoy("lander@other", &checkout, &convoys()), Ok("lander@other".into()));
        // `<name>@` is the convoy of no project, never completed into the checkout's project.
        let mut projectless = convoys();
        projectless.push(known("convoy-5", "lander", None));
        assert_eq!(complete_convoy("lander@", &checkout, &projectless), Ok("lander@".into()));
        assert_eq!(complete_convoy("loner@", &checkout, &convoys()), Ok("loner@".into()));
        assert_eq!(complete_convoy("project:other/convoy:lander", &checkout, &convoys()), Ok("lander@other".into()));
        assert_eq!(complete_convoy("missing", &none, &convoys()), Ok("missing".into()));
        assert_eq!(complete_convoy("missing", &checkout, &convoys()), Ok("missing@flotilla".into()));
        // A record name names its record from any context; a role it shares a name with does not
        // give way to it.
        assert_eq!(complete_convoy("convoy-2", &checkout, &convoys()), Ok("convoy-2@other".into()));
        let mut shadowed = convoys();
        shadowed.push(known("lander", "reviewer", Some("other")));
        assert_eq!(complete_convoy("lander", &checkout, &shadowed), Ok("lander@flotilla".into()));
        assert!(complete_convoy("project:flotilla", &none, &convoys()).is_err(), "a project is no convoy");
    }
}
