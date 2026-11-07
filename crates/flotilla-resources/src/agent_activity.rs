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
}

impl AgentActivityView {
    /// The activity a view can act on: none once the session has ended or its home has gone
    /// quiet.
    pub fn live_activity(&self) -> Option<AgentActivity> {
        (self.current && self.reading.ended.is_none()).then_some(self.reading.activity).flatten()
    }

    /// The reading as a row shows it, marked stale when its home has gone quiet.
    pub fn row_reading(&self) -> AgentSessionReading {
        AgentSessionReading { stale: !self.current, ..self.reading.clone() }
    }
}

/// Keyed reads of the AgentSessions the decided store holds: this home's, and the replicas of
/// every home it follows.
pub trait AgentSessionReads: Send + Sync {
    /// The AgentSession named `name` (its TerminalSession record's name) homed at `origin` (a
    /// replica's origin root; `None` for this home), or `None` when none is held.
    fn read(&self, name: &str, origin: Option<&str>) -> Result<Option<AgentActivityView>, String>;
}

/// Live monitoring state for a crew session: delivery holds first, then what the terminal
/// shows that needs an operator, then what the agent's AgentSession says it is doing. A session
/// that is not running, or has no AgentSession yet, has none.
pub fn crew_attention(status: Option<&TerminalSessionStatus>, agent: Option<&AgentActivityView>) -> Option<CrewAttention> {
    let status = status.filter(|status| status.phase == TerminalSessionPhase::Running)?;
    if status.degraded.as_ref().is_some_and(|condition| condition.is_controller_fence_unverifiable()) {
        return Some(CrewAttention::ControllerFenceUnverifiable);
    }
    if status.degraded.as_ref().is_some_and(|condition| condition.is_delivery()) {
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
        ScreenObstruction, StatusPatch, TerminalAttention, TerminalAttentionSource, TerminalAttentionState, TerminalSessionPhase,
        TerminalSessionStatus, TerminalSessionStatusPatch,
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
        TerminalSessionStatusPatch::MarkDeliveryUnconfirmed {
            message_id: "turn".into(),
            message: "unsent".into(),
            observed_at: Utc::now(),
        }
        .apply(&mut status);
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
