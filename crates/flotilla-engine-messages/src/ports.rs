//! Transition ports (#3317, ruling 5): the reads of the records the engine
//! path's delivery and gates make, so each kind that moves to the engine
//! later swaps one implementation and touches no caller.
//!
//! - [`RoleBindings`]: which live crew session holds which role inboxes,
//!   resolved as today's role routing (`MessageReceiverSnapshot`) resolves
//!   them, over the records those sessions name.
//! - [`ConditionReads`]: a condition's current value.
//!
//! Both read through [`KeyedReads`], each record by its key.
//!
//! [`HomeReads`] is not an old-store port: it is the routed point read of an
//! engine record at its home, for a miss in this home's replica.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use async_trait::async_trait;
use flotilla_address::{Address, Condition, Kind};
use flotilla_resources::{
    Convoy, ConvoyEnsure, ReadResourceObject, ResourceObject, ResourceProvenance, TerminalSession, TerminalSessionPhase,
    TerminalSessionSource, ThreeValue, CONVOY_LABEL, ENSURED_FROM_ANNOTATION, ROLE_ADDRESS_ANNOTATION, ROLE_LABEL, VESSEL_LABEL,
};
use flotilla_store::{ResourceBackend, WeakResourceBackend};

use super::{
    keyed_reads::{snapshot_work, KeyedReads},
    roles::FleetRoles,
    view::condition_leaf,
};

/// A live crew session at this home and the role inboxes it holds.
#[derive(Debug, Clone)]
pub struct RoleHolder {
    pub terminal: ResourceObject<TerminalSession>,
    pub session_id: String,
    pub inboxes: BTreeSet<Address>,
}

/// The bindings port: who holds each role at this home.
#[async_trait]
pub trait RoleBindings: Send + Sync {
    async fn holders(&self, namespace: &str, roles: &FleetRoles) -> Result<Vec<RoleHolder>, String>;
}

/// The conditions port: a condition's current value.
#[async_trait]
pub trait ConditionReads: Send + Sync {
    async fn value(&self, namespace: &str, condition: &Condition) -> Result<ThreeValue, String>;
}

/// Bindings from today's role routing: a role is held by the session its
/// `MessageReceiverSnapshot` resolves to, for a convoy role, an adopted
/// claim and a standing role a declaration names. The snapshot holds only
/// the records this home's running crew sessions name, each read by its key:
/// their convoys, the sessions of their seats, and a standing role's newest
/// launch at each home with its sessions.
pub struct RoleRouting {
    pub keyed: Arc<KeyedReads>,
}

/// The backend a port reads, while it is alive: the backend holds the
/// Message path, which holds these ports, so they hold it weakly.
pub(crate) fn upgrade(backend: &WeakResourceBackend) -> Result<ResourceBackend, String> {
    backend.upgrade().ok_or_else(|| "the resource backend serving messages is gone".to_owned())
}

/// One running crew session at this home and the role addresses it may hold.
struct Candidate {
    terminal: ResourceObject<TerminalSession>,
    session_id: String,
    addresses: Vec<String>,
}

/// The records role routing resolves a home's candidates over, each read once.
#[derive(Default)]
struct Observed {
    convoys: BTreeMap<String, Vec<ReadResourceObject<Convoy>>>,
    /// Sessions by name and home: a session's launches at two homes are two holders.
    terminals: BTreeMap<(String, bool), ReadResourceObject<TerminalSession>>,
}

impl Observed {
    async fn convoy(
        &mut self,
        keyed: &KeyedReads,
        namespace: &str,
        name: &str,
    ) -> Result<Option<(ResourceObject<Convoy>, String)>, String> {
        if !self.convoys.contains_key(name) {
            let copies = keyed.convoy_copies(namespace, name).await.map_err(|error| error.to_string())?;
            self.convoys.insert(name.to_owned(), copies);
        }
        // This home's copy, else the first listed.
        let copies = &self.convoys[name];
        let held = copies.iter().find(|copy| matches!(copy.provenance, ResourceProvenance::Local)).or(copies.first());
        Ok(held.map(|copy| {
            let home = match &copy.provenance {
                ResourceProvenance::Local => keyed.home().to_owned(),
                ResourceProvenance::Replica { origin_root, .. } => origin_root.to_string(),
            };
            (copy.object.clone(), home)
        }))
    }

    fn hold(&mut self, terminal: ReadResourceObject<TerminalSession>) {
        let local = matches!(terminal.provenance, ResourceProvenance::Local);
        self.terminals.insert((terminal.object.metadata.name.clone(), local), terminal);
    }

    /// The sessions holding `role` in work `work` of `convoy`, read by their names.
    async fn seat(&mut self, keyed: &KeyedReads, namespace: &str, convoy: &str, home: &str, work: &str, role: &str) -> Result<(), String> {
        for session in keyed.work_sessions(convoy, home, work, role).await? {
            if let Some(terminal) = keyed.read::<TerminalSession>(namespace, &session).await.map_err(|error| error.to_string())? {
                self.hold(terminal);
            }
        }
        Ok(())
    }
}

#[async_trait]
impl RoleBindings for RoleRouting {
    async fn holders(&self, namespace: &str, roles: &FleetRoles) -> Result<Vec<RoleHolder>, String> {
        let keyed = &self.keyed;
        let backend = keyed.backend()?;
        let mut observed = Observed::default();
        let mut candidates = Vec::new();
        for launch in keyed.running_here().await? {
            let Some(source) = keyed.read::<TerminalSession>(namespace, launch.session_name()).await.map_err(|error| error.to_string())?
            else {
                continue;
            };
            if !matches!(source.provenance, ResourceProvenance::Local) {
                continue;
            }
            let terminal = source.object.clone();
            observed.hold(source);
            let TerminalSessionSource::Agent { context, .. } = &terminal.spec.source else { continue };
            let Some(status) = terminal.status.as_ref().filter(|status| status.phase == TerminalSessionPhase::Running) else { continue };
            let Some(session_id) = status.session_id.clone() else { continue };
            let held = observed.convoy(keyed, namespace, &context.convoy).await?;
            let project = held.as_ref().and_then(|(convoy, _)| convoy.spec.project_ref.clone()).unwrap_or_else(|| namespace.to_owned());
            let labels = &terminal.metadata.labels;
            let mut addresses = Vec::new();
            if let (Some(convoy), Some(vessel), Some(role)) = (labels.get(CONVOY_LABEL), labels.get(VESSEL_LABEL), labels.get(ROLE_LABEL)) {
                addresses.push(format!("{project}/{convoy}/{vessel}/{role}"));
                if let Some((_, home)) = observed.convoy(keyed, namespace, convoy).await? {
                    observed.seat(keyed, namespace, convoy, &home, vessel, role).await?;
                }
                // A standing role is held by its declared standing convoy's crew.
                if self.standing(&mut observed, namespace, convoy, &project, role).await? {
                    addresses.push(format!("{project}/{role}"));
                }
            }
            addresses.extend(terminal.metadata.annotations.get(ROLE_ADDRESS_ANNOTATION).cloned());
            candidates.push(Candidate { terminal, session_id, addresses });
        }
        let snapshot = flotilla_messaging::MessageReceiverSnapshot::load_from_observations(
            &backend,
            namespace,
            observed.convoys.into_values().flatten().collect(),
            observed.terminals.into_values().collect(),
        )
        .await
        .map_err(|error| error.to_string())?;
        let mut holders = Vec::new();
        for Candidate { terminal, session_id, addresses } in candidates {
            let mut inboxes = BTreeSet::new();
            for address in addresses {
                match snapshot.resolve(&address) {
                    Ok(Some(resolved)) if resolved.object.metadata.name == terminal.metadata.name => match roles.inbox(&address) {
                        Ok(inbox) => {
                            inboxes.insert(inbox);
                        }
                        Err(error) => tracing::debug!(%address, %error, "role holds no engine inbox"),
                    },
                    Ok(_) => {}
                    Err(error) => tracing::debug!(%address, %error, "role routing did not resolve"),
                }
            }
            if !inboxes.is_empty() {
                holders.push(RoleHolder { terminal, session_id, inboxes });
            }
        }
        Ok(holders)
    }
}

impl RoleRouting {
    /// Whether `convoy` is a generation of a declared standing role `role` of `project`. If it
    /// is, the role's newest launch at each home and their crews' sessions are read into
    /// `observed`, for routing to judge which generation holds it.
    async fn standing(&self, observed: &mut Observed, namespace: &str, convoy: &str, project: &str, role: &str) -> Result<bool, String> {
        let keyed = &self.keyed;
        let Some((held, _)) = observed.convoy(keyed, namespace, convoy).await? else { return Ok(false) };
        let Some(ensure) = held.metadata.annotations.get(ENSURED_FROM_ANNOTATION) else { return Ok(false) };
        let declared = keyed.record::<ConvoyEnsure>(namespace, ensure).await.map_err(|error| error.to_string())?;
        if !declared.is_some_and(|ensure| ensure.spec.project_ref == project && ensure.spec.role == role) {
            return Ok(false);
        }
        for launch in keyed.newest_launches(flotilla_resources::convoy::launch_of(convoy), project).await? {
            let Some((generation, home)) = observed.convoy(keyed, namespace, &launch).await? else { continue };
            let seats = generation.status.iter().flat_map(|status| status.workflow_snapshot.iter()).flat_map(|snapshot| {
                snapshot.vessels.iter().flat_map(|vessel| vessel.crew.iter().map(|crew| (vessel.name.clone(), crew.role.clone())))
            });
            for (work, crew) in seats.collect::<Vec<_>>() {
                observed.seat(keyed, namespace, &launch, &home, &work, &crew).await?;
            }
        }
        Ok(true)
    }
}

/// Condition values through the Leaf evaluation the old store's gates make, with a role in a
/// convoy read from its convoy's work, by key.
pub struct RecordConditions {
    pub keyed: Arc<KeyedReads>,
    /// The change-request and issue observers' freshness windows, which composition installs.
    pub staleness: Arc<std::sync::OnceLock<(std::time::Duration, std::time::Duration)>>,
    pub clock: Arc<dyn flotilla_resources::Clock>,
}

/// The freshness windows of a path no composition installed them on: the old inbox's default.
pub const UNINSTALLED_STALENESS: std::time::Duration = std::time::Duration::from_secs(300);

impl RecordConditions {
    /// The convoy record and the work holding role `address` in it: from the convoy's snapshot,
    /// else from the labels of a session holding the role.
    async fn work(&self, namespace: &str, address: &Address) -> Result<(String, String), String> {
        let convoy = address.get(Kind::Convoy).ok_or_else(|| format!("`{address}` names no convoy"))?;
        let role = address.name();
        let held = self.keyed.convoy(namespace, convoy).await.map_err(|error| error.to_string())?;
        if let Some(work) = held.and_then(|held| snapshot_work(&held.object, role)) {
            return Ok((convoy.to_owned(), work));
        }
        let sessions = self.keyed.labelled_sessions(namespace, convoy, role).await.map_err(|error| error.to_string())?;
        sessions
            .iter()
            .find_map(|session| session.metadata.labels.get(VESSEL_LABEL).cloned())
            .map(|work| (convoy.to_owned(), work))
            .ok_or_else(|| format!("no vessel holds `{role}` in convoy `{convoy}`"))
    }
}

#[async_trait]
impl ConditionReads for RecordConditions {
    async fn value(&self, namespace: &str, condition: &Condition) -> Result<ThreeValue, String> {
        let backend = self.keyed.backend()?;
        let address = &condition.address;
        let work = match address.kind() {
            Kind::Role if address.get(Kind::Convoy).is_some() => Some(self.work(namespace, address).await),
            _ => None,
        };
        let leaf = condition_leaf(condition, |convoy, role| work.unwrap_or_else(|| Err(format!("`{role}` in `{convoy}` was not read"))))?;
        let (change_request_stale_after, issue_stale_after) =
            self.staleness.get().copied().unwrap_or((UNINSTALLED_STALENESS, UNINSTALLED_STALENESS));
        flotilla_messaging::ConditionRecords {
            backend,
            namespace: namespace.to_owned(),
            change_request_stale_after,
            issue_stale_after,
            clock: Arc::clone(&self.clock),
        }
        .value(&leaf)
        .await
        .map_err(|error| error.to_string())
    }
}

/// A routed point read at another home (#3463): when this home's replica
/// misses a message, the read goes to the home that holds it and comes back
/// marked remote, with its age.
#[async_trait]
pub trait HomeReads: Send + Sync {
    /// `name` of `kind` at the first reachable home other than this one that holds it.
    async fn find(&self, kind: flotilla_schema::KindCode, name: &str)
        -> Result<Option<flotilla_schema_sqlite::stream::ReadResult>, String>;
}
