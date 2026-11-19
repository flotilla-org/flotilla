//! Which crew session at this home holds which role inboxes, kept as the records role routing
//! reads change (d12, d13).
//!
//! Role routing ([`RoleBindings::route`]) decides a session's holding from its own convoy's
//! records: the convoy, the sessions and vessels of its seats, and a standing role's generations,
//! which are launches of the same convoy. So a change to one of those records reroutes only the
//! sessions here of that convoy, and a pass with nothing changed routes nothing. Watches of the
//! records report each change ([`RoutingChanges::touched`], through [`RoutingInput`]); the view
//! takes them at the next refresh.
//!
//! What every session's holding may turn on reroutes every session: the fleet roles, a standing
//! declaration, and an adopted claim with the project declarations that validate it. So does a
//! watch opened again, which may have missed changes, and a watch that stopped: while it stays
//! stopped, each refresh routes every session, as no change can be followed for its kind.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex, PoisonError},
};

use flotilla_resources::{
    Convoy, ConvoyEnsure, CrewDefaults, FleetDesignation, Project, ReadWatchEvent, Resource, ResourceProvenance, TerminalSession, Vessel,
    CONVOY_LABEL, ROLE_ADDRESS_ANNOTATION,
};
use tracing::{info_span, Instrument};

use crate::{
    ports::{RoleBindings, RoleHolder},
    roles::FleetRoles,
};

/// What a change to a record role routing reads touches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Touched {
    /// Nothing routing reads.
    Nothing,
    /// The records of convoy `0`'s launches: its sessions here are routed again.
    Convoy(String),
    /// Session `name`, crew of `convoy` when its view names one, held `here` or at another home,
    /// and whether it claims an adopted role.
    Session { name: String, convoy: Option<String>, here: bool, claims: bool },
    /// The declarations that validate adopted claims: every session is routed again while any
    /// session here claims a role.
    Claims,
    /// What any session's holding may turn on.
    Everything,
}

/// A kind whose records role routing reads, and what a change to one touches.
pub trait RoutingInput: Resource + Sized {
    fn touched(event: &ReadWatchEvent<Self>) -> Touched;
}

/// The object an event carries, if it carries one.
fn object<R: Resource>(event: &ReadWatchEvent<R>) -> Option<&flotilla_resources::ReadResourceObject<R>> {
    match event {
        ReadWatchEvent::Added(read) | ReadWatchEvent::Modified(read) | ReadWatchEvent::Deleted(read) => Some(read),
        ReadWatchEvent::DeletedByName { .. } => None,
    }
}

/// The name an event is about.
fn name<R: Resource>(event: &ReadWatchEvent<R>) -> &str {
    match event {
        ReadWatchEvent::Added(read) | ReadWatchEvent::Modified(read) | ReadWatchEvent::Deleted(read) => &read.object.metadata.name,
        ReadWatchEvent::DeletedByName { tombstone, .. } => &tombstone.name,
    }
}

/// The convoy a record of convoy `name`'s launches is grouped under: its launches share their
/// sessions' routing, since a standing role's generations are launches of one convoy.
fn group(convoy: &str) -> String {
    flotilla_resources::convoy::launch_of(convoy).to_owned()
}

impl RoutingInput for TerminalSession {
    fn touched(event: &ReadWatchEvent<Self>) -> Touched {
        let (here, convoy, claims) = match (object(event), event) {
            (Some(read), _) => (
                matches!(read.provenance, ResourceProvenance::Local),
                read.object.metadata.labels.get(CONVOY_LABEL).cloned(),
                read.object.metadata.annotations.contains_key(ROLE_ADDRESS_ANNOTATION),
            ),
            (None, ReadWatchEvent::DeletedByName { tombstone, provenance }) => {
                (matches!(provenance, ResourceProvenance::Local), None, tombstone.annotations.contains_key(ROLE_ADDRESS_ANNOTATION))
            }
            (None, _) => return Touched::Everything,
        };
        // Another home's session crew of no convoy seats no role here, unless it claims one.
        if !here && convoy.is_none() && !claims && object(event).is_some() {
            return Touched::Nothing;
        }
        Touched::Session { name: name(event).to_owned(), convoy, here, claims }
    }
}

impl RoutingInput for Convoy {
    fn touched(event: &ReadWatchEvent<Self>) -> Touched {
        Touched::Convoy(name(event).to_owned())
    }
}

impl RoutingInput for Vessel {
    fn touched(event: &ReadWatchEvent<Self>) -> Touched {
        // A vessel's sessions seat its convoy's roles; one deleted by name names no convoy.
        object(event).map_or(Touched::Everything, |read| Touched::Convoy(read.object.spec.convoy_ref.clone()))
    }
}

impl RoutingInput for ConvoyEnsure {
    fn touched(_: &ReadWatchEvent<Self>) -> Touched {
        Touched::Everything
    }
}

impl RoutingInput for FleetDesignation {
    fn touched(_: &ReadWatchEvent<Self>) -> Touched {
        // The fleet roles are compared at each refresh; the fleet's project roots adopted claims'
        // project hierarchy.
        Touched::Claims
    }
}

impl RoutingInput for Project {
    fn touched(_: &ReadWatchEvent<Self>) -> Touched {
        Touched::Claims
    }
}

impl RoutingInput for CrewDefaults {
    fn touched(_: &ReadWatchEvent<Self>) -> Touched {
        Touched::Claims
    }
}

/// The changes not yet taken.
#[derive(Debug, Default)]
struct Dirty {
    all: bool,
    claims: bool,
    /// Convoys whose sessions here are routed again, by their group.
    convoys: BTreeSet<String>,
    /// Sessions here routed again, each with its convoy's group when the change named one.
    sessions: BTreeMap<String, Option<String>>,
}

impl Dirty {
    fn merge(&mut self, other: Dirty) {
        self.all |= other.all;
        self.claims |= other.claims;
        self.convoys.extend(other.convoys);
        for (session, convoy) in other.sessions {
            let held = self.sessions.entry(session).or_default();
            if convoy.is_some() {
                *held = convoy;
            }
        }
    }
}

/// The changes the watches report, shared with them; each wakes the reader.
#[derive(Debug)]
pub struct RoutingChanges {
    dirty: Mutex<Dirty>,
    /// The watches that stopped and were not opened again.
    unwatched: std::sync::atomic::AtomicUsize,
    wake: Arc<tokio::sync::Notify>,
}

impl RoutingChanges {
    /// Changes waking `wake`; every session is routed at the first refresh.
    pub fn new(wake: Arc<tokio::sync::Notify>) -> Arc<Self> {
        Arc::new(Self { dirty: Mutex::new(Dirty { all: true, ..Dirty::default() }), unwatched: Default::default(), wake })
    }

    fn dirty(&self) -> std::sync::MutexGuard<'_, Dirty> {
        self.dirty.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Records what a change touched, and wakes the reader when it touched anything.
    pub fn touched(&self, touched: Touched) {
        {
            let mut dirty = self.dirty();
            match touched {
                Touched::Nothing => return,
                Touched::Convoy(convoy) => {
                    dirty.convoys.insert(group(&convoy));
                }
                Touched::Session { name, convoy, here, claims } => {
                    let convoy = convoy.map(|convoy| group(&convoy));
                    if claims {
                        dirty.all = true;
                    }
                    if let Some(convoy) = &convoy {
                        dirty.convoys.insert(convoy.clone());
                    }
                    if here {
                        dirty.sessions.insert(name, convoy);
                    } else if convoy.is_none() {
                        // Another home's session deleted by name: whichever convoy it seated.
                        dirty.all = true;
                    }
                }
                Touched::Claims => dirty.claims = true,
                Touched::Everything => dirty.all = true,
            }
        }
        self.wake.notify_one();
    }

    /// A watch opened, again or for the first time: it may have missed changes.
    pub fn reopened(&self) {
        self.touched(Touched::Everything);
    }

    /// A watch stopped for good: every refresh routes every session from now on.
    pub fn stopped(&self) {
        self.unwatched.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.touched(Touched::Everything);
    }

    fn take(&self) -> Dirty {
        let mut taken = std::mem::take(&mut *self.dirty());
        if self.unwatched.load(std::sync::atomic::Ordering::SeqCst) > 0 {
            taken.all = true;
        }
        taken
    }

    /// Puts back changes a failed refresh took.
    fn restore(&self, taken: Dirty) {
        self.dirty().merge(taken);
    }
}

/// What one refresh did.
#[derive(Debug, Default)]
pub struct Refreshed {
    /// The sessions whose holding changed, by name.
    pub changed: BTreeSet<String>,
    /// Whether it routed any session.
    pub routed: bool,
}

/// This home's role holders, kept from the changes to what routing reads.
pub struct HeldRoles {
    changes: Arc<RoutingChanges>,
    namespace: String,
    roles: Option<FleetRoles>,
    /// The holders, by session name.
    holders: BTreeMap<String, RoleHolder>,
    /// Each convoy group's running crew sessions here, holders or not.
    members: BTreeMap<String, BTreeSet<String>>,
    /// Each such session's convoy group.
    member_of: BTreeMap<String, String>,
    /// The sessions here that claim an adopted role: routed with every convoy, since a claim
    /// shadows a standing role's generation.
    claimants: BTreeSet<String>,
}

impl HeldRoles {
    pub fn new(namespace: &str, changes: Arc<RoutingChanges>) -> Self {
        Self {
            changes,
            namespace: namespace.to_owned(),
            roles: None,
            holders: BTreeMap::new(),
            members: BTreeMap::new(),
            member_of: BTreeMap::new(),
            claimants: BTreeSet::new(),
        }
    }

    pub fn changes(&self) -> &Arc<RoutingChanges> {
        &self.changes
    }

    /// The holders, by session name.
    pub fn holders(&self) -> &BTreeMap<String, RoleHolder> {
        &self.holders
    }

    /// Routes again the sessions the changes since the last refresh touched, under `roles`.
    /// Changes a failed refresh took are kept for the next.
    pub async fn refresh(&mut self, bindings: &dyn RoleBindings, roles: &FleetRoles) -> Result<Refreshed, String> {
        let mut taken = self.changes.take();
        if self.roles.as_ref() != Some(roles) {
            taken.all = true;
        }
        if taken.claims && !self.claimants.is_empty() {
            taken.all = true;
        }
        if taken.all {
            return match bindings.route(&self.namespace, roles, None).instrument(info_span!("held_roles.route_all")).await {
                Ok(routed) => {
                    self.roles = Some(roles.clone());
                    let before = std::mem::take(&mut self.holders);
                    self.members.clear();
                    self.member_of.clear();
                    self.claimants.clear();
                    Ok(self.apply(before, routed))
                }
                Err(error) => {
                    self.changes.restore(taken);
                    Err(error)
                }
            };
        }
        let mut sessions = BTreeSet::new();
        for (session, convoy) in &taken.sessions {
            sessions.insert(session.clone());
            // Its former convoy, whose other sessions may hold what it held.
            if let Some(former) = self.member_of.get(session) {
                sessions.extend(self.members.get(former).into_iter().flatten().cloned());
            }
            if let Some(convoy) = convoy {
                sessions.extend(self.members.get(convoy).into_iter().flatten().cloned());
            }
        }
        for convoy in &taken.convoys {
            sessions.extend(self.members.get(convoy).into_iter().flatten().cloned());
        }
        if sessions.is_empty() {
            return Ok(Refreshed::default());
        }
        sessions.extend(self.claimants.iter().cloned());
        let routed = match bindings
            .route(&self.namespace, roles, Some(&sessions))
            .instrument(info_span!("held_roles.route", sessions = sessions.len()))
            .await
        {
            Ok(routed) => routed,
            Err(error) => {
                self.changes.restore(taken);
                return Err(error);
            }
        };
        let mut before = BTreeMap::new();
        for session in &sessions {
            if let Some(holder) = self.holders.remove(session) {
                before.insert(session.clone(), holder);
            }
            if let Some(convoy) = self.member_of.remove(session) {
                if let Some(members) = self.members.get_mut(&convoy) {
                    members.remove(session);
                    if members.is_empty() {
                        self.members.remove(&convoy);
                    }
                }
            }
            self.claimants.remove(session);
        }
        Ok(self.apply(before, routed))
    }

    /// Takes `routed` in for the sessions it routed, whose holders were `before`: the names whose
    /// holding changed.
    fn apply(&mut self, mut before: BTreeMap<String, RoleHolder>, routed: crate::ports::Routed) -> Refreshed {
        for candidate in routed.candidates {
            let convoy = group(&candidate.convoy);
            self.members.entry(convoy.clone()).or_default().insert(candidate.session.clone());
            self.member_of.insert(candidate.session.clone(), convoy);
            if candidate.claims {
                self.claimants.insert(candidate.session);
            }
        }
        let mut changed = BTreeSet::new();
        for holder in routed.holders {
            let name = holder.terminal.metadata.name.clone();
            let same = before.remove(&name).is_some_and(|held| {
                held.session_id == holder.session_id
                    && held.inboxes == holder.inboxes
                    && held.terminal.metadata.resource_version == holder.terminal.metadata.resource_version
            });
            if !same {
                changed.insert(name.clone());
            }
            self.holders.insert(name, holder);
        }
        changed.extend(before.into_keys());
        Refreshed { changed, routed: true }
    }

    /// Forgets session `name`'s holding and routes it again at the next refresh: its reader could
    /// not take the holding up.
    pub fn retry(&mut self, name: &str) {
        self.holders.remove(name);
        let convoy = self.member_of.get(name).cloned();
        self.changes.dirty().sessions.insert(name.to_owned(), convoy);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use async_trait::async_trait;

    use super::*;
    use crate::ports::{Routed, RoutedSession};

    /// Routing over fixed running sessions, by name: their convoy and whether they claim a role.
    /// It records which sessions each route asked for (`None` for every one).
    #[derive(Default)]
    struct Sessions {
        running: Mutex<BTreeMap<String, (String, bool)>>,
        routes: Mutex<Vec<Option<BTreeSet<String>>>>,
    }

    impl Sessions {
        fn run(&self, session: &str, convoy: &str, claims: bool) {
            self.running.lock().expect("running").insert(session.to_owned(), (convoy.to_owned(), claims));
        }

        fn routes(&self) -> Vec<Option<BTreeSet<String>>> {
            std::mem::take(&mut *self.routes.lock().expect("routes"))
        }
    }

    #[async_trait]
    impl RoleBindings for Sessions {
        async fn route(&self, _: &str, _: &FleetRoles, sessions: Option<&BTreeSet<String>>) -> Result<Routed, String> {
            self.routes.lock().expect("routes").push(sessions.cloned());
            let running = self.running.lock().expect("running");
            let candidates = running
                .iter()
                .filter(|(name, _)| sessions.is_none_or(|sessions| sessions.contains(*name)))
                .map(|(name, (convoy, claims))| RoutedSession { session: name.clone(), convoy: convoy.clone(), claims: *claims })
                .collect();
            Ok(Routed { holders: Vec::new(), candidates })
        }
    }

    fn names(names: &[&str]) -> Option<BTreeSet<String>> {
        Some(names.iter().map(|name| (*name).to_owned()).collect())
    }

    fn session(name: &str, convoy: &str) -> Touched {
        Touched::Session { name: name.into(), convoy: Some(convoy.into()), here: true, claims: false }
    }

    // A refresh routes only the sessions of the convoys a change touched, a standing role's
    // generations together, and nothing when nothing changed; what any holding may turn on
    // routes every session.
    #[tokio::test]
    async fn a_refresh_routes_only_what_changes_touched() {
        let bindings = Sessions::default();
        bindings.run("a-coder", "alpha#1", false);
        bindings.run("a-reviewer", "alpha#1", false);
        bindings.run("b-coder", "beta#1", false);
        let roles = FleetRoles::declared("lab").expect("roles");
        let changes = RoutingChanges::new(Arc::new(tokio::sync::Notify::new()));
        let mut held = HeldRoles::new("flotilla", Arc::clone(&changes));

        held.refresh(&bindings, &roles).await.expect("refresh");
        assert_eq!(bindings.routes(), vec![None], "the first refresh routes every session");
        held.refresh(&bindings, &roles).await.expect("refresh");
        assert_eq!(bindings.routes(), Vec::<Option<BTreeSet<String>>>::new(), "nothing changed, nothing routed");

        // A standing role's next generation is a launch of the same convoy.
        changes.touched(Touched::Convoy("alpha#2".into()));
        held.refresh(&bindings, &roles).await.expect("refresh");
        assert_eq!(bindings.routes(), vec![names(&["a-coder", "a-reviewer"])]);

        // Another home's session of a convoy reroutes that convoy's sessions here.
        changes.touched(Touched::Session { name: "b-elsewhere".into(), convoy: Some("beta#1".into()), here: false, claims: false });
        held.refresh(&bindings, &roles).await.expect("refresh");
        assert_eq!(bindings.routes(), vec![names(&["b-coder"])]);

        // A session starting here joins its convoy's.
        bindings.run("b-reviewer", "beta#1", false);
        changes.touched(session("b-reviewer", "beta#1"));
        held.refresh(&bindings, &roles).await.expect("refresh");
        assert_eq!(bindings.routes(), vec![names(&["b-coder", "b-reviewer"])]);

        // A project declaration changes no holding while no session claims an adopted role.
        changes.touched(Touched::Claims);
        held.refresh(&bindings, &roles).await.expect("refresh");
        assert_eq!(bindings.routes(), Vec::<Option<BTreeSet<String>>>::new());

        // A claim reroutes every session; the claimant is routed with every convoy after it.
        bindings.run("adopted", "", true);
        changes.touched(Touched::Session { name: "adopted".into(), convoy: None, here: true, claims: true });
        held.refresh(&bindings, &roles).await.expect("refresh");
        assert_eq!(bindings.routes(), vec![None]);
        changes.touched(Touched::Convoy("beta#1".into()));
        held.refresh(&bindings, &roles).await.expect("refresh");
        assert_eq!(bindings.routes(), vec![names(&["adopted", "b-coder", "b-reviewer"])]);
        changes.touched(Touched::Claims);
        held.refresh(&bindings, &roles).await.expect("refresh");
        assert_eq!(bindings.routes(), vec![None], "with a claimant, a declaration change reroutes every session");

        // Other fleet roles reroute every session.
        let other = FleetRoles::declared("other").expect("roles");
        held.refresh(&bindings, &other).await.expect("refresh");
        assert_eq!(bindings.routes(), vec![None]);
    }

    // A watch that stopped leaves no change to follow for its kind: every refresh routes every
    // session.
    #[tokio::test]
    async fn a_stopped_watch_routes_every_session_each_refresh() {
        let bindings = Sessions::default();
        bindings.run("a-coder", "alpha#1", false);
        let roles = FleetRoles::declared("lab").expect("roles");
        let changes = RoutingChanges::new(Arc::new(tokio::sync::Notify::new()));
        let mut held = HeldRoles::new("flotilla", Arc::clone(&changes));
        held.refresh(&bindings, &roles).await.expect("refresh");
        changes.stopped();
        held.refresh(&bindings, &roles).await.expect("refresh");
        held.refresh(&bindings, &roles).await.expect("refresh");
        assert_eq!(bindings.routes(), vec![None, None, None]);
    }
}
