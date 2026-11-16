//! Terminal sessions on the engine (#3108): the request a launch makes, the record a stored launch
//! shows its readers, and the [`TerminalRecords`] port over the decided store.
//!
//! Each launch of a session is a resource of its own (`terminal-<vessel>-<role>#<n>`). A request
//! while the newest launch has not ended is served by that launch; a request, or a renewal, once
//! it has ended starts the next launch. The TerminalSession controller writes every edge after the
//! request, and the launch's facts in the transaction that realises it.

use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

use chrono::{DateTime, Utc};
use flotilla_resources::{
    terminal_records::{
        launched, AgentLaunch, TerminalChange, TerminalLaunchFacts as FactsView, TerminalLaunchRequest, TerminalLaunchSource,
        TerminalObservations, TerminalRecord, TerminalRecords, TerminalRenewal,
    },
    TerminalSession as OldTerminalSession, TerminalSessionSource,
};
use flotilla_schema::{Frozen, Kind, Ref, Timestamp};
use flotilla_schema_sqlite::{stream::Service, Read, StoreHandle};
use flotilla_state_machines::Resource;

use super::{
    reads::{indexed, keyed, latest, view, ReadError, StoreReads, View},
    EngineError, Lifecycles,
};
use crate::stored::{
    terminal_facts::TerminalLaunchFacts,
    terminal_session::{Launch, TerminalSession, TerminalSessionKind, TerminalSessionRequest},
    Holder,
};

/// The request `request` makes at `home`.
pub fn request_of(request: &TerminalLaunchRequest, home: &str) -> Result<TerminalSessionRequest, String> {
    Ok(TerminalSessionRequest {
        launch_of: request.session.clone(),
        launch: 0,
        environment: Ref::new(request.environment.clone(), home),
        holder: request.vessel.as_deref().map(Holder::vessel),
        role: request.role.clone(),
        pool: request.pool.clone(),
        cwd: request.cwd.clone(),
        env: request.env.clone(),
        source: match &request.source {
            TerminalLaunchSource::Tool { command } => Launch::Tool { command: command.clone() },
            TerminalLaunchSource::Agent { capability, adapter, model, brief } => Launch::Agent {
                capability: capability.clone(),
                adapter: adapter.clone(),
                model: model.clone(),
                brief: Frozen::new(brief.clone()).map_err(|error| format!("freeze the brief: {error}"))?,
            },
        },
    })
}

/// The request old TerminalSession `obj` made at `home`, for its import: a crew session is held
/// for its vessel, a tool for no one.
pub fn request_of_old(obj: &flotilla_resources::ResourceObject<OldTerminalSession>, home: &str) -> Result<TerminalSessionRequest, String> {
    let spec = &obj.spec;
    let (source, holder) = match &spec.source {
        TerminalSessionSource::Tool { command } => (Launch::Tool { command: command.clone() }, None),
        TerminalSessionSource::Agent { selector, brief, context, .. } => {
            let brief = brief.clone();
            (
                Launch::Agent {
                    capability: selector.capability.clone(),
                    adapter: selector.adapter.clone(),
                    model: selector.model.clone(),
                    brief: Frozen::new(brief).map_err(|error| format!("freeze the brief: {error}"))?,
                },
                Some(Holder::vessel(&context.vessel_ref)),
            )
        }
    };
    Ok(TerminalSessionRequest {
        launch_of: obj.metadata.name.clone(),
        launch: 0,
        environment: Ref::new(spec.env_ref.clone(), home),
        holder,
        role: spec.role.clone(),
        pool: spec.pool.clone(),
        cwd: spec.cwd.clone(),
        env: spec.env.clone(),
        source,
    })
}

/// The vessel `holder` names, for a crew's session.
pub fn holder_vessel(holder: &Holder) -> Option<&str> {
    holder.0.strip_prefix("vessel:")
}

/// The launch request `request` shows its readers.
pub fn launch_request_of(request: &TerminalSessionRequest) -> TerminalLaunchRequest {
    TerminalLaunchRequest {
        session: request.launch_of.clone(),
        environment: request.environment.name.clone(),
        vessel: request.holder.as_ref().and_then(holder_vessel).map(str::to_owned),
        role: request.role.clone(),
        pool: request.pool.clone(),
        cwd: request.cwd.clone(),
        env: request.env.clone(),
        source: source_of(&request.source),
    }
}

fn source_of(launch: &Launch) -> TerminalLaunchSource {
    match launch {
        Launch::Tool { command } => TerminalLaunchSource::Tool { command: command.clone() },
        Launch::Agent { capability, adapter, model, brief } => TerminalLaunchSource::Agent {
            capability: capability.clone(),
            adapter: adapter.clone(),
            model: model.clone(),
            brief: brief.value().clone(),
        },
    }
}

/// The facts `facts` show their readers.
pub fn facts_view(facts: &TerminalLaunchFacts) -> FactsView {
    FactsView {
        endpoint: facts.endpoint.as_ref().map(|endpoint| (endpoint.runtime_root.clone(), endpoint.daemon.clone())),
        agent: facts.agent.as_ref().map(|agent| AgentLaunch {
            adapter: agent.adapter.clone(),
            model: agent.model.clone(),
            stance: agent.stance.clone(),
        }),
        build_jobs: facts.build_jobs,
    }
}

/// The stored facts `facts` make.
pub fn stored_facts(facts: &FactsView) -> TerminalLaunchFacts {
    use crate::stored::terminal_facts::{AgentLaunch as StoredAgent, TerminalEndpoint};
    TerminalLaunchFacts {
        endpoint: facts
            .endpoint
            .as_ref()
            .map(|(runtime_root, daemon)| TerminalEndpoint { runtime_root: runtime_root.clone(), daemon: daemon.clone() }),
        agent: facts.agent.as_ref().map(|agent| StoredAgent {
            adapter: agent.adapter.clone(),
            model: agent.model.clone(),
            stance: agent.stance.clone(),
        }),
        build_jobs: facts.build_jobs,
    }
}

fn time(at: Timestamp) -> DateTime<Utc> {
    at.to_datetime().unwrap_or(DateTime::UNIX_EPOCH)
}

/// `view` as a reader sees it, with the facts its launch was realised with.
pub(crate) fn record(read: &mut Read<'_>, view: View<TerminalSessionKind>) -> Result<TerminalRecord, ReadError> {
    let facts = match read.resolve(TerminalLaunchFacts::CODE, &view.name, &view.home)? {
        Some(id) => read.load::<TerminalLaunchFacts>(id)?.map(|loaded| facts_view(&loaded.value)),
        None => None,
    };
    let requested_at = view.created;
    let request = &view.request;
    Ok(TerminalRecord {
        environment: request.environment.name.clone(),
        environment_home: request.environment.home.clone(),
        vessel: request.holder.as_ref().and_then(holder_vessel).map(str::to_owned),
        role: request.role.clone(),
        pool: request.pool.clone(),
        cwd: request.cwd.clone(),
        env: request.env.clone(),
        source: source_of(&request.source),
        requested_at,
        since: time(view.since),
        reading: view.judgement.reading(),
        facts,
        name: view.name,
        home: view.home,
    })
}

/// The [`TerminalRecords`] port over one decided store: this home's sessions through its
/// TerminalSession home, and every home's through keyed, indexed and scanned reads.
pub struct EngineTerminals {
    lifecycles: Arc<dyn Lifecycles<TerminalSessionKind>>,
    reads: StoreReads,
    homes: Arc<dyn Fn() -> Vec<String> + Send + Sync>,
    changes: tokio::sync::broadcast::Sender<TerminalChange>,
    observations: TerminalObservations,
    /// Launches read into records by listing, index and scan reads, the cost of those reads.
    launches_read: AtomicU64,
}

/// How many changes a slow follower may fall behind by; one that lags rereads everything.
const CHANGES: usize = 256;

/// How often a request that loses a race to another requester of the same session reads again.
const REQUEST_ATTEMPTS: usize = 3;

impl EngineTerminals {
    /// The port over `store`, whose homes `homes` names.
    pub fn new(
        lifecycles: Arc<dyn Lifecycles<TerminalSessionKind>>,
        store: Arc<StoreHandle>,
        homes: impl Fn() -> Vec<String> + Send + Sync + 'static,
    ) -> Self {
        Self {
            lifecycles,
            reads: StoreReads::new(store),
            homes: Arc::new(homes),
            changes: tokio::sync::broadcast::channel(CHANGES).0,
            observations: TerminalObservations::default(),
            launches_read: AtomicU64::new(0),
        }
    }

    /// How many launches listing, index and scan reads have read, for read-cost tests.
    pub fn launches_read(&self) -> u64 {
        self.launches_read.load(Ordering::Relaxed)
    }

    /// The port over `service`'s store and homes.
    pub fn over(lifecycles: Arc<dyn Lifecycles<TerminalSessionKind>>, service: Arc<Service>) -> Self {
        let store = service.store();
        Self::new(lifecycles, store, move || service.homes())
    }

    /// Follows the sessions' records and facts in `service`'s log, at every home, telling
    /// [`TerminalRecords::changes`] of each. It runs at the store's commits.
    pub fn follow(&self, service: Arc<Service>) -> Result<tokio::task::JoinHandle<()>, String> {
        let mut watch = super::watch::EngineWatch::start(service, [<Resource<TerminalSession> as Kind>::CODE, TerminalLaunchFacts::CODE])
            .map_err(|error| error.to_string())?;
        let changes = self.changes.clone();
        Ok(tokio::spawn(async move {
            loop {
                let followed = watch.next(|_, followed| Ok(followed.into_iter().map(terminal_change).collect::<Vec<_>>())).await;
                match followed {
                    // No follower is no reason to stop: one may subscribe later.
                    Ok(followed) => followed.into_iter().for_each(|change| drop(changes.send(change))),
                    Err(error) => tracing::warn!(%error, "the terminal sessions' changes could not be read from the log; retrying"),
                }
            }
        }))
    }

    fn now() -> Timestamp {
        Timestamp::from_datetime(&Utc::now())
    }

    fn records(
        &self,
        what: &str,
        views: impl FnOnce(&mut Read<'_>) -> Result<Vec<View<TerminalSessionKind>>, ReadError>,
    ) -> Result<Vec<TerminalRecord>, String> {
        let records: Vec<TerminalRecord> = self
            .reads
            .read(|read| views(read)?.into_iter().map(|view| record(read, view)).collect())
            .map_err(|error| format!("engine terminal sessions {what}: {error}"))?;
        self.launches_read.fetch_add(records.len() as u64, Ordering::Relaxed);
        Ok(records)
    }

    fn scan(read: &mut Read<'_>, home: &str) -> Result<Vec<View<TerminalSessionKind>>, ReadError> {
        let mut views = Vec::new();
        let mut after = None;
        loop {
            let page = read.scan::<Resource<TerminalSession>>(home, after, 64)?;
            for id in page.ids {
                views.extend(view::<TerminalSessionKind>(read, id)?);
            }
            after = page.next;
            if after.is_none() {
                return Ok(views);
            }
        }
    }

    /// One try at the launch that serves `request` for its session.
    fn serve(&self, request: &TerminalSessionRequest) -> Result<String, EngineError> {
        let session = &request.launch_of;
        match self.lifecycles.newest_launch(session) {
            Some(held) if held.spec().environment != request.environment || held.spec().holder != request.holder => {
                Err(EngineError::Failed(format!(
                    "terminal session {session} is held in environment {} for {:?}",
                    held.spec().environment.name,
                    held.spec().holder
                )))
            }
            Some(held) if held.state().live() => Ok(held.name().to_owned()),
            Some(held) => self.lifecycles.replace_launch(session, held.name(), request.clone(), Self::now()),
            None => self.lifecycles.request(session, request.clone(), Self::now()).map(|created| created.name().to_owned()),
        }
    }
}

impl TerminalRecords for EngineTerminals {
    fn codes(&self) -> Vec<flotilla_schema::KindCode> {
        vec![<Resource<TerminalSession> as Kind>::CODE, TerminalLaunchFacts::CODE]
    }

    fn cursor(&self) -> Option<Box<dyn flotilla_resources::lifecycle_reads::ChangeCursor<TerminalChange>>> {
        let cursor = super::log_cursor::LogCursor::start(
            self.reads.clone(),
            Arc::clone(&self.homes),
            [<Resource<TerminalSession> as Kind>::CODE, TerminalLaunchFacts::CODE],
            terminal_change,
        );
        match cursor {
            Ok(cursor) => Some(Box::new(cursor)),
            Err(error) => {
                tracing::warn!(%error, "the log could not be followed from here; readers read afresh");
                None
            }
        }
    }

    fn home(&self) -> &str {
        self.lifecycles.home()
    }

    fn changes(&self) -> tokio::sync::broadcast::Receiver<TerminalChange> {
        self.changes.subscribe()
    }

    fn observations(&self) -> &TerminalObservations {
        &self.observations
    }

    fn get(&self, name: &str, home: Option<&str>) -> Result<Option<TerminalRecord>, String> {
        let home = home.unwrap_or(self.home()).to_owned();
        self.reads
            .read(|read| {
                let view = if launched(name) != name {
                    keyed::<TerminalSessionKind>(read, name, &home)?
                } else {
                    latest::<TerminalSessionKind>(read, name, &home)?
                };
                view.map(|view| record(read, view)).transpose()
            })
            .map_err(|error| format!("engine TerminalSession/{name}: {error}"))
    }

    fn at(&self, home: &str) -> Result<Vec<TerminalRecord>, String> {
        self.records(&format!("at {home}"), |read| Self::scan(read, home))
    }

    fn every(&self) -> Result<Vec<TerminalRecord>, String> {
        let homes = (self.homes)();
        self.records("at every home", |read| {
            let mut views = Vec::new();
            for home in &homes {
                views.extend(Self::scan(read, home)?);
            }
            Ok(views)
        })
    }

    fn of_vessel(&self, vessel: &str) -> Result<Vec<TerminalRecord>, String> {
        self.records(&format!("of vessel {vessel}"), |read| indexed(read, &TerminalSessionRequest::by_holder(&Holder::vessel(vessel))))
    }

    fn of_environment(&self, environment: &str, home: &str) -> Result<Vec<TerminalRecord>, String> {
        self.records(&format!("in environment {environment}"), |read| {
            indexed(read, &TerminalSessionRequest::by_environment(&Ref::new(environment.to_owned(), home)))
        })
    }

    fn request(&self, request: TerminalLaunchRequest) -> Result<String, String> {
        let session = request.session.clone();
        let request = request_of(&request, self.home())?;
        let mut last = None;
        for _ in 0..REQUEST_ATTEMPTS {
            match self.serve(&request) {
                Ok(launch) => return Ok(launch),
                // Another requester launched it first: read again.
                Err(error @ (EngineError::Refused(_) | EngineError::Unknown | EngineError::Raced { .. })) => last = Some(error),
                Err(error) => return Err(format!("request terminal session {session}: {error}")),
            }
        }
        Err(format!("request terminal session {session}: {}", last.map(|error| error.to_string()).unwrap_or_default()))
    }

    fn renew(&self, session: &str) -> Result<TerminalRenewal, String> {
        let held = self.lifecycles.newest_launch(session).ok_or_else(|| format!("terminal session {session} is not held here"))?;
        let newest = held.name().to_owned();
        if held.state().live() {
            let record = self.get(&newest, None)?.ok_or_else(|| format!("terminal session {newest} is not held here"))?;
            return Ok(TerminalRenewal::Live(Box::new(record)));
        }
        self.lifecycles
            .replace_launch(session, &newest, held.spec().clone(), Self::now())
            .map(TerminalRenewal::Started)
            .map_err(|error| format!("renew terminal session {session}: {error}"))
    }
}

/// What a followed write of the kinds tells readers.
fn terminal_change(followed: flotilla_engine_store::follow::Followed) -> TerminalChange {
    match followed {
        flotilla_engine_store::follow::Followed::Changed(identity) => TerminalChange::Changed { name: identity.name, home: identity.home },
        flotilla_engine_store::follow::Followed::Rescan(_) => TerminalChange::Rescanned,
    }
}
