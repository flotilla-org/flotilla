//! A kind's engine records held in memory, served as its real port serves them
//! ([`super::profile::Profile`]).
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Mutex, MutexGuard, PoisonError},
};

use flotilla_resources::{
    engine_records::{KindRecords, LogPosition, Record, Since},
    Resource,
};
use futures::stream::BoxStream;

use super::profile::{profile_of, Profile};

/// A kind's records at each home. This home writes its own as the kind's profile allows; every
/// write moves this home's log position; a record any home holds arrives through
/// [`MemoryRecords::hold`], as the engine's log brings it.
pub struct MemoryRecords<R: Resource> {
    home: String,
    profile: Profile,
    state: Mutex<State<R>>,
}

struct State<R: Resource> {
    /// Every home's records, by name and home.
    records: BTreeMap<(String, String), Record<R>>,
    /// Each home's changes, in order: the name each touched.
    log: BTreeMap<String, Vec<String>>,
    subscribers: Vec<futures::channel::mpsc::UnboundedSender<()>>,
}

impl<R: Resource> std::fmt::Debug for MemoryRecords<R> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("MemoryRecords").field("kind", &R::API_PATHS.kind).field("home", &self.home).finish()
    }
}

impl<R: Resource> MemoryRecords<R> {
    /// `R`'s records at `home`, served as its profile says.
    pub fn at(home: &str) -> Self {
        Self::with_profile(home, profile_of(R::API_PATHS.kind))
    }

    pub fn with_profile(home: &str, profile: Profile) -> Self {
        Self {
            home: home.to_owned(),
            profile,
            state: Mutex::new(State { records: BTreeMap::new(), log: BTreeMap::new(), subscribers: Vec::new() }),
        }
    }

    fn state(&self) -> MutexGuard<'_, State<R>> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// `home` holds `name` as `spec` with `status`: created, or replaced with its version moved, as
    /// that home's engine would hold it; the change reaches this home's readers.
    pub fn hold(&self, home: &str, name: &str, spec: R::Spec, status: Option<R::Status>) -> Record<R> {
        let mut state = self.state();
        let key = (name.to_owned(), home.to_owned());
        let record = match state.records.get(&key) {
            Some(held) => Record { version: held.version + 1, spec, status, ..held.clone() },
            None => Record {
                name: name.to_owned(),
                home: home.to_owned(),
                version: 1,
                created_at: chrono::Utc::now(),
                labels: BTreeMap::new(),
                annotations: BTreeMap::new(),
                spec,
                status,
            },
        };
        state.records.insert(key, record.clone());
        Self::logged(&mut state, home, name);
        record
    }

    /// `home` no longer holds `name`; the change reaches this home's readers.
    pub fn release(&self, home: &str, name: &str) {
        let mut state = self.state();
        state.records.remove(&(name.to_owned(), home.to_owned()));
        Self::logged(&mut state, home, name);
    }

    fn logged(state: &mut State<R>, home: &str, name: &str) {
        state.log.entry(home.to_owned()).or_default().push(name.to_owned());
        state.subscribers.retain(|subscriber| subscriber.unbounded_send(()).is_ok());
    }

    fn position(state: &State<R>) -> LogPosition {
        LogPosition::logged(state.log.iter().map(|(home, names)| (home.clone(), names.len() as u64)).collect())
    }

    /// `name`'s record: this home's where it holds one, else the lexically first home's.
    fn resolve(&self, state: &State<R>, name: &str) -> Option<Record<R>> {
        let mut copies = state.records.range((name.to_owned(), String::new())..).take_while(|((held, _), _)| held == name);
        let first = copies.next()?;
        Some(std::iter::once(first).chain(copies).find(|((_, home), _)| *home == self.home).unwrap_or(first).1.clone())
    }

    fn refused(&self, name: &str, what: &str) -> String {
        format!("{} {name}: {what} through its engine port is refused", R::API_PATHS.kind)
    }
}

#[async_trait::async_trait]
impl<R: Resource> KindRecords<R> for MemoryRecords<R> {
    fn codes(&self) -> Vec<flotilla_resources::engine_records::KindCode> {
        Vec::new()
    }

    /// A write queues its tick before it returns.
    fn ticks(&self) -> flotilla_resources::engine_records::Ticks {
        flotilla_resources::engine_records::Ticks::AtCommit
    }

    fn home(&self) -> String {
        self.home.clone()
    }

    async fn get(&self, name: &str) -> Result<Option<Record<R>>, String> {
        Ok(self.resolve(&self.state(), name))
    }

    async fn list(&self) -> Result<Vec<Record<R>>, String> {
        let state = self.state();
        let names: BTreeSet<&String> = state.records.keys().map(|(name, _)| name).collect();
        Ok(names.into_iter().filter_map(|name| self.resolve(&state, name)).collect())
    }

    async fn sources(&self) -> Result<Vec<Record<R>>, String> {
        Ok(self.state().records.values().cloned().collect())
    }

    async fn positioned(&self) -> Result<(Vec<Record<R>>, LogPosition), String> {
        let state = self.state();
        Ok((state.records.values().cloned().collect(), Self::position(&state)))
    }

    async fn position(&self) -> Result<LogPosition, String> {
        Ok(Self::position(&self.state()))
    }

    async fn changed_since(&self, position: &LogPosition) -> Result<Since, String> {
        let state = self.state();
        let mut names = BTreeSet::new();
        for (home, log) in &state.log {
            let read = position.homes.get(home).copied().unwrap_or_default();
            if read > log.len() as u64 {
                return Ok(Since::Expired(format!("{home}'s log is behind the position")));
            }
            names.extend(log.iter().skip(read as usize).cloned());
        }
        Ok(Since::Changed { names, through: Self::position(&state) })
    }

    async fn copies(&self, name: &str) -> Result<Vec<Record<R>>, String> {
        let state = self.state();
        Ok(state
            .records
            .range((name.to_owned(), String::new())..)
            .take_while(|((held, _), _)| held == name)
            .map(|(_, r)| r.clone())
            .collect())
    }

    async fn apply(
        &self,
        name: &str,
        labels: &BTreeMap<String, String>,
        annotations: &BTreeMap<String, String>,
        spec: &R::Spec,
    ) -> Result<Record<R>, String> {
        if !self.profile.writes {
            return Err(self.refused(name, "writing"));
        }
        let labels = if self.profile.labels { labels.clone() } else { BTreeMap::new() };
        let annotations = if self.profile.annotations { annotations.clone() } else { BTreeMap::new() };
        let mut state = self.state();
        let key = (name.to_owned(), self.home.clone());
        let record = match state.records.get(&key) {
            Some(held) => {
                // Only an edit of the spec moves the version, as on the engine.
                let edited = serde_json::to_value(&held.spec).ok() != serde_json::to_value(spec).ok();
                Record { version: held.version + u64::from(edited), labels, annotations, spec: spec.clone(), ..held.clone() }
            }
            None => Record {
                name: name.to_owned(),
                home: self.home.clone(),
                version: 1,
                created_at: chrono::Utc::now(),
                labels,
                annotations,
                spec: spec.clone(),
                status: None,
            },
        };
        state.records.insert(key, record.clone());
        Self::logged(&mut state, &self.home, name);
        Ok(record)
    }

    async fn set_status(&self, name: &str, status: &R::Status) -> Result<Record<R>, String> {
        if !self.profile.status {
            return Err(self.refused(name, "writing a status"));
        }
        let mut state = self.state();
        let key = (name.to_owned(), self.home.clone());
        let record = state.records.get_mut(&key).ok_or_else(|| format!("{} {name} is not held at {}", R::API_PATHS.kind, self.home))?;
        record.status = Some(status.clone());
        record.version += u64::from(self.profile.status_moves_version);
        let record = record.clone();
        Self::logged(&mut state, &self.home, name);
        Ok(record)
    }

    async fn delete(&self, name: &str) -> Result<(), String> {
        if !self.profile.writes {
            return Err(self.refused(name, "deleting"));
        }
        let mut state = self.state();
        if state.records.remove(&(name.to_owned(), self.home.clone())).is_some() {
            Self::logged(&mut state, &self.home, name);
        }
        Ok(())
    }

    async fn changes(&self) -> Result<BoxStream<'static, ()>, String> {
        let (sender, receiver) = futures::channel::mpsc::unbounded();
        self.state().subscribers.push(sender);
        Ok(futures::StreamExt::boxed(receiver))
    }
}
