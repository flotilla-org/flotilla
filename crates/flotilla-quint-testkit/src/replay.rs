//! Shared Quint trace, step, stream and small value helpers.
use crate::itf::{self, at, boolean, list, number, payload, show, slot, tag, text, variant};
use flotilla_schema::{Address, Timestamp};
use serde_json::Value;
use std::{collections::BTreeSet, fmt};

/// The delivery condition of `controller_delivery`'s envelope, and the
/// turn-rules role that withdraws a message once it lapses.
pub const DELIVERY: &str = "project:still-wanted";
pub const TURN_RULES: &str = "project:fleet/role:turn-rules";
/// Microseconds per model clock tick (ten minutes).
pub const UNIT: i64 = 600_000_000;
/// The inbox every replayed message and Ensure names, as the world keys it:
/// a canonical inbox address.
pub const INBOX: &str = "project:replay/role:inbox";
/// The authorised party, the superseding message and the outcome condition, as
/// the world keys them: each address's canonical text.
pub const OWNER: &str = "project:owner";
pub const NEWER: &str = "project:newer";
pub const OUTCOME: &str = "project:outcome";
pub fn moment(ticks: i64) -> Timestamp {
    Timestamp::from_micros(ticks * UNIT)
}

/// A resource name as the model integer it stands for.
pub fn id_of(name: &str) -> i64 {
    name.parse().unwrap_or_else(|_| panic!("resource name {name} is not a model id"))
}

/// A canonical address; the world is keyed by its text.
/// The delivery condition the replay's envelopes carry: about [`DELIVERY`].
pub const DELIVERY_CONDITION: &str = "project:still-wanted .phase == Wanted";
/// The outcome condition an Outcome expectation waits on: about [`OUTCOME`].
pub const OUTCOME_CONDITION: &str = "project:outcome .phase == Done";
pub fn condition(text: &str) -> flotilla_address::Condition {
    text.parse().unwrap_or_else(|error| panic!("{text} as a condition: {error}"))
}
pub fn at_address(text: &str) -> Address {
    text.parse().unwrap_or_else(|error| panic!("{text} as an address: {error}"))
}

/// A sum value without a payload, as ITF encodes it.
pub fn constant(name: &str) -> Value {
    variant(name, itf::unit())
}

/// One trace file's states.
pub struct Trace<'a> {
    pub file: &'a str,
    pub states: &'a [Value],
}

/// One model step: the state it starts in and the state it reaches.
#[derive(Clone, Copy)]
pub struct Step<'a> {
    pub file: &'a str,
    pub index: usize,
    pub before: &'a Value,
    pub after: &'a Value,
}

impl<'a> Trace<'a> {
    pub fn steps(&self) -> impl Iterator<Item = Step<'a>> + 'a {
        let file = self.file;
        self.states.windows(2).enumerate().map(move |(i, pair)| Step { file, index: i + 1, before: &pair[0], after: &pair[1] })
    }
}

impl<'a> Step<'a> {
    pub fn action(&self) -> &'a str {
        text(&self.after["mbt::actionTaken"])
    }

    pub fn pick(&self, key: &str) -> &'a Value {
        let picked = &self.after["mbt::nondetPicks"][key];
        assert_eq!(tag(picked), "Some", "{self}: no pick for {key}");
        payload(picked)
    }

    pub fn chosen(&self, key: &str) -> i64 {
        number(self.pick(key))
    }

    pub fn yes(&self, key: &str) -> bool {
        boolean(self.pick(key))
    }

    /// A model time for scenarios without a clock: one tick per step.
    pub fn tick(&self) -> Timestamp {
        moment(i64::try_from(self.index).expect("step index"))
    }
}

impl fmt::Display for Step<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} step {} ({})", self.file, self.index, self.action())
    }
}

/// The imported store whose qualified name ends in `suffix`, e.g. `::convoys`.
pub fn store<'v>(state: &'v Value, suffix: &str) -> &'v Value {
    let mut found = state.as_object().expect("ITF state").iter().filter(|(name, _)| name.ends_with(suffix));
    let (_, value) = found.next().unwrap_or_else(|| panic!("no store ending in {suffix}"));
    assert!(found.next().is_none(), "one store ending in {suffix}");
    value
}

pub fn slot_at<'v>(state: &'v Value, suffix: &str, id: i64) -> Option<&'v Value> {
    slot(at(store(state, suffix), id))
}

/// Rust and the model hold the same value, up to set and map order.
pub fn same(rust: &Value, model: &Value, step: &Step, what: &str) {
    assert!(itf::normalise(rust) == itf::normalise(model), "{step}: {what} differs\n   Rust: {}\n  model: {}", show(rust), show(model));
}

/// One stream row: when, what kind, and which session wrote it.
pub struct Row {
    pub at: i64,
    pub kind: String,
    pub session: i64,
}

/// A consumer stream (`world/stream.qnt`): rows in order and the applied prefix.
pub struct Stream {
    pub rows: Vec<Row>,
    pub applied: usize,
}

impl Stream {
    pub fn of(v: &Value) -> Self {
        let rows = list(&v["rows"]).iter().map(|row| Row {
            at: number(&row["at"]),
            kind: text(&row["kind"]).into(),
            session: number(&row["session"]),
        });
        Stream { rows: rows.collect(), applied: usize::try_from(number(&v["applied"])).expect("applied prefix") }
    }

    pub fn seen(&self) -> &[Row] {
        &self.rows[..self.applied]
    }

    pub fn sessions(&self, kind: &str) -> BTreeSet<i64> {
        self.seen().iter().filter(|row| row.kind == kind).map(|row| row.session).collect()
    }

    pub fn any(&self, kind: &str) -> bool {
        self.seen().iter().any(|row| row.kind == kind)
    }

    /// `S::watermark`: the newest applied keepalive, else zero.
    pub fn watermark(&self) -> i64 {
        self.seen().iter().filter(|row| row.kind == "keepalive").map(|row| row.at).fold(0, i64::max)
    }
}
