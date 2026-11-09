//! Terminal sessions as the engine holds them (#3108): the port every terminal session reader and
//! requester uses, served over the decided store.
//!
//! A terminal session is one process in one environment's terminal pool: a crew's agent, held for
//! its vessel, or a tool. Each run of it is a launch of its own (`terminal-<vessel>-<role>#<n>`):
//! a launch that ended (exited, lost, failed) is never revived, and its holder asks for the next
//! launch. A launch's number is ordered within its home only, so a launch name is unique and
//! ordered within its home, and the newest launch of a name is read at a home.
//!
//! A crew's id is its launch name: its AgentSession is named so, and `FLOTILLA_CREW_ID` carries
//! it. Two homes can each hold a launch of the same name and number, so any lookup of a crew or an
//! AgentSession by that name also takes the home (owner's ruling, #3108).
//!
//! What a launch was given when it was realised (its cleat endpoint, its agent harness, its build
//! parallelism) is its [`TerminalLaunchFacts`], fixed for the launch. What its home observes of it
//! while it runs (attention, occupancy, the screen, its crew's path back to the daemon, the
//! command it was launched with) is a local observation: it stays in memory at that home, beside
//! the adapter, and is never replicated ([`TerminalObservations`]). Nothing here is stored in this
//! shape.

use std::{
    collections::{BTreeMap, HashMap},
    sync::Mutex,
};

use chrono::{DateTime, Utc};
use flotilla_protocol::{result_set::CleatEndpoint, LifecycleReading};

pub use crate::clone_records::launched;
use crate::{ScreenObstruction, TerminalAttention, TerminalBrief, TerminalOccupancy};

/// How the reason a launch was lost begins when its process exited, rather than its pool losing
/// it: a crew's exited agent waits for a person to resume it, while a lost one is relaunched.
pub const PROCESS_EXITED: &str = "process exited";

/// What a launch runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TerminalLaunchSource {
    /// A tool's command line.
    Tool { command: String },
    /// An agent harness chosen by capability, started with its brief.
    Agent { capability: String, adapter: Option<String>, model: Option<String>, brief: TerminalBrief },
}

/// The harness an agent launch resolved to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentLaunch {
    pub adapter: String,
    pub model: Option<String>,
    /// The harness's permission stance, as its adapter names it.
    pub stance: String,
}

/// What one launch was given when it was realised, fixed for the launch.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TerminalLaunchFacts {
    /// The cleat daemon holding the session (`runtime_root`, `daemon`), when its pool is cleat.
    pub endpoint: Option<(String, String)>,
    /// The harness an agent's launch resolved to.
    pub agent: Option<AgentLaunch>,
    /// The Rust build and linker parallelism a host-direct launch was given.
    pub build_jobs: Option<u64>,
}

/// One launch of a terminal session as a reader sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalRecord {
    /// The launch: `<session>#<n>`.
    pub name: String,
    /// The home that holds it: the host of its environment.
    pub home: String,
    /// The environment it runs in, and that environment's home.
    pub environment: String,
    pub environment_home: String,
    /// The vessel it is held for, for a crew's session; none for a standalone tool.
    pub vessel: Option<String>,
    pub role: String,
    pub pool: String,
    pub cwd: String,
    /// Declared, non-secret launch values.
    pub env: BTreeMap<String, String>,
    pub source: TerminalLaunchSource,
    /// When this launch was requested.
    pub requested_at: DateTime<Utc>,
    /// When it entered its current state: realised, while running; ended, once it has.
    pub since: DateTime<Utc>,
    pub reading: LifecycleReading,
    /// What it was given when it was realised; none before.
    pub facts: Option<TerminalLaunchFacts>,
}

impl TerminalRecord {
    /// The session this launch is of: its name without the launch number.
    pub fn session_name(&self) -> &str {
        launched(&self.name)
    }

    /// Its crew's id: the launch name, for an agent's launch. Unique only within its home.
    pub fn crew_id(&self) -> Option<&str> {
        matches!(self.source, TerminalLaunchSource::Agent { .. }).then_some(self.name.as_str())
    }

    /// The pool's session id it runs under, once realised.
    pub fn session_id(&self) -> Option<&str> {
        match &self.reading {
            LifecycleReading::Ready { identity } => Some(identity),
            _ => None,
        }
    }

    pub fn is_running(&self) -> bool {
        matches!(self.reading, LifecycleReading::Ready { .. })
    }

    pub fn is_agent(&self) -> bool {
        matches!(self.source, TerminalLaunchSource::Agent { .. })
    }

    /// Whether it has ended: exited, lost, failed or given up.
    pub fn ended(&self) -> bool {
        matches!(
            self.reading,
            LifecycleReading::Released | LifecycleReading::Lost { .. } | LifecycleReading::Failed { .. } | LifecycleReading::GaveUp { .. }
        )
    }

    /// When it started running: its Realise, while it runs.
    pub fn started_at(&self) -> Option<DateTime<Utc>> {
        self.is_running().then_some(self.since)
    }

    /// When it ended, once it has.
    pub fn ended_at(&self) -> Option<DateTime<Utc>> {
        self.ended().then_some(self.since)
    }

    /// Whether it ended because its process exited ([`PROCESS_EXITED`]).
    pub fn exited(&self) -> bool {
        matches!(&self.reading, LifecycleReading::Lost { reason: Some(reason) } if reason.starts_with(PROCESS_EXITED))
    }

    /// The cleat endpoint a direct attach connects to, while it runs in a cleat pool.
    pub fn cleat_endpoint(&self) -> Option<CleatEndpoint> {
        let session = self.session_id()?;
        let (runtime_root, daemon) = self.facts.as_ref()?.endpoint.clone()?;
        Some(CleatEndpoint { runtime_root, daemon, session: session.to_owned() })
    }

    /// The harness its launch resolved to.
    pub fn agent(&self) -> Option<&AgentLaunch> {
        self.facts.as_ref()?.agent.as_ref()
    }

    /// Its launch number, ordered within its home.
    pub fn launch_number(&self) -> u64 {
        self.name.rsplit_once('#').and_then(|(_, n)| n.parse().ok()).unwrap_or(0)
    }
}

/// The read view's annotation naming the launch it shows: one per launch, so a reader that holds
/// it can tell a session's next launch from this one.
pub const LAUNCH_ANNOTATION: &str = "flotilla.work/launch";

/// The launch read view `session` shows, which also names its crew's AgentSession; none for a
/// view that was not read from a launch.
pub fn launch_of(session: &crate::ResourceObject<crate::TerminalSession>) -> Option<&str> {
    session.metadata.annotations.get(LAUNCH_ANNOTATION).map(String::as_str)
}

/// What a launch's holder Vessel says about it, joined in for readers of the old shape.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TerminalHolderView {
    pub convoy: String,
    /// The vessel's name within its convoy (its requirement key).
    pub vessel_name: String,
    /// The actuator annotations the vessel carries.
    pub annotations: BTreeMap<String, String>,
}

impl TerminalRecord {
    /// This launch in the old TerminalSession shape, for readers not yet converted to records: a
    /// read view only, never written. It is named as its session, its resource version is the
    /// launch, and its holder's convoy and labels come from `holder`; what its home observes of
    /// it, from `observation`, which only its home has.
    pub fn read_view(
        &self,
        namespace: &str,
        holder: Option<&TerminalHolderView>,
        observation: Option<&TerminalObservation>,
    ) -> crate::ResourceObject<crate::TerminalSession> {
        use crate::{
            CrewSessionStatus, ObjectMeta, OwnerReference, Resource, TerminalCrewContext, TerminalSessionPhase, TerminalSessionSource,
            TerminalSessionSpec, TerminalSessionStatus, Vessel, CONVOY_LABEL, ROLE_LABEL, VESSEL_LABEL, VESSEL_REF_LABEL,
        };
        let phase = match &self.reading {
            LifecycleReading::Ready { .. } => TerminalSessionPhase::Running,
            LifecycleReading::InProgress | LifecycleReading::Retrying { .. } | LifecycleReading::Exhausted { .. } => {
                TerminalSessionPhase::Starting
            }
            LifecycleReading::Released => TerminalSessionPhase::Stopped,
            LifecycleReading::Lost { .. } if self.exited() => TerminalSessionPhase::Stopped,
            LifecycleReading::Lost { .. } => TerminalSessionPhase::Lost,
            LifecycleReading::Failed { .. } | LifecycleReading::GaveUp { .. } => TerminalSessionPhase::Failed,
        };
        let mut labels = BTreeMap::from([(ROLE_LABEL.to_owned(), self.role.clone())]);
        let mut annotations = BTreeMap::from([(LAUNCH_ANNOTATION.to_owned(), self.name.clone())]);
        let mut owner_references = Vec::new();
        if let Some(vessel) = &self.vessel {
            labels.insert(VESSEL_REF_LABEL.to_owned(), vessel.clone());
            owner_references.push(OwnerReference {
                api_version: format!("{}/{}", Vessel::API_PATHS.group, Vessel::API_PATHS.version),
                kind: Vessel::API_PATHS.kind.to_owned(),
                name: vessel.clone(),
                controller: true,
            });
        }
        if let Some(holder) = holder {
            labels.insert(CONVOY_LABEL.to_owned(), holder.convoy.clone());
            labels.insert(VESSEL_LABEL.to_owned(), holder.vessel_name.clone());
            annotations.extend(holder.annotations.clone());
        }
        let source = match &self.source {
            TerminalLaunchSource::Tool { command } => TerminalSessionSource::Tool { command: command.clone() },
            TerminalLaunchSource::Agent { capability, adapter, model, brief } => TerminalSessionSource::Agent {
                selector: crate::Selector { capability: capability.clone(), adapter: adapter.clone(), model: model.clone() },
                brief: brief.clone(),
                context: Box::new(TerminalCrewContext {
                    namespace: namespace.to_owned(),
                    convoy: holder.map(|holder| holder.convoy.clone()).unwrap_or_default(),
                    vessel_ref: self.vessel.clone().unwrap_or_default(),
                }),
                message: None,
            },
        };
        let observation = observation.cloned().unwrap_or_default();
        let facts = self.facts.clone().unwrap_or_default();
        crate::ResourceObject {
            metadata: ObjectMeta {
                name: self.session_name().to_owned(),
                namespace: namespace.to_owned(),
                resource_version: self.name.clone(),
                labels,
                annotations,
                owner_references,
                finalizers: Vec::new(),
                deletion_timestamp: None,
                creation_timestamp: self.requested_at,
                merge: None,
            },
            spec: TerminalSessionSpec {
                env_ref: self.environment.clone(),
                role: self.role.clone(),
                source,
                cwd: self.cwd.clone(),
                pool: self.pool.clone(),
                env: self.env.clone(),
            },
            status: Some(TerminalSessionStatus {
                codex_native_hooks: observation.codex_native_hooks,
                configured_limits: facts.build_jobs.map(|jobs| flotilla_protocol::ConfiguredResourceLimits {
                    build_jobs: Some(jobs as usize),
                    linker_threads: Some(jobs as usize),
                    ..Default::default()
                }),
                phase,
                session_id: self.session_id().map(str::to_owned),
                cleat_endpoint: self.cleat_endpoint(),
                started_at: self.started_at(),
                stopped_at: self.ended_at(),
                message: self.reading.reason().filter(|_| !self.is_running()),
                crew: self.agent().map(|agent| CrewSessionStatus {
                    id: self.name.clone(),
                    adapter: agent.adapter.clone(),
                    model: agent.model.clone(),
                    stance: agent.stance.clone(),
                }),
                launch_command: observation.launch_command,
                attention: observation.attention,
                screen_obstruction: observation.screen_obstruction,
                occupancy: observation.occupancy,
                daemon_unreachable: observation.daemon_unreachable,
                ..Default::default()
            }),
        }
    }
}

/// Each session's newest launch among `records`, by home and session name: a later launch of a
/// session at one home replaces an earlier one. Launches at different homes are never compared.
pub fn newest_launches(records: impl IntoIterator<Item = TerminalRecord>) -> BTreeMap<(String, String), TerminalRecord> {
    let mut newest = BTreeMap::<(String, String), TerminalRecord>::new();
    for record in records {
        let key = (record.home.clone(), record.session_name().to_owned());
        match newest.get(&key) {
            Some(held) if held.launch_number() >= record.launch_number() => {}
            _ => {
                newest.insert(key, record);
            }
        }
    }
    newest
}

/// What a requester asks a launch to run, at this home.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalLaunchRequest {
    /// The session it is a launch of (`terminal-<vessel>-<role>`).
    pub session: String,
    /// The environment it runs in, at this home.
    pub environment: String,
    pub vessel: Option<String>,
    pub role: String,
    pub pool: String,
    pub cwd: String,
    pub env: BTreeMap<String, String>,
    pub source: TerminalLaunchSource,
}

/// What changed among the terminal sessions, at any home.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TerminalChange {
    /// The launch of this name at this home changed.
    Changed { name: String, home: String },
    /// Any of them may have changed: a home's log was read afresh.
    Rescanned,
}

/// What asking for a session's next launch did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TerminalRenewal {
    /// Its newest launch had ended; this launch was requested after it.
    Started(String),
    /// Its newest launch has not ended.
    Live(Box<TerminalRecord>),
}

/// The terminal session port, served by the engine's [`crate::lifecycle_reads::LifecycleReads`].
/// Each read is keyed or indexed. [`TerminalRecords::request`] and [`TerminalRecords::renew`]
/// are the only writes, and only at this home: the TerminalSession controller writes every edge
/// after them, including the release once the holder has ended.
pub trait TerminalRecords: Send + Sync {
    /// This home.
    fn home(&self) -> &str;
    /// `name` held at `home`, or at this home when `None`. A launch name (`<session>#<n>`) reads
    /// that launch; a session's name reads its newest launch there.
    fn get(&self, name: &str, home: Option<&str>) -> Result<Option<TerminalRecord>, String>;
    /// Every launch held at `home`, ended or not.
    fn at(&self, home: &str) -> Result<Vec<TerminalRecord>, String>;
    /// Every launch at every home this store holds.
    fn every(&self) -> Result<Vec<TerminalRecord>, String>;
    /// Every launch held for `vessel`, across homes.
    fn of_vessel(&self, vessel: &str) -> Result<Vec<TerminalRecord>, String>;
    /// Every launch that runs in environment `environment` at `home`.
    fn of_environment(&self, environment: &str, home: &str) -> Result<Vec<TerminalRecord>, String>;
    /// The launch that serves `request` at this home: the session's newest launch when that has
    /// not ended, else its next launch, requested now.
    fn request(&self, request: TerminalLaunchRequest) -> Result<String, String>;
    /// Starts session `session`'s next launch at this home, with its newest launch's request, if
    /// that launch has ended.
    fn renew(&self, session: &str) -> Result<TerminalRenewal, String>;
    /// The sessions' changes from now on, as their records are written or replicated here. Where
    /// none are followed, the channel is closed.
    fn changes(&self) -> tokio::sync::broadcast::Receiver<TerminalChange> {
        tokio::sync::broadcast::channel(1).1
    }
    /// What this home observes of its own running launches, in memory.
    fn observations(&self) -> &TerminalObservations;
}

/// What this home observes of one of its running launches: local observations, never replicated
/// (owner's ruling, #3108). Each change is traced where it is observed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TerminalObservation {
    /// What the harness appears to be doing, from its hooks or its screen.
    pub attention: Option<TerminalAttention>,
    /// Why the last screen read an otherwise idle agent as unable to take a turn (#3460).
    pub screen_obstruction: Option<ScreenObstruction>,
    /// Whether a person occupies the terminal's controller seat.
    pub occupancy: TerminalOccupancy,
    /// Why its crew cannot reach this daemon, when it cannot (#3618).
    pub daemon_unreachable: Option<String>,
    /// The command it was launched with: a passthrough attach runs it.
    pub launch_command: Option<String>,
    /// Whether its Codex harness reports native lifecycle hooks.
    pub codex_native_hooks: bool,
}

impl TerminalObservation {
    /// Whether `incoming` adds to what is held: fresh hook readings take precedence over the
    /// screen, identical readings are rate-limited, and a Codex harness without native hooks has
    /// every newer completion counted (its notify hook reports only turn completion). `adapter`
    /// is the harness the launch resolved to.
    pub fn should_record_attention(&self, incoming: &TerminalAttention, adapter: Option<&str>) -> bool {
        use crate::{TerminalAttentionSource, TerminalAttentionState};
        self.attention.as_ref().is_none_or(|previous| {
            previous.should_replace_with(incoming)
                || (!self.codex_native_hooks
                    && adapter == Some("codex")
                    && incoming.source == TerminalAttentionSource::Hook
                    && incoming.state == TerminalAttentionState::Idle
                    && incoming.as_of > previous.as_of)
        })
    }

    /// Records a hook's or screen's `attention` if it adds to what is held.
    pub fn observe_attention(&mut self, attention: TerminalAttention, adapter: Option<&str>) {
        if self.should_record_attention(&attention, adapter) {
            self.attention = Some(attention);
        }
    }

    /// Records one screen-and-seat reading: `occupancy`, `attention` if it adds to what is held,
    /// and the screen's obstruction when the screen was read (`Some`).
    pub fn observe(
        &mut self,
        attention: Option<TerminalAttention>,
        obstruction: Option<Option<ScreenObstruction>>,
        occupancy: TerminalOccupancy,
        adapter: Option<&str>,
    ) {
        self.occupancy = occupancy;
        if let Some(obstruction) = obstruction {
            self.screen_obstruction = obstruction;
        }
        if let Some(attention) = attention {
            self.observe_attention(attention, adapter);
        }
    }
}

/// This home's observations of its launches, by launch name, held in memory. A change wakes the
/// readers that subscribed.
#[derive(Debug)]
pub struct TerminalObservations {
    held: Mutex<HashMap<String, TerminalObservation>>,
    changes: tokio::sync::broadcast::Sender<String>,
}

impl Default for TerminalObservations {
    fn default() -> Self {
        Self { held: Mutex::new(HashMap::new()), changes: tokio::sync::broadcast::channel(256).0 }
    }
}

impl TerminalObservations {
    /// What is observed of launch `name`, if anything.
    pub fn get(&self, name: &str) -> Option<TerminalObservation> {
        self.lock().get(name).cloned()
    }

    /// Changes launch `name`'s observation with `change`; a change that alters it wakes the
    /// subscribers. Whether it changed.
    pub fn update(&self, name: &str, change: impl FnOnce(&mut TerminalObservation)) -> bool {
        let changed = {
            let mut held = self.lock();
            let observation = held.entry(name.to_owned()).or_default();
            let before = observation.clone();
            change(observation);
            *observation != before
        };
        if changed {
            let _ = self.changes.send(name.to_owned());
        }
        changed
    }

    /// Forgets launch `name`, which has ended.
    pub fn forget(&self, name: &str) {
        if self.lock().remove(name).is_some() {
            let _ = self.changes.send(name.to_owned());
        }
    }

    /// The launches whose observation changed from now on.
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<String> {
        self.changes.subscribe()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, TerminalObservation>> {
        self.held.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(name: &str, home: &str) -> TerminalRecord {
        TerminalRecord {
            name: name.into(),
            home: home.into(),
            environment: "env".into(),
            environment_home: home.into(),
            vessel: None,
            role: "tool".into(),
            pool: "cleat".into(),
            cwd: "/".into(),
            env: BTreeMap::new(),
            source: TerminalLaunchSource::Tool { command: "sh".into() },
            requested_at: DateTime::UNIX_EPOCH,
            since: DateTime::UNIX_EPOCH,
            reading: LifecycleReading::InProgress,
            facts: None,
        }
    }

    #[test]
    fn the_newest_launch_is_judged_within_its_home() {
        let newest = newest_launches([record("t#9", "a"), record("t#12", "a"), record("t#3", "b")]);
        assert_eq!(newest[&("a".into(), "t".into())].name, "t#12");
        assert_eq!(newest[&("b".into(), "t".into())].name, "t#3");
    }

    #[test]
    fn an_unchanged_observation_wakes_nobody() {
        let observations = TerminalObservations::default();
        let mut changes = observations.subscribe();
        assert!(observations.update("t#1", |observation| observation.launch_command = Some("sh".into())));
        assert!(!observations.update("t#1", |observation| observation.launch_command = Some("sh".into())));
        assert_eq!(changes.try_recv().ok().as_deref(), Some("t#1"));
        assert!(changes.try_recv().is_err());
    }

    // Native lifecycle hooks mark the launch that reported them; the session's next launch starts
    // without them, and an ended launch's observations are forgotten with it.
    #[test]
    fn native_codex_hooks_hold_until_the_next_launch() {
        use crate::{TerminalAttention, TerminalAttentionSource, TerminalAttentionState};
        let observations = TerminalObservations::default();
        let working = TerminalAttention {
            state: TerminalAttentionState::Working,
            as_of: DateTime::UNIX_EPOCH,
            source: TerminalAttentionSource::Hook,
        };
        observations.update("t#1", |observation| {
            observation.codex_native_hooks |= true;
            observation.observe_attention(working.clone(), Some("codex"));
        });
        let held = observations.get("t#1").expect("observed");
        assert!(held.codex_native_hooks);
        assert_eq!(held.attention, Some(working));
        assert!(!observations.get("t#2").unwrap_or_default().codex_native_hooks);
        observations.forget("t#1");
        assert_eq!(observations.get("t#1"), None);
    }
}
