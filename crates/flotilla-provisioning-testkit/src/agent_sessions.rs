//! AgentSessions as a reader at one home sees them, held in memory for tests of their readers
//! (#3108): each records only a change of activity, as the AgentSession machine does, and this
//! home's sessions carry what its daemon has seen of their progress.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use chrono::{DateTime, Utc};
use flotilla_protocol::{AgentActivity, AgentSessionEnd, AgentSessionReading};
use flotilla_resources::agent_activity::{AgentActivityView, AgentSessionChange, AgentSessionReads, ProgressFacts};
use futures::StreamExt;
use tokio::sync::broadcast;

/// This home's AgentSessions and its crews' progress.
pub struct AgentSessionsFixture {
    sessions: Mutex<HashMap<String, AgentSessionReading>>,
    /// What this home's daemon has seen of its sessions' progress.
    pub progress: Arc<ProgressFacts>,
    changes: broadcast::Sender<AgentSessionChange>,
}

impl AgentSessionsFixture {
    /// No sessions, watching progress from `watched_from`.
    pub fn new(watched_from: DateTime<Utc>) -> Self {
        Self::holding(HashMap::new(), watched_from)
    }

    fn holding(sessions: HashMap<String, AgentSessionReading>, watched_from: DateTime<Utc>) -> Self {
        Self { sessions: Mutex::new(sessions), progress: Arc::new(ProgressFacts::new(watched_from)), changes: broadcast::channel(64).0 }
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
    /// Every home is heard from; only this home's sessions carry progress.
    fn read(&self, name: &str, origin: Option<&str>) -> Result<Option<AgentActivityView>, String> {
        let reading = self.sessions.lock().expect("agent sessions").get(name).cloned();
        // Progress is kept by session, as the daemon keeps it.
        let progress = origin.is_none().then(|| self.progress.get(flotilla_resources::terminal_records::launched(name)));
        Ok(reading.map(|reading| AgentActivityView { reading, current: true, stale_at: None, progress }))
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
