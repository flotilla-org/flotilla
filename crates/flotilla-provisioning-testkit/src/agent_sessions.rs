//! AgentSessions as a reader at one home sees them, held in memory for tests of their readers
//! (#3108): each records only a change of activity, as the AgentSession machine does, and this
//! home's sessions carry what its daemon has seen of their progress.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use chrono::{DateTime, Utc};
use flotilla_protocol::{AgentActivity, AgentSessionEnd, AgentSessionReading};
use flotilla_resources::agent_activity::{AgentActivityView, AgentSessionReads, ProgressFacts};

/// This home's AgentSessions and its crews' progress.
pub struct AgentSessionsFixture {
    sessions: Mutex<HashMap<String, AgentSessionReading>>,
    /// What this home's daemon has seen of its sessions' progress.
    pub progress: Arc<ProgressFacts>,
}

impl AgentSessionsFixture {
    /// No sessions, watching progress from `watched_from`.
    pub fn new(watched_from: DateTime<Utc>) -> Self {
        Self { sessions: Mutex::default(), progress: Arc::new(ProgressFacts::new(watched_from)) }
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
        }
    }

    /// Session `name` ended.
    pub fn end(&self, name: &str, ended: AgentSessionEnd) {
        if let Some(reading) = self.sessions.lock().expect("agent sessions").get_mut(name) {
            reading.ended = Some(ended);
        }
    }

    /// The same AgentSessions read by a daemon started at `watched_from`: the records survive, what
    /// the previous daemon saw of their progress does not.
    pub fn restarted(&self, watched_from: DateTime<Utc>) -> Self {
        Self {
            sessions: Mutex::new(self.sessions.lock().expect("agent sessions").clone()),
            progress: Arc::new(ProgressFacts::new(watched_from)),
        }
    }

    /// Forgets session `name`, as a store that never recorded it.
    pub fn forget(&self, name: &str) {
        self.sessions.lock().expect("agent sessions").remove(name);
    }
}

impl AgentSessionReads for AgentSessionsFixture {
    /// Every home is heard from; only this home's sessions carry progress.
    fn read(&self, name: &str, origin: Option<&str>) -> Result<Option<AgentActivityView>, String> {
        let reading = self.sessions.lock().expect("agent sessions").get(name).cloned();
        // Progress is kept by session, as the daemon keeps it.
        let progress = origin.is_none().then(|| self.progress.get(flotilla_resources::terminal_records::launched(name)));
        Ok(reading.map(|reading| AgentActivityView { reading, current: true, progress }))
    }
}
