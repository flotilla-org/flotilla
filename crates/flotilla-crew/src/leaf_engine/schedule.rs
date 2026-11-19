//! When the stall judge runs (#3108): on the changes that can move a convoy's judgement, and at
//! the next instant one turns on time. Nothing is judged on a timer, and a pass judges only the
//! convoys that changed or are due.
//!
//! A crew session's own facts, its AgentSession reading and its seat's messages, are held between
//! passes and read again only on that session's own changes or when one of its deadlines comes
//! due. A convoy judged for any reason composes its seats' judgements from the facts held, so a
//! wake reads only the session it is about.

use std::collections::{BTreeSet, HashMap, HashSet};

use chrono::{DateTime, Utc};
use flotilla_messaging::SeatMessage;
use flotilla_resources::agent_activity::AgentActivityView;

/// How long after a failed write or withdrawal its convoy is judged again.
pub(super) const RETRY_AFTER: chrono::Duration = chrono::Duration::seconds(30);

/// An AgentSession as the judge reads it: its name, and the other home it is homed at.
pub(super) type SessionKey = (String, Option<String>);

/// What one judgement found it waits on besides the changes: the earliest instant a judgement
/// of the convoy turns on time, the instant each of its sessions' does, and the other homes whose
/// silence left a reading unjudged.
#[derive(Debug, Default)]
pub(super) struct Waits {
    pub(super) due: Option<DateTime<Utc>>,
    pub(super) sessions: HashMap<SessionKey, DateTime<Utc>>,
    /// The sessions the judgement read.
    pub(super) read: HashSet<SessionKey>,
    pub(super) unheard: BTreeSet<String>,
}

impl Waits {
    pub(super) fn due(&mut self, at: DateTime<Utc>) {
        self.due = Some(self.due.map_or(at, |due| due.min(at)));
    }

    pub(super) fn session_due(&mut self, session: &SessionKey, at: DateTime<Utc>) {
        self.sessions.entry(session.clone()).and_modify(|due| *due = (*due).min(at)).or_insert(at);
    }
}

/// A seat's messages as last read, with the turns they were read for.
#[derive(Debug)]
struct Standings {
    turns: Vec<String>,
    messages: Vec<SeatMessage>,
}

/// What the judge holds between passes in one namespace.
#[derive(Debug)]
pub(super) struct StallSchedule {
    /// Every convoy changed: the first pass, or a change that names none, such as a feed that
    /// lagged. An ended convoy already judged since it ended is not among them.
    every_changed: bool,
    /// The convoys whose inputs changed since they were last judged.
    changed: HashSet<String>,
    /// Each seat's messages as last read, by the seat. A change to its inbox drops them.
    standings: HashMap<String, Standings>,
    /// The seats each judged convoy's messages were last read for.
    seats: HashMap<String, BTreeSet<String>>,
    /// Each session's reading as last read; none where it has none. A change to the session, or
    /// hearing from its home, drops it.
    readings: HashMap<SessionKey, Option<AgentActivityView>>,
    /// The sessions each judged convoy last read.
    readers: HashMap<String, HashSet<SessionKey>>,
    /// When each convoy's judgement next turns on time.
    due: HashMap<String, DateTime<Utc>>,
    /// When each session's judgement next turns on time, by the convoy that judged it.
    session_due: HashMap<String, HashMap<SessionKey, DateTime<Utc>>>,
    /// When the namespace's own steps are tried again after a failure.
    retry: Option<DateTime<Utc>>,
    /// The convoys another home's silence left unjudged, by that home.
    unheard: HashMap<String, HashSet<String>>,
    /// The charter notifications are to be reconciled again.
    charter: bool,
    /// The convoys judged since the schedule began.
    seen: HashSet<String>,
    /// The ended convoys judged since they ended.
    ended_judged: HashSet<String>,
}

impl Default for StallSchedule {
    /// Nothing judged yet: every convoy is changed, and nothing is held.
    fn default() -> Self {
        Self {
            every_changed: true,
            changed: HashSet::new(),
            standings: HashMap::new(),
            seats: HashMap::new(),
            readings: HashMap::new(),
            readers: HashMap::new(),
            due: HashMap::new(),
            session_due: HashMap::new(),
            retry: None,
            unheard: HashMap::new(),
            charter: true,
            seen: HashSet::new(),
            ended_judged: HashSet::new(),
        }
    }
}

impl StallSchedule {
    /// The inputs of `names` changed, or every convoy's for `None`.
    pub(super) fn changed(&mut self, names: Option<&HashSet<String>>) {
        match names {
            Some(names) => self.changed.extend(names.iter().cloned()),
            None => self.every_changed = true,
        }
    }

    /// Convoy records `names` changed, or every one for `None`. Their seats' messages are read
    /// again only once the seats or the turns they name differ from the last read's.
    pub(super) fn convoys_changed(&mut self, names: Option<&HashSet<String>>) {
        self.changed(names);
        self.charter = true;
    }

    /// Messages in `inboxes` changed: the seats they hold are read again, and the convoys
    /// `names` judged.
    pub(super) fn inboxes_changed(&mut self, inboxes: &BTreeSet<flotilla_address::Address>, names: &HashSet<String>) {
        // A seat with no inbox holds no message, so no change is to it.
        self.standings.retain(|seat, _| flotilla_resources::convoy::engine::role_inbox(seat).is_ok_and(|inbox| !inboxes.contains(&inbox)));
        self.changed.extend(names.iter().cloned());
    }

    /// Any seat's messages may have changed: every seat is read again.
    pub(super) fn messages_rescanned(&mut self) {
        self.standings.clear();
        self.every_changed = true;
    }

    /// Session `session` changed: its reading is read again.
    pub(super) fn session_changed(&mut self, session: &SessionKey) {
        self.readings.remove(session);
    }

    /// Any session may have changed: every reading is read again, and every convoy judged.
    pub(super) fn sessions_rescanned(&mut self) {
        self.readings.clear();
        self.every_changed = true;
    }

    /// The charter notifications' inputs changed.
    pub(super) fn charter_changed(&mut self) {
        self.charter = true;
    }

    /// Home `origin` was heard from: its sessions' readings are read again, and the convoys its
    /// silence left unjudged judged again.
    pub(super) fn heard(&mut self, origin: &str) {
        self.readings.retain(|(_, home), _| home.as_deref() != Some(origin));
        if let Some(names) = self.unheard.remove(origin) {
            self.changed.extend(names);
        }
    }

    /// The earliest instant a convoy, one of its sessions or the namespace is due.
    pub(super) fn next_due(&self) -> Option<DateTime<Utc>> {
        self.due.values().chain(self.session_due.values().flat_map(HashMap::values)).copied().chain(self.retry).min()
    }

    /// Takes the convoys of `held` to judge at `now`: those changed since they were last judged
    /// and those due, themselves or by one of their sessions. A session due is read again.
    /// Whether the namespace's own steps are due goes with them.
    /// `held` maps each convoy held here to whether it has ended.
    pub(super) fn take(&mut self, held: &HashMap<&String, bool>, now: DateTime<Utc>) -> Pass {
        // A change that names no convoy passes by the ended ones already judged since they ended:
        // their seats are gone, so nothing they hold can move.
        if std::mem::take(&mut self.every_changed) {
            let every: Vec<String> = held
                .iter()
                .filter(|(name, ended)| !**ended || !self.ended_judged.contains(**name))
                .map(|(name, _)| (*name).clone())
                .collect();
            self.changed.extend(every);
        }
        // A convoy no longer held here is judged no more, and what it alone held goes.
        self.changed.retain(|name| held.contains_key(name));
        self.due.retain(|name, _| held.contains_key(name));
        self.session_due.retain(|name, _| held.contains_key(name));
        self.seen.retain(|name| held.contains_key(name));
        self.ended_judged.retain(|name| held.get(name) == Some(&true));
        if self.seats.len() != self.seats.iter().filter(|(name, _)| held.contains_key(name)).count() {
            self.seats.retain(|name, _| held.contains_key(name));
            let seats: HashSet<&String> = self.seats.values().flatten().collect();
            self.standings.retain(|seat, _| seats.contains(seat));
        }
        if self.readers.len() != self.readers.iter().filter(|(name, _)| held.contains_key(name)).count() {
            self.readers.retain(|name, _| held.contains_key(name));
            let read: HashSet<&SessionKey> = self.readers.values().flatten().collect();
            self.readings.retain(|session, _| read.contains(session));
        }
        let due: Vec<String> = self.due.iter().filter(|(_, at)| **at <= now).map(|(name, _)| name.clone()).collect();
        for name in &due {
            self.due.remove(name);
        }
        let mut judged = std::mem::take(&mut self.changed);
        judged.extend(due);
        for (name, sessions) in &mut self.session_due {
            let due: Vec<SessionKey> = sessions.iter().filter(|(_, at)| **at <= now).map(|(session, _)| session.clone()).collect();
            for session in due {
                sessions.remove(&session);
                self.readings.remove(&session);
                judged.insert(name.clone());
            }
        }
        self.session_due.retain(|_, sessions| !sessions.is_empty());
        let retry = self.retry.is_some_and(|at| at <= now);
        if retry {
            self.retry = None;
        }
        let first = judged.iter().filter(|name| self.seen.insert((*name).clone())).cloned().collect();
        let ended =
            judged.iter().filter(|name| held.get(name) == Some(&true) && self.ended_judged.insert((*name).clone())).cloned().collect();
        Pass { judged, first, ended, charter: std::mem::take(&mut self.charter), retry }
    }

    /// Puts back what `pass` took and did not finish, for the next pass.
    pub(super) fn put_back(&mut self, pass: Pass) {
        for name in &pass.first {
            self.seen.remove(name);
        }
        for name in &pass.ended {
            self.ended_judged.remove(name);
        }
        self.changed.extend(pass.judged);
        self.charter |= pass.charter;
    }

    /// Whether `seat`'s messages are to be read for `turns`: they changed since they were last
    /// read, or that read asked for other turns.
    pub(super) fn unread(&self, seat: &str, turns: &[String]) -> bool {
        self.standings.get(seat).is_none_or(|held| held.turns != turns)
    }

    /// `seat`'s messages, as just read for `turns`.
    pub(super) fn read(&mut self, seat: &str, turns: Vec<String>, messages: Vec<SeatMessage>) {
        self.standings.insert(seat.to_owned(), Standings { turns, messages });
    }

    /// Convoy `name`'s messages are read for `seats`; the seats it no longer has are dropped.
    pub(super) fn seats(&mut self, name: &str, seats: BTreeSet<String>) {
        if let Some(gone) = self.seats.insert(name.to_owned(), seats) {
            let seats = &self.seats[name];
            for seat in gone.iter().filter(|seat| !seats.contains(*seat)) {
                self.standings.remove(seat);
            }
        }
    }

    /// The messages to the seats of `judged`, as last read.
    pub(super) fn standings<'a>(&self, judged: impl Iterator<Item = &'a String>) -> Vec<SeatMessage> {
        judged
            .filter_map(|name| self.seats.get(name))
            .flatten()
            .filter_map(|seat| self.standings.get(seat))
            .flat_map(|held| held.messages.iter().cloned())
            .collect()
    }

    /// The sessions' readings held, for a pass to read from and add to.
    pub(super) fn take_readings(&mut self) -> HashMap<SessionKey, Option<AgentActivityView>> {
        std::mem::take(&mut self.readings)
    }

    /// The sessions' readings after a pass, those no judged convoy read since dropped.
    pub(super) fn hold_readings(&mut self, mut readings: HashMap<SessionKey, Option<AgentActivityView>>) {
        let read: HashSet<&SessionKey> = self.readers.values().flatten().collect();
        readings.retain(|session, _| read.contains(session));
        self.readings = readings;
    }

    /// What judging `name` found it waits on.
    pub(super) fn waits(&mut self, name: &str, waits: Waits) {
        for origin in waits.unheard {
            self.unheard.entry(origin).or_default().insert(name.to_owned());
        }
        if let Some(at) = waits.due {
            self.due.insert(name.to_owned(), at);
        }
        if waits.sessions.is_empty() {
            self.session_due.remove(name);
        } else {
            self.session_due.insert(name.to_owned(), waits.sessions);
        }
        self.readers.insert(name.to_owned(), waits.read);
    }

    /// What the namespace's own steps found they wait on: a retry after a failure.
    pub(super) fn namespace_waits(&mut self, waits: Waits) {
        if let Some(at) = waits.due {
            self.retry = Some(self.retry.map_or(at, |retry| retry.min(at)));
        }
    }
}

/// What one pass judges.
#[derive(Debug, Default)]
pub(super) struct Pass {
    /// The convoys judged.
    pub(super) judged: HashSet<String>,
    /// Those of them judged for the first time since the schedule began.
    pub(super) first: HashSet<String>,
    /// Those of them ended and judged for the first time since they ended.
    ended: HashSet<String>,
    /// Whether the charter notifications are reconciled.
    pub(super) charter: bool,
    /// Whether the namespace's own steps are retried.
    pub(super) retry: bool,
}

impl Pass {
    /// Whether the pass has anything to do.
    pub(super) fn is_empty(&self) -> bool {
        self.judged.is_empty() && !self.charter && !self.retry
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(names: &[&str]) -> HashSet<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    /// `names` held here, each live.
    fn live(names: &HashSet<String>) -> HashMap<&String, bool> {
        names.iter().map(|name| (name, false)).collect()
    }

    fn session(name: &str) -> SessionKey {
        (name.to_owned(), None)
    }

    #[test]
    fn a_pass_takes_the_changed_and_the_due_and_nothing_else() {
        let now = Utc::now();
        let held_names = names(&["a", "b", "c"]);
        let held = live(&held_names);
        let mut schedule = StallSchedule::default();
        // The first pass judges every convoy, once.
        assert_eq!(schedule.take(&held, now).judged, held_names);
        assert!(schedule.take(&held, now).is_empty());
        schedule.waits("b", Waits { due: Some(now + chrono::Duration::seconds(5)), ..Waits::default() });
        schedule.changed(Some(&names(&["a", "gone"])));
        assert_eq!(schedule.next_due(), Some(now + chrono::Duration::seconds(5)));
        assert_eq!(schedule.take(&held, now).judged, names(&["a"]));
        assert_eq!(schedule.take(&held, now + chrono::Duration::seconds(5)).judged, names(&["b"]));
        assert_eq!(schedule.next_due(), None);
    }

    #[test]
    fn a_heard_home_judges_again_what_its_silence_left() {
        let now = Utc::now();
        let held_names = names(&["a", "b"]);
        let held = live(&held_names);
        let mut schedule = StallSchedule::default();
        schedule.take(&held, now);
        schedule.waits("a", Waits { unheard: BTreeSet::from(["other".to_owned()]), ..Waits::default() });
        schedule.heard("elsewhere");
        assert!(schedule.take(&held, now).is_empty());
        schedule.heard("other");
        assert_eq!(schedule.take(&held, now).judged, names(&["a"]));
    }

    #[test]
    fn a_change_naming_no_convoy_passes_by_the_ended_already_judged() {
        let now = Utc::now();
        let held_names = names(&["live", "ended", "ending"]);
        let mut held: HashMap<&String, bool> = held_names.iter().map(|name| (name, name == "ended")).collect();
        let mut schedule = StallSchedule::default();
        assert_eq!(schedule.take(&held, now).judged, held_names, "the first pass judges every convoy");
        schedule.changed(None);
        assert_eq!(schedule.take(&held, now).judged, names(&["live", "ending"]));
        // A convoy that ended while its change went unheard is judged once more, then passed by.
        held.insert(held_names.get("ending").expect("held"), true);
        schedule.changed(None);
        assert_eq!(schedule.take(&held, now).judged, names(&["live", "ending"]));
        schedule.changed(None);
        assert_eq!(schedule.take(&held, now).judged, names(&["live"]));
        // A change that names an ended convoy still judges it.
        schedule.changed(Some(&names(&["ended"])));
        assert_eq!(schedule.take(&held, now).judged, names(&["ended"]));
    }

    // A session's deadline judges its convoy and reads that session again; the convoy's other
    // sessions' readings stay held, and its own deadline is left as it was.
    #[test]
    fn a_session_due_reads_only_that_session_again() {
        let now = Utc::now();
        let held_names = names(&["a"]);
        let held = live(&held_names);
        let mut schedule = StallSchedule::default();
        schedule.take(&held, now);
        let mut readings = schedule.take_readings();
        readings.insert(session("coder"), None);
        readings.insert(session("reviewer"), None);
        let mut waits = Waits { due: Some(now + chrono::Duration::seconds(60)), ..Waits::default() };
        waits.read = HashSet::from([session("coder"), session("reviewer")]);
        waits.session_due(&session("coder"), now + chrono::Duration::seconds(10));
        waits.session_due(&session("coder"), now + chrono::Duration::seconds(5));
        schedule.waits("a", waits);
        schedule.hold_readings(readings);
        assert_eq!(schedule.next_due(), Some(now + chrono::Duration::seconds(5)));
        assert!(schedule.take(&held, now + chrono::Duration::seconds(4)).is_empty());
        assert_eq!(schedule.take(&held, now + chrono::Duration::seconds(5)).judged, names(&["a"]));
        let readings = schedule.take_readings();
        assert!(!readings.contains_key(&session("coder")), "the due session was not read again");
        assert!(readings.contains_key(&session("reviewer")), "a session not due was read again");
        assert_eq!(schedule.next_due(), Some(now + chrono::Duration::seconds(60)));
    }

    // A reading no judged convoy read any more is not held.
    #[test]
    fn a_reading_no_convoy_reads_is_dropped() {
        let now = Utc::now();
        let held_names = names(&["a", "b"]);
        let held = live(&held_names);
        let mut schedule = StallSchedule::default();
        schedule.take(&held, now);
        let mut readings = schedule.take_readings();
        readings.insert(session("a-old"), None);
        readings.insert(session("b"), None);
        schedule.waits("a", Waits { read: HashSet::from([session("a-old")]), ..Waits::default() });
        schedule.waits("b", Waits { read: HashSet::from([session("b")]), ..Waits::default() });
        schedule.hold_readings(readings);
        // `a` relaunched its seat: it reads another session now.
        schedule.waits("a", Waits { read: HashSet::from([session("a-new")]), ..Waits::default() });
        let mut readings = schedule.take_readings();
        readings.insert(session("a-new"), None);
        schedule.hold_readings(readings);
        let readings = schedule.take_readings();
        assert_eq!(readings.keys().cloned().collect::<HashSet<_>>(), HashSet::from([session("a-new"), session("b")]));
        schedule.hold_readings(readings);
        // `b` is held here no more.
        let held_names = names(&["a"]);
        schedule.take(&live(&held_names), now);
        assert_eq!(schedule.take_readings().keys().cloned().collect::<HashSet<_>>(), HashSet::from([session("a-new")]));
    }
}
