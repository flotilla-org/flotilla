//! AgentSessions as a reader at one home sees them, held in memory for tests of their readers
//! (#3108): each records only a change of activity, as the AgentSession machine does, and this
//! home's sessions carry what its daemon has seen of their progress.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use chrono::{DateTime, Utc};
use flotilla_protocol::{AgentActivity, AgentSessionEnd, AgentSessionReading};
use flotilla_resources::agent_activity::{
    home_current, home_stale_at, AgentActivityView, AgentSessionChange, AgentSessionReads, ProgressFacts,
};
use flotilla_resources::{Clock, SystemClock};
use futures::StreamExt;
use tokio::sync::broadcast;

/// This home's AgentSessions and its crews' progress.
pub struct AgentSessionsFixture {
    sessions: Mutex<HashMap<String, AgentSessionReading>>,
    /// What this home's daemon has seen of its sessions' progress.
    pub progress: Arc<ProgressFacts>,
    changes: broadcast::Sender<AgentSessionChange>,
    /// When each other home whose silence a test plays was last heard from.
    heard: Mutex<HashMap<String, DateTime<Utc>>>,
    /// The clock those homes' readings are judged current by.
    clock: Mutex<Arc<dyn Clock>>,
    /// How many times each session was read.
    reads: Mutex<HashMap<String, usize>>,
}

impl AgentSessionsFixture {
    /// How many times session `name` has been read.
    pub fn reads(&self, name: &str) -> usize {
        self.reads.lock().expect("reads").get(name).copied().unwrap_or_default()
    }

    /// No sessions, watching progress from `watched_from`.
    pub fn new(watched_from: DateTime<Utc>) -> Self {
        Self::holding(HashMap::new(), watched_from)
    }

    fn holding(sessions: HashMap<String, AgentSessionReading>, watched_from: DateTime<Utc>) -> Self {
        Self {
            sessions: Mutex::new(sessions),
            progress: Arc::new(ProgressFacts::new(watched_from)),
            changes: broadcast::channel(64).0,
            heard: Mutex::default(),
            clock: Mutex::new(Arc::new(SystemClock)),
            reads: Mutex::default(),
        }
    }

    /// Home `home` was heard from at `at`, its readings judged current by `clock` from now on:
    /// they stop being current once it has been silent too long, until it is heard from again.
    pub fn heard(&self, home: &str, at: DateTime<Utc>, clock: Arc<dyn Clock>) {
        self.heard.lock().expect("heard homes").insert(home.to_owned(), at);
        *self.clock.lock().expect("clock") = clock;
        let _ = self.changes.send(AgentSessionChange::Heard { origin: home.to_owned() });
    }

    fn changed(&self, name: &str) {
        let _ = self.changes.send(AgentSessionChange::Changed { name: name.to_owned(), origin: None });
    }

    /// The agent in session `name` reported `activity` at `at`, through `cause` (a harness's
    /// native event, or [`flotilla_resources::agent_activity::SCREEN_CAUSE`]). The same
    /// activity again leaves the record as it was.
    pub fn observe(&self, name: &str, activity: AgentActivity, cause: &str, at: DateTime<Utc>) {
        let mut sessions = self.sessions.lock().expect("agent sessions");
        let reading = sessions.entry(name.to_owned()).or_insert_with(|| AgentSessionReading {
            ended: None,
            activity: None,
            activity_since: None,
            activity_cause: None,
            stale: false,
        });
        if reading.activity != Some(activity) {
            reading.activity = Some(activity);
            reading.activity_since = Some(at);
            reading.activity_cause = Some(cause.to_owned());
            drop(sessions);
            self.changed(name);
        }
    }

    /// Session `name` ended.
    pub fn end(&self, name: &str, ended: AgentSessionEnd) {
        if let Some(reading) = self.sessions.lock().expect("agent sessions").get_mut(name) {
            reading.ended = Some(ended);
        }
        self.changed(name);
    }

    /// The same AgentSessions read by a daemon started at `watched_from`: the records survive, what
    /// the previous daemon saw of their progress does not.
    pub fn restarted(&self, watched_from: DateTime<Utc>) -> Self {
        Self::holding(self.sessions.lock().expect("agent sessions").clone(), watched_from)
    }

    /// Forgets session `name`, as a store that never recorded it.
    pub fn forget(&self, name: &str) {
        self.sessions.lock().expect("agent sessions").remove(name);
        self.changed(name);
    }
}

impl AgentSessionReads for AgentSessionsFixture {
    /// Every home is heard from, but one [`Self::heard`] names, whose readings are current until
    /// it falls silent; only this home's sessions carry progress.
    fn read(&self, name: &str, origin: Option<&str>) -> Result<Option<AgentActivityView>, String> {
        *self.reads.lock().expect("reads").entry(name.to_owned()).or_default() += 1;
        let reading = self.sessions.lock().expect("agent sessions").get(name).cloned();
        // Progress is kept by session, as the daemon keeps it.
        let progress = origin.is_none().then(|| self.progress.get(flotilla_resources::terminal_records::launched(name)));
        let heard = origin.and_then(|origin| self.heard.lock().expect("heard homes").get(origin).copied());
        let (current, stale_at) = match heard {
            Some(at) => {
                let current = home_current(Some(at), self.clock.lock().expect("clock").now());
                (current, current.then(|| home_stale_at(at)))
            }
            None => (true, None),
        };
        Ok(reading.map(|reading| AgentActivityView { reading, current, stale_at, progress }))
    }

    fn changes(&self) -> Result<futures::stream::BoxStream<'static, AgentSessionChange>, String> {
        Ok(futures::stream::unfold(self.changes.subscribe(), |mut changes| async move {
            match changes.recv().await {
                Ok(change) => Some((change, changes)),
                Err(broadcast::error::RecvError::Lagged(_)) => Some((AgentSessionChange::Rescanned, changes)),
                Err(broadcast::error::RecvError::Closed) => None,
            }
        })
        .boxed())
    }
}
