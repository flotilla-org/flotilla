//! State readings and their arbitration (#2951 ruling 5).

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// What a session is doing, in OSC 7501's vocabulary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentState {
    Idle,
    Working,
    Done,
    Blocked { kind: String },
    Error,
    Clear,
}

/// Where a reading came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StateSignal {
    /// The harness's own protocol, such as Codex app-server turn events.
    Native,
    /// cleat's program status record for the session.
    CleatProgramStatus,
    /// A recognizer reading the screen.
    Screen,
    /// A harness hook. Hooks give identity and lifecycle, never activity, so
    /// arbitration ignores hook readings.
    Hook,
}

/// The default arbitration order: native protocol, then cleat program status,
/// then screen inference. A connector may declare another order per operation
/// and version, and configuration may override it.
pub const DEFAULT_STATE_ORDER: [StateSignal; 3] = [StateSignal::Native, StateSignal::CleatProgramStatus, StateSignal::Screen];

/// Whether the harness reported the state itself or it was inferred.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confidence {
    Native,
    Inferred,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateReading {
    pub state: AgentState,
    pub signal: StateSignal,
    pub confidence: Confidence,
    pub as_of: DateTime<Utc>,
}

/// A connector's stream of readings for one live session.
#[async_trait]
pub trait StateStream: Send {
    /// The next reading, or `None` once the session's stream has ended.
    async fn next(&mut self) -> Option<StateReading>;
}

/// The reading to believe: the newest reading from the first signal in
/// `order` that has one. Hook readings never decide activity, even when an
/// order names them.
pub fn arbitrate<'a>(order: &[StateSignal], readings: &'a [StateReading]) -> Option<&'a StateReading> {
    order
        .iter()
        .filter(|signal| **signal != StateSignal::Hook)
        .find_map(|signal| readings.iter().filter(|reading| reading.signal == *signal).max_by_key(|reading| reading.as_of))
}

#[cfg(test)]
mod tests {
    use hegel::generators as gs;

    use super::*;

    const SIGNALS: [StateSignal; 4] = [StateSignal::Native, StateSignal::CleatProgramStatus, StateSignal::Screen, StateSignal::Hook];

    fn reading(signal: StateSignal, seconds: i64) -> StateReading {
        StateReading {
            state: AgentState::Working,
            signal,
            confidence: if signal == StateSignal::Native { Confidence::Native } else { Confidence::Inferred },
            as_of: DateTime::from_timestamp(seconds, 0).expect("timestamp"),
        }
    }

    // Behaviour (#2951 ruling 5): the chosen reading comes from the highest
    // priority signal present in the order, is the newest from that signal,
    // and is never a hook reading. Generator: 0-6 readings over every signal,
    // with colliding timestamps, against orders drawn from every signal
    // including hooks, empty orders and duplicates.
    // Cheap cases: more than the default profile, to reach orders naming hooks.
    #[test]
    fn arbitration_prefers_the_first_present_signal_and_ignores_hooks() {
        hegel::Hegel::new(arbitration_property).settings(hegel::Settings::new().test_cases(300)).run();
    }

    fn arbitration_property(tc: hegel::TestCase) {
        let count = tc.draw(gs::integers::<usize>().min_value(0).max_value(6));
        let readings: Vec<_> = (0..count)
            .map(|_| {
                let signal = SIGNALS[tc.draw(gs::integers::<usize>().min_value(0).max_value(3))];
                reading(signal, tc.draw(gs::integers::<i64>().min_value(0).max_value(3)))
            })
            .collect();
        let length = tc.draw(gs::integers::<usize>().min_value(0).max_value(4));
        let order: Vec<_> = (0..length).map(|_| SIGNALS[tc.draw(gs::integers::<usize>().min_value(0).max_value(3))]).collect();
        let chosen = arbitrate(&order, &readings);
        let expected =
            order.iter().find(|signal| **signal != StateSignal::Hook && readings.iter().any(|reading| reading.signal == **signal));
        match (chosen, expected) {
            (None, None) => {}
            (Some(chosen), Some(signal)) => {
                assert_eq!(chosen.signal, *signal);
                assert!(readings.iter().filter(|reading| reading.signal == *signal).all(|reading| reading.as_of <= chosen.as_of));
            }
            (chosen, expected) => panic!("chose {chosen:?}, expected a reading from {expected:?}"),
        }
    }

    // Behaviour: hooks never decide activity, even when an order names them first.
    #[test]
    fn hook_readings_never_win_arbitration() {
        let readings = [reading(StateSignal::Hook, 9), reading(StateSignal::Screen, 1)];
        let order = [StateSignal::Hook, StateSignal::Screen];
        assert_eq!(arbitrate(&order, &readings).map(|reading| reading.signal), Some(StateSignal::Screen));
        assert_eq!(arbitrate(&[StateSignal::Hook], &readings), None);
    }

    // Behaviour: with the default order, native beats cleat beats screen.
    // Example: the order is the contract's fixed default.
    #[test]
    fn default_order_is_native_then_cleat_then_screen() {
        let readings = [reading(StateSignal::Screen, 9), reading(StateSignal::CleatProgramStatus, 5), reading(StateSignal::Hook, 10)];
        assert_eq!(arbitrate(&DEFAULT_STATE_ORDER, &readings).map(|reading| reading.signal), Some(StateSignal::CleatProgramStatus));
        let readings = [reading(StateSignal::Native, 1), reading(StateSignal::CleatProgramStatus, 5)];
        assert_eq!(arbitrate(&DEFAULT_STATE_ORDER, &readings).map(|reading| reading.signal), Some(StateSignal::Native));
    }
}
