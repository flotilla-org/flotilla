//! When the stall judge runs (#3108): on the changes that can move a convoy's judgement, and at
//! the next instant one turns on time. Nothing is judged on a timer, and a pass judges only the
//! convoys that changed or are due.

use std::collections::{BTreeSet, HashMap, HashSet};

use chrono::{DateTime, Utc};
use flotilla_messaging::SeatMessage;

/// How long after a failed write or withdrawal its convoy is judged again.
pub(super) const RETRY_AFTER: chrono::Duration = chrono::Duration::seconds(30);

/// What one judgement found it waits on besides the changes: the earliest instant a judgement
/// turns on time, and the other homes whose silence left a reading unjudged.
#[derive(Debug, Default)]
pub(super) struct Waits {
    pub(super) due: Option<DateTime<Utc>>,
    pub(super) unheard: BTreeSet<String>,
}

impl Waits {
    pub(super) fn due(&mut self, at: DateTime<Utc>) {
        self.due = Some(self.due.map_or(at, |due| due.min(at)));
    }
}

/// What a convoy's seats' messages are read for: its crew seats, and the turns its status names
/// with the seat each went to.
pub(super) type SeatQuery = (Vec<String>, Vec<(String, String)>);

/// A convoy's seats' messages as last read, with what they were read for.
#[derive(Debug)]
struct Standings {
    query: SeatQuery,
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
    /// Every convoy's seats' messages are to be read again.
    every_unread: bool,
    /// The convoys whose seats' messages changed since they were last read.
    unread: HashSet<String>,
    /// Each judged convoy's seats' messages, as last read.
    standings: HashMap<String, Standings>,
    /// When each convoy's judgement next turns on time.
    due: HashMap<String, DateTime<Utc>>,
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
    /// Nothing judged yet: every convoy is changed and unread.
    fn default() -> Self {
        Self {
            every_changed: true,
            changed: HashSet::new(),
            every_unread: true,
            unread: HashSet::new(),
            standings: HashMap::new(),
            due: HashMap::new(),
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

    /// Messages to the seats of `names` changed, or to any seat for `None`.
    pub(super) fn messages_changed(&mut self, names: Option<&HashSet<String>>) {
        match names {
            Some(names) => {
                self.unread.extend(names.iter().cloned());
                self.changed.extend(names.iter().cloned());
            }
            None => {
                self.every_unread = true;
                self.every_changed = true;
            }
        }
    }

    /// The charter notifications' inputs changed.
    pub(super) fn charter_changed(&mut self) {
        self.charter = true;
    }

    /// Home `origin` was heard from: the convoys its silence left unjudged are judged again.
    pub(super) fn heard(&mut self, origin: &str) {
        if let Some(names) = self.unheard.remove(origin) {
            self.changed.extend(names);
        }
    }

    /// The earliest instant a convoy or the namespace is due.
    pub(super) fn next_due(&self) -> Option<DateTime<Utc>> {
        self.due.values().copied().chain(self.retry).min()
    }

    /// Takes the convoys of `held` to judge at `now`: those changed since they were last judged
    /// and those due. Whether the namespace's own steps are due goes with them.
    /// `held` maps each convoy held here to whether it has ended.
    pub(super) fn take(&mut self, held: &HashMap<&String, bool>, now: DateTime<Utc>) -> Pass {
        // A change that names no convoy passes by the ended ones already judged since they ended:
        // their seats are gone, so nothing they hold can move.
        let every = || held.iter().filter(|(name, ended)| !**ended || !self.ended_judged.contains(**name)).map(|(name, _)| (*name).clone());
        if std::mem::take(&mut self.every_changed) {
            let every: Vec<String> = every().collect();
            self.changed.extend(every);
        }
        if std::mem::take(&mut self.every_unread) {
            let every: Vec<String> = every().collect();
            self.unread.extend(every);
        }
        // A convoy no longer held here is judged no more.
        self.changed.retain(|name| held.contains_key(name));
        self.unread.retain(|name| held.contains_key(name));
        self.standings.retain(|name, _| held.contains_key(name));
        self.due.retain(|name, _| held.contains_key(name));
        self.seen.retain(|name| held.contains_key(name));
        self.ended_judged.retain(|name| held.get(name) == Some(&true));
        let due: Vec<String> = self.due.iter().filter(|(_, at)| **at <= now).map(|(name, _)| name.clone()).collect();
        for name in &due {
            self.due.remove(name);
        }
        let mut judged = std::mem::take(&mut self.changed);
        judged.extend(due);
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

    /// Whether `name`'s seats' messages are to be read for `query`: they changed since they were
    /// last read, or that read asked for other seats or turns.
    pub(super) fn unread(&self, name: &str, query: &SeatQuery) -> bool {
        self.unread.contains(name) || self.standings.get(name).is_none_or(|held| &held.query != query)
    }

    /// `name`'s seats' messages, as just read for `query`.
    pub(super) fn read(&mut self, name: &str, query: SeatQuery, messages: Vec<SeatMessage>) {
        self.unread.remove(name);
        self.standings.insert(name.to_owned(), Standings { query, messages });
    }

    /// The messages to the seats of `judged`, as last read.
    pub(super) fn standings<'a>(&self, judged: impl Iterator<Item = &'a String>) -> Vec<SeatMessage> {
        judged.filter_map(|name| self.standings.get(name)).flat_map(|held| held.messages.iter().cloned()).collect()
    }

    /// What judging `name` found it waits on.
    pub(super) fn waits(&mut self, name: &str, waits: Waits) {
        for origin in waits.unheard {
            self.unheard.entry(origin).or_default().insert(name.to_owned());
        }
        if let Some(at) = waits.due {
            self.due.insert(name.to_owned(), at);
        }
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

    #[test]
    fn a_pass_takes_the_changed_and_the_due_and_nothing_else() {
        let now = Utc::now();
        let held_names = names(&["a", "b", "c"]);
        let held = live(&held_names);
        let mut schedule = StallSchedule::default();
        // The first pass judges every convoy, once.
        assert_eq!(schedule.take(&held, now).judged, held_names);
        assert!(schedule.take(&held, now).is_empty());
        schedule.waits("b", Waits { due: Some(now + chrono::Duration::seconds(5)), unheard: BTreeSet::new() });
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
        schedule.waits("a", Waits { due: None, unheard: BTreeSet::from(["other".to_owned()]) });
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
}
