//! What a crew's agent is doing, as a reader at this home sees it (#3108): the AgentSession its
//! home records, and whether this home has heard from that home recently enough to trust it.
//!
//! An AgentSession changes only when its activity does, so the age of a reading says nothing
//! about whether it is current. Freshness is per home instead: a reading from this home is
//! always current, and one replicated from another home is current while that home's change
//! stream has been heard from within [`HOME_HEARD_WITHIN`]. Whether the session itself is still
//! observed is its writer's to say: it ends the session Lost when the process goes.

use chrono::{DateTime, Utc};
use flotilla_protocol::{AgentActivity, AgentSessionReading, CrewAttention};

use crate::{ScreenObstruction, TerminalSessionPhase, TerminalSessionStatus};

/// The cause an activity read from the agent's screen records. A hook records its native event
/// name instead.
pub const SCREEN_CAUSE: &str = "screen";

/// How long a screen read of an idle agent stands before it counts as quiet: a redraw can show
/// idle for a moment mid-turn.
pub const SCREEN_DEBOUNCE: chrono::Duration = chrono::Duration::seconds(5);

/// How long after this home last heard from another home its readings stay current. Peer
/// change streams send a keepalive at their idle cadence; this leaves room for a delayed
/// round or two before readings from a silent home show as unobservable.
pub const HOME_HEARD_WITHIN: chrono::Duration = chrono::Duration::seconds(120);

/// Whether readings from a home this home last heard from at `heard` are current at `now`.
/// Never heard from is not current.
pub fn home_current(heard: Option<DateTime<Utc>>, now: DateTime<Utc>) -> bool {
    heard.is_some_and(|heard| now.signed_duration_since(heard) < HOME_HEARD_WITHIN)
}

/// When readings from a home last heard from at `heard` stop being current.
pub fn home_stale_at(heard: DateTime<Utc>) -> DateTime<Utc> {
    heard + HOME_HEARD_WITHIN
}

/// An AgentSession as this home reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentActivityView {
    pub reading: AgentSessionReading,
    /// Whether its home has been heard from recently enough to trust the reading; always true
    /// for this home's own sessions.
    pub current: bool,
    /// What this home's daemon has seen of the session beside its activity; `None` for another
    /// home's session, whose progress only its own daemon sees.
    pub progress: Option<CrewProgress>,
}

/// When a crew's agent was last seen making progress that its AgentSession does not record
/// (#3108): the harness's latest tool call, the terminal's latest new output and the latest
/// prompt submitted to it, and when its hooks last reported at all. Only this home's daemon acts on them, in its stall check and its
/// delivery confirmation, so they are held in memory beside the adapter that saw them and traced
/// on each change, never stored or replicated: a restart starts each one afresh.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CrewProgress {
    /// When this daemon began watching: nothing before it was seen, so no quiet is counted from
    /// earlier than it.
    pub watched_from: DateTime<Utc>,
    pub tool_at: Option<DateTime<Utc>>,
    pub output_at: Option<DateTime<Utc>>,
    pub prompt_submit_at: Option<DateTime<Utc>>,
    /// The latest hook of any kind: evidence the hooks are wired, not progress.
    pub hook_at: Option<DateTime<Utc>>,
}

impl CrewProgress {
    /// The latest progress seen, or when watching began if none has been. A hook alone is not
    /// progress.
    pub fn latest(&self) -> DateTime<Utc> {
        [self.tool_at, self.output_at, self.prompt_submit_at].into_iter().flatten().fold(self.watched_from, std::cmp::max)
    }
}

/// This home's crews' [`CrewProgress`], by the name of the TerminalSession record the crew runs
/// in. Each time only moves forward; each move is traced.
pub struct ProgressFacts {
    watched_from: DateTime<Utc>,
    sessions: std::sync::Mutex<std::collections::HashMap<String, Held>>,
    listeners: std::sync::Mutex<Vec<std::sync::Weak<ProgressListener>>>,
}

/// Called with a session's name when a tool call or a prompt submit is first seen for it: what
/// can confirm a turn held for that session.
pub type ProgressListener = dyn Fn(&str) + Send + Sync;

impl std::fmt::Debug for ProgressFacts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProgressFacts").field("watched_from", &self.watched_from).finish_non_exhaustive()
    }
}

#[derive(Debug)]
struct Held {
    progress: CrewProgress,
    /// The output last seen, so the same output again is not new.
    output_digest: Option<String>,
}

impl ProgressFacts {
    /// Starts watching at `now`, having seen nothing.
    pub fn new(now: DateTime<Utc>) -> Self {
        Self { watched_from: now, sessions: Default::default(), listeners: Default::default() }
    }

    /// Calls `listener` at each tool call or prompt submit seen from here on, while it lives.
    pub fn on_turn_evidence(&self, listener: std::sync::Weak<ProgressListener>) {
        let mut listeners = self.listeners.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        listeners.retain(|listener| listener.strong_count() > 0);
        listeners.push(listener);
    }

    fn turn_evidence(&self, session: &str) {
        let listeners: Vec<_> =
            self.listeners.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).iter().filter_map(std::sync::Weak::upgrade).collect();
        for listener in listeners {
            listener(session);
        }
    }

    /// The harness reported a tool call at `at`.
    pub fn tool(&self, session: &str, at: DateTime<Utc>) {
        if self.record(session, "tool", at, |progress| &mut progress.tool_at) {
            self.turn_evidence(session);
        }
    }

    /// The terminal showed output fingerprinted `digest` at `at`: progress when it differs from
    /// the output last seen. Returns whether it did.
    pub fn output(&self, session: &str, digest: &str, at: DateTime<Utc>) -> bool {
        let new = {
            let mut sessions = self.sessions.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            let held = sessions.entry(session.to_owned()).or_insert_with(|| self.empty());
            let new = held.output_digest.as_deref() != Some(digest);
            held.output_digest = Some(digest.to_owned());
            new
        };
        if new {
            self.record(session, "output", at, |progress| &mut progress.output_at);
        }
        new
    }

    /// A prompt was submitted to the agent at `at`.
    pub fn prompt_submit(&self, session: &str, at: DateTime<Utc>) {
        if self.record(session, "prompt-submit", at, |progress| &mut progress.prompt_submit_at) {
            self.turn_evidence(session);
        }
    }

    /// The harness's hook reported at `at`.
    pub fn hook(&self, session: &str, at: DateTime<Utc>) {
        self.record(session, "hook", at, |progress| &mut progress.hook_at);
    }

    /// The session's progress so far; empty when none has been seen since this daemon started.
    pub fn get(&self, session: &str) -> CrewProgress {
        self.sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(session)
            .map_or_else(|| self.empty().progress, |held| held.progress)
    }

    fn empty(&self) -> Held {
        Held {
            progress: CrewProgress {
                watched_from: self.watched_from,
                tool_at: None,
                output_at: None,
                prompt_submit_at: None,
                hook_at: None,
            },
            output_digest: None,
        }
    }

    /// Forgets an ended session's progress.
    pub fn forget(&self, session: &str) {
        self.sessions.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).remove(session);
    }

    /// Whether `at` moved the time forward.
    fn record(
        &self,
        session: &str,
        cause: &'static str,
        at: DateTime<Utc>,
        field: fn(&mut CrewProgress) -> &mut Option<DateTime<Utc>>,
    ) -> bool {
        let mut sessions = self.sessions.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let empty = self.empty();
        let held = field(&mut sessions.entry(session.to_owned()).or_insert(empty).progress);
        let moved = held.is_none_or(|held| at > held);
        if moved {
            *held = Some(at);
            tracing::info!(session, cause, %at, "crew progress");
        }
        moved
    }
}

impl AgentActivityView {
    /// The activity a view can act on: none once the session has ended or its home has gone
    /// quiet.
    pub fn live_activity(&self) -> Option<AgentActivity> {
        (self.current && self.reading.ended.is_none()).then_some(self.reading.activity).flatten()
    }

    /// When the turn under way began: Working's since, while the agent is working.
    pub fn working_since(&self) -> Option<DateTime<Utc>> {
        (self.live_activity() == Some(AgentActivity::Working)).then_some(self.reading.activity_since).flatten()
    }

    /// Since when the agent has been quiet: Idle's since, a screen read's taken [`SCREEN_DEBOUNCE`]
    /// later, while the agent is idle.
    pub fn quiet_since(&self) -> Option<DateTime<Utc>> {
        if !matches!(self.live_activity(), Some(AgentActivity::Idle | AgentActivity::Interrupted)) {
            return None;
        }
        let since = self.reading.activity_since?;
        Some(if self.from_screen() { since + SCREEN_DEBOUNCE } else { since })
    }

    /// Whether the agent's own hooks have been heard since `started`: an activity a hook
    /// reported since then, or, as this daemon saw, any hook or tool call since then.
    pub fn hook_seen_since(&self, started: DateTime<Utc>) -> bool {
        (!self.from_screen() && self.reading.activity.is_some() && self.reading.activity_since.is_some_and(|since| since >= started))
            || self.progress.is_some_and(|progress| {
                [progress.hook_at, progress.tool_at, progress.prompt_submit_at].into_iter().flatten().any(|at| at >= started)
            })
    }

    /// Whether the agent's hooks report it finished a turn after `submitted`, for a harness whose
    /// only hook is its turn's end (Codex notify): it is Idle, as a hook reported since then, or
    /// this daemon saw a hook since then. A screen read of Idle that came first leaves the record
    /// Idle and refuses the hook's same activity, so the hook's time is held in memory too.
    /// A screen read of Idle alone is not a finished turn.
    pub fn completed_after(&self, submitted: DateTime<Utc>) -> bool {
        self.live_activity() == Some(AgentActivity::Idle)
            && ((!self.from_screen() && self.reading.activity_since.is_some_and(|since| since > submitted))
                || self.progress.and_then(|progress| progress.hook_at).is_some_and(|at| at > submitted))
    }

    /// Whether the agent took up a turn submitted at `submitted`: it began working after it,
    /// or this daemon saw a prompt submitted or a tool called after it.
    pub fn took_up_after(&self, submitted: DateTime<Utc>) -> bool {
        self.working_since().is_some_and(|since| since > submitted)
            || self.progress.is_some_and(|progress| {
                progress.prompt_submit_at.is_some_and(|at| at > submitted) || progress.tool_at.is_some_and(|at| at > submitted)
            })
    }

    /// Whether the recorded activity was read from the agent's screen rather than reported by it.
    pub fn from_screen(&self) -> bool {
        self.reading.activity_cause.as_deref() == Some(SCREEN_CAUSE)
    }

    /// The reading as a row shows it, marked stale when its home has gone quiet.
    pub fn row_reading(&self) -> AgentSessionReading {
        AgentSessionReading { stale: !self.current, ..self.reading.clone() }
    }
}

/// Keyed reads of the AgentSessions the decided store holds: this home's, and the replicas of
/// every home it follows.
pub trait AgentSessionReads: Send + Sync {
    /// The AgentSession named `name` (its crew's launch name) homed at `origin` (a
    /// replica's origin root; `None` for this home), or `None` when none is held.
    fn read(&self, name: &str, origin: Option<&str>) -> Result<Option<AgentActivityView>, String>;
}

/// Live monitoring state for a crew session: delivery holds first, then what the terminal
/// shows that needs an operator, then what the agent's AgentSession says it is doing. A session
/// that is not running, or has no AgentSession yet, has none.
pub fn crew_attention(status: Option<&TerminalSessionStatus>, agent: Option<&AgentActivityView>) -> Option<CrewAttention> {
    let status = status.filter(|status| status.phase == TerminalSessionPhase::Running)?;
    if status.delivery_unconfirmed_since.is_some() {
        return Some(CrewAttention::DeliveryUnconfirmed);
    }
    match status.idle_obstruction() {
        Some(ScreenObstruction::ComposerText { .. }) => return Some(CrewAttention::IdleComposerText),
        Some(ScreenObstruction::ScrolledBack) => return Some(CrewAttention::IdleScrolledBack),
        Some(ScreenObstruction::HookReview) | None => {}
    }
    let agent = agent?;
    if agent.reading.ended.is_some() {
        return None;
    }
    if !agent.current {
        return Some(CrewAttention::Unobservable);
    }
    Some(match agent.reading.activity? {
        AgentActivity::Working => CrewAttention::Working,
        AgentActivity::NeedsInput => CrewAttention::NeedsInput,
        AgentActivity::Idle | AgentActivity::Interrupted => CrewAttention::Idle,
    })
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use flotilla_protocol::{AgentActivity, AgentSessionEnd, AgentSessionReading, CrewAttention};

    use super::{crew_attention, home_current, AgentActivityView, HOME_HEARD_WITHIN};
    use crate::{
        ScreenObstruction, TerminalAttention, TerminalAttentionSource, TerminalAttentionState, TerminalSessionPhase, TerminalSessionStatus,
    };

    fn agent(activity: AgentActivity, current: bool) -> AgentActivityView {
        AgentActivityView {
            reading: AgentSessionReading {
                ended: None,
                activity: Some(activity),
                activity_since: Some(Utc::now()),
                activity_cause: Some("Stop".into()),
                stale: false,
            },
            current,
            progress: None,
        }
    }

    fn running() -> TerminalSessionStatus {
        TerminalSessionStatus { phase: TerminalSessionPhase::Running, ..Default::default() }
    }

    // The age of a reading is not its freshness: an hour-old Working is current while its home
    // is heard from, and any reading from a home gone quiet is unobservable.
    #[test]
    fn freshness_is_the_homes_not_the_readings() {
        let now = Utc::now();
        let mut old = agent(AgentActivity::Working, true);
        old.reading.activity_since = Some(now - chrono::Duration::hours(1));
        assert_eq!(crew_attention(Some(&running()), Some(&old)), Some(CrewAttention::Working));
        assert_eq!(crew_attention(Some(&running()), Some(&agent(AgentActivity::Working, false))), Some(CrewAttention::Unobservable));
        assert!(home_current(Some(now - HOME_HEARD_WITHIN + chrono::Duration::seconds(1)), now));
        assert!(!home_current(Some(now - HOME_HEARD_WITHIN), now));
        assert!(!home_current(None, now));
    }

    #[test]
    fn activity_maps_to_crew_attention() {
        for (activity, attention) in [
            (AgentActivity::Working, CrewAttention::Working),
            (AgentActivity::NeedsInput, CrewAttention::NeedsInput),
            (AgentActivity::Idle, CrewAttention::Idle),
            (AgentActivity::Interrupted, CrewAttention::Idle),
        ] {
            assert_eq!(crew_attention(Some(&running()), Some(&agent(activity, true))), Some(attention), "{activity:?}");
        }
    }

    // Delivery holds and an obstructed screen outrank what the agent reports; a session that is
    // not running, has ended, or has no AgentSession yet shows none.
    #[test]
    fn holds_and_obstructions_come_first_and_absence_is_none() {
        let working = agent(AgentActivity::Working, true);
        let mut status = running();
        status.delivery_unconfirmed_since = Some(Utc::now());
        assert_eq!(crew_attention(Some(&status), Some(&working)), Some(CrewAttention::DeliveryUnconfirmed));

        let mut status = running();
        status.attention = Some(TerminalAttention {
            state: TerminalAttentionState::Unobservable,
            as_of: Utc::now(),
            source: TerminalAttentionSource::Screen,
        });
        status.screen_obstruction = Some(ScreenObstruction::ComposerText { reply: None, chars: 3 });
        assert_eq!(crew_attention(Some(&status), Some(&working)), Some(CrewAttention::IdleComposerText));

        assert_eq!(crew_attention(Some(&running()), None), None);
        let mut ended = working.clone();
        ended.reading.ended = Some(AgentSessionEnd::Lost);
        assert_eq!(crew_attention(Some(&running()), Some(&ended)), None);
        let stopped = TerminalSessionStatus { phase: TerminalSessionPhase::Stopped, ..Default::default() };
        assert_eq!(crew_attention(Some(&stopped), Some(&working)), None);
    }
}
