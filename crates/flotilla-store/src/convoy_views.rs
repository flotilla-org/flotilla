//! Convoys in the old Convoy shape, composed from the engine's launches on each read (#3108,
//! #4412 C).
//!
//! The engine holds each launch of a convoy ([`ConvoyRecords`]); readers not yet converted to
//! records see each launch as a [`Convoy`] read view named by the launch, its phases derived from
//! the launch, its works' Vessels and its seats' sessions, and, at the launch's own home, joined
//! with what that home observes of it ([`flotilla_resources::convoy_compose`]). Nothing is held:
//! a read by name composes that launch from its record and its joins by key, and a listing
//! composes every launch. No read waits on another. Nothing here writes: the convoy verbs write
//! edges, and the home its notes.
//!
//! A listing holds the launches that have not retired; a read by name finds a retired one too,
//! with how it ended. What changed since a position is named from the log, by the launches, the
//! Vessel launches and the sessions written since it, each joined back to its launch by key, and
//! from the notes this home changed since it.
//!
//! Installed as the backend's [`KindRecords`] for `Convoy`, it serves the typed reads, lists and
//! watches of the kind; its writes are refused.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex, OnceLock, PoisonError},
};

use futures::{stream::BoxStream, StreamExt};

use flotilla_resources::{
    convoy_compose::{compose_listing, compose_named, Composed},
    convoy_records::{ConvoyChange, ConvoyListing, ConvoyRecords},
    engine_records::{IndexCover, KindRecords, LogPosition, Record, Since, WriteError},
    lifecycle_reads::{ChangeCursor, CursorSince, LifecycleReads},
    terminal_records::TerminalChange,
    vessel_records::VesselChange,
    Convoy, ConvoySpec, ConvoyStatus,
};

use crate::held_listing::ticks;

/// Read views of the convoys in one namespace, composed on each read.
#[derive(Clone)]
pub struct ConvoyViews {
    namespace: String,
    reads: Arc<dyn LifecycleReads>,
    /// The log's cursors, opened at the first read that names changes; none where no log is held,
    /// and every position then expires.
    log: Arc<OnceLock<Option<Mutex<LogCursors>>>>,
}

/// Cursors over the log of the records a launch's view is composed from. They are never passed:
/// each read asks one what was written since a position, from the log.
struct LogCursors {
    convoys: Box<dyn ChangeCursor<ConvoyChange>>,
    vessels: Box<dyn ChangeCursor<VesselChange>>,
    terminals: Box<dyn ChangeCursor<TerminalChange>>,
    /// The latest each home's sequence a read was named through: where the next position is
    /// read from.
    heads: BTreeMap<String, u64>,
}

impl ConvoyViews {
    pub fn new(namespace: &str, reads: Arc<dyn LifecycleReads>) -> Self {
        Self { namespace: namespace.to_owned(), reads, log: Arc::default() }
    }

    async fn blocking<T: Send + 'static>(
        &self,
        read: impl FnOnce(&ConvoyViews) -> Result<T, String> + Send + 'static,
    ) -> Result<T, String> {
        let views = self.clone();
        tokio::task::spawn_blocking(flotilla_schema::reads::carry(move || read(&views))).await.map_err(|error| error.to_string())?
    }

    fn convoys(&self) -> Result<&dyn ConvoyRecords, String> {
        self.reads.convoys()
    }

    fn here(&self) -> String {
        self.convoys().map(|convoys| convoys.home().to_owned()).unwrap_or_default()
    }

    /// Every home's launch `name`, a retired one with how it ended.
    fn named(&self, name: &str) -> Result<Vec<Composed>, String> {
        compose_named(self.reads.as_ref(), &self.namespace, name, None)
    }

    /// Every launch at every home that has not retired. Those that cannot be read are counted as
    /// unreadable.
    /// The launches `sources` name, each read through its key or the `open_by_project` index,
    /// composed; this home's alone when `local`.
    fn covering(&self, sources: &BTreeSet<Source>, local: bool) -> Result<Vec<Composed>, String> {
        let convoys = self.convoys()?;
        let mut listing = ConvoyListing::default();
        for source in sources {
            listing.extend(match source {
                Source::Named(name) => convoys.named(name, None)?,
                Source::OpenIn(project) => convoys.open_in(project)?,
                Source::InProject(project) => convoys.of_project(project)?,
                Source::Launches(project, of) => convoys.launches_of(of, project)?,
                Source::OpenRole(project, role) => convoys.open_of_role(project, role)?,
            });
        }
        // A launch two sources name is composed once.
        let mut seen = BTreeSet::new();
        listing.records.retain(|record| seen.insert((record.name.clone(), record.home.clone())));
        let mut seen = BTreeSet::new();
        listing.unreadable.retain(|launch| seen.insert((launch.name.clone(), launch.home.clone())));
        let here = convoys.home().to_owned();
        if local {
            listing.records.retain(|record| record.home == here);
            listing.unreadable.retain(|launch| launch.home == here);
        }
        compose_listing(self.reads.as_ref(), &self.namespace, listing)
    }

    fn every(&self) -> Result<Vec<Composed>, String> {
        let convoys = self.convoys()?;
        let listing = convoys.every()?;
        convoys.memory().set_unreadable(listing.unreadable.len() as u64);
        compose_listing(self.reads.as_ref(), &self.namespace, listing)
    }

    /// The log's cursors, opened on first use; none where no log is held.
    fn log(&self) -> Option<&Mutex<LogCursors>> {
        self.log.get_or_init(|| self.open_cursors().map(Mutex::new)).as_ref()
    }

    /// Cursors over the log, each starting at the log's end: the heads are where all three stand.
    fn open_cursors(&self) -> Option<LogCursors> {
        let mut cursors = LogCursors {
            convoys: self.convoys().ok()?.cursor()?,
            vessels: self.reads.vessels().ok()?.cursor()?,
            terminals: self.reads.terminals().ok()?.cursor()?,
            heads: BTreeMap::new(),
        };
        cursors.heads = least([cursors.convoys.read_through(), cursors.vessels.read_through(), cursors.terminals.read_through()]);
        Some(cursors)
    }

    /// Where a read now starts from: the log read through each home's sequence, and the notes
    /// this home has changed. The notes are read first, so a change made while the log is read
    /// is named again, never missed.
    fn position_now(&self) -> Result<LogPosition, String> {
        let notes = self.convoys()?.memory().notes_read();
        let Some(log) = self.log() else { return Ok(LogPosition { homes: BTreeMap::new(), unlogged: Some(notes) }) };
        let mut log = log.lock().unwrap_or_else(PoisonError::into_inner);
        let heads = log.heads.clone();
        let homes = match log.written_since(&heads)? {
            Ok((_, through)) => through,
            Err(reason) => {
                // The heads have left the log, as a home caught up from a snapshot since they
                // were read leaves it: the position starts again where the log ends now. Its
                // start is no position, since such a home's log no longer holds it.
                let Some(reopened) = self.open_cursors() else { return Err(reason) };
                *log = reopened;
                log.heads.clone()
            }
        };
        log.advance(&homes);
        Ok(LogPosition { homes, unlogged: Some(notes) })
    }

    /// The launches whose views may have changed since `position`.
    fn changed_since_now(&self, position: &LogPosition) -> Result<Since, String> {
        let convoys = self.convoys()?;
        let notes = convoys.memory().notes_read();
        let mut names = match convoys.memory().noted_since(position.unlogged.as_ref()) {
            Ok(names) => names,
            Err(reason) => return Ok(Since::Expired(reason)),
        };
        let Some(log) = self.log() else { return Ok(Since::Expired("no log of the convoys is held here".to_owned())) };
        let mut log = log.lock().unwrap_or_else(PoisonError::into_inner);
        let (written, through) = match log.written_since(&position.homes)? {
            Ok(read) => read,
            Err(reason) => return Ok(Since::Expired(reason)),
        };
        log.advance(&through);
        drop(log);
        let (vessels, terminals) = (self.reads.vessels()?, self.reads.terminals()?);
        names.extend(written.convoys.into_iter().map(|(name, _)| name));
        let mut vessels_written = written.vessels;
        for (terminal, home) in written.terminals {
            if let Some(vessel) = terminals.get(&terminal, Some(&home))?.and_then(|terminal| terminal.vessel) {
                vessels_written.insert((vessel, home));
            }
        }
        for (vessel, home) in vessels_written {
            if let Some(vessel) = vessels.get(&vessel, Some(&home))? {
                names.insert(vessel.convoy);
            }
        }
        Ok(Since::Changed { names, through: LogPosition { homes: through, unlogged: Some(notes) } })
    }

    /// Every home's launch `name` composed, a retired one included where `retired`.
    async fn copies_of(&self, name: &str, retired: bool) -> Result<Vec<Record<Convoy>>, String> {
        let name = name.to_owned();
        let composed = self.blocking(move |views| views.named(&name)).await?;
        Ok(composed.into_iter().filter(|launch| retired || !launch.retired).map(record).collect())
    }
}

/// What was written to the log since a position, and each home's sequence it was read through.
type WrittenThrough = (Written, BTreeMap<String, u64>);

/// What was written to the log since a position, by kind.
struct Written {
    convoys: BTreeSet<(String, String)>,
    vessels: BTreeSet<(String, String)>,
    terminals: BTreeSet<(String, String)>,
}

impl LogCursors {
    /// The launches, Vessel launches and sessions written since each home's sequence in `at`, and
    /// each home's sequence all three were read through; `Err` holds why the log no longer holds
    /// them.
    fn written_since(&self, at: &BTreeMap<String, u64>) -> Result<Result<WrittenThrough, String>, String> {
        let mut throughs = Vec::with_capacity(3);
        let mut written = Vec::with_capacity(3);
        for since in [self.convoys.since(at)?, self.vessels.since(at)?, self.terminals.since(at)?] {
            match since {
                CursorSince::Changed { written: read, through } => {
                    written.push(read.into_iter().collect::<BTreeSet<_>>());
                    throughs.push(through);
                }
                CursorSince::Expired(reason) => return Ok(Err(reason)),
            }
        }
        let [convoys, vessels, terminals]: [BTreeSet<(String, String)>; 3] = written.try_into().expect("three cursors read");
        // Each cursor reads in its own transaction: the three are read through the least of each
        // home's sequence they read, so nothing past it is missed.
        Ok(Ok((Written { convoys, vessels, terminals }, least(throughs))))
    }

    /// Moves the heads to `through` where it is further.
    fn advance(&mut self, through: &BTreeMap<String, u64>) {
        for (home, seq) in through {
            let head = self.heads.entry(home.clone()).or_default();
            *head = (*head).max(*seq);
        }
    }
}

/// Each home's least sequence among the `throughs` that name it. The three cursors read each
/// home's one log, so a cursor that names no sequence of a home has read nothing of its kind
/// there; leaving the home out would read its log from the start, which a home caught up from a
/// snapshot no longer holds.
fn least(throughs: impl IntoIterator<Item = BTreeMap<String, u64>>) -> BTreeMap<String, u64> {
    let mut least = BTreeMap::<String, u64>::new();
    for (home, seq) in throughs.into_iter().flatten() {
        least.entry(home).and_modify(|held| *held = (*held).min(seq)).or_insert(seq);
    }
    least
}

/// What a launch read for a cover is read through.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Source {
    /// Every home's launch of the name.
    Named(String),
    /// Every home's open launch in the project, through the `open_by_project` index.
    OpenIn(String),
    /// Every home's launch in the project that has not retired, ended or not, through the leading
    /// column of the `by_launch` index.
    InProject(String),
    /// Every home's launch of a name in a project that has not retired, through the `by_launch`
    /// index.
    Launches(String, String),
    /// Every home's open launch of a role in a project, through the `open_by_role` index.
    OpenRole(String, String),
}

/// What one part of a cover selects, as the launches it may select are read.
#[derive(Clone, Debug)]
enum Term {
    /// The launches these name, each a name or a project's launches: a project's are read only
    /// when another part keeps the open ones alone.
    Sources(Vec<Part>),
    /// Only launches that have not ended: a live phase, or none yet.
    Live,
    /// Something no read here selects by.
    Unknown,
}

#[derive(Clone, Debug)]
enum Part {
    Named(String),
    /// Every launch in the project, ended or not.
    Project(String),
    /// The project's open launches.
    OpenIn(String),
    /// Every launch of the name, in a project another part names.
    LaunchOf(String),
    /// The launches of a name in a project.
    Launches(String, String),
    /// Every launch of the role, in a project another part names, ended or not.
    Role(String),
    /// The open launches of a role in a project.
    OpenRole(String, String),
}

/// The index keys of the project, launch-of and role labels, as the store's query index names them.
static PROJECT_KEY: std::sync::LazyLock<String> =
    std::sync::LazyLock::new(|| format!("label:{}", flotilla_resources::labels::PROJECT_LABEL));
static LAUNCH_OF_KEY: std::sync::LazyLock<String> =
    std::sync::LazyLock::new(|| format!("label:{}", flotilla_resources::labels::LAUNCH_OF_LABEL));
static ROLE_KEY: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| format!("label:{}", flotilla_resources::labels::ROLE_LABEL));

/// The phases of a launch that has not ended.
const LIVE_PHASES: [&str; 5] = ["Pending", "Active", "Interrupted", "Landing", "Anchored"];

/// The launches `cover` may select, as reads by key and of projects' open launches; `None` when
/// it selects by anything else, or by a project with no part keeping the open launches alone.
fn sources_of(cover: &IndexCover) -> Option<BTreeSet<Source>> {
    match term(cover) {
        Term::Sources(parts) => parts
            .into_iter()
            .map(|part| match part {
                Part::Named(name) => Some(Source::Named(name)),
                Part::OpenIn(project) => Some(Source::OpenIn(project)),
                Part::Launches(project, of) => Some(Source::Launches(project, of)),
                Part::OpenRole(project, role) => Some(Source::OpenRole(project, role)),
                Part::Project(project) => Some(Source::InProject(project)),
                Part::LaunchOf(_) | Part::Role(_) => None,
            })
            .collect(),
        Term::Live | Term::Unknown => None,
    }
}

fn term(cover: &IndexCover) -> Term {
    match cover {
        IndexCover::AnyKey(keys) => {
            let part = |(key, value): &(String, String)| match key.as_str() {
                "field:metadata.name" => Some(Some(Part::Named(value.clone()))),
                "field:spec.project_ref" => Some(Some(Part::Project(value.clone()))),
                key if key == PROJECT_KEY.as_str() => Some(Some(Part::Project(value.clone()))),
                key if key == LAUNCH_OF_KEY.as_str() => Some(Some(Part::LaunchOf(value.clone()))),
                "field:spec.role" => Some(Some(Part::Role(value.clone()))),
                key if key == ROLE_KEY.as_str() => Some(Some(Part::Role(value.clone()))),
                "field:status.phase" if LIVE_PHASES.contains(&value.as_str()) => Some(None),
                "missing:status.phase" => Some(None),
                _ => None,
            };
            // No key selects no launch.
            if keys.is_empty() {
                return Term::Sources(Vec::new());
            }
            let parts: Option<Vec<Option<Part>>> = keys.iter().map(part).collect();
            match parts {
                Some(parts) if !parts.is_empty() && parts.iter().all(Option::is_some) => {
                    Term::Sources(parts.into_iter().flatten().collect())
                }
                Some(parts) if !parts.is_empty() && parts.iter().all(Option::is_none) => Term::Live,
                _ => Term::Unknown,
            }
        }
        IndexCover::All(parts) => {
            let terms: Vec<Term> = parts.iter().map(term).collect();
            let live = terms.iter().any(|term| matches!(term, Term::Live));
            // One project and one name it is a launch of: the launch index reads them.
            let single = |pick: fn(&Part) -> Option<&String>| {
                let values: BTreeSet<&String> = terms
                    .iter()
                    .filter_map(|term| match term {
                        Term::Sources(parts) if parts.len() == 1 => pick(&parts[0]),
                        _ => None,
                    })
                    .collect();
                (values.len() == 1).then(|| values.into_iter().next().cloned()).flatten()
            };
            let project = single(|part| match part {
                Part::Project(project) => Some(project),
                _ => None,
            });
            let launch_of = single(|part| match part {
                Part::LaunchOf(of) => Some(of),
                _ => None,
            });
            let role = single(|part| match part {
                Part::Role(role) => Some(role),
                _ => None,
            });
            if let (Some(project), Some(of)) = (&project, launch_of) {
                return Term::Sources(vec![Part::Launches(project.clone(), of)]);
            }
            // One project and one role, open launches alone: the role index reads them.
            if let (Some(project), Some(role), true) = (project, role, live) {
                return Term::Sources(vec![Part::OpenRole(project, role)]);
            }
            // Any part's sources hold every launch all parts select; with a part keeping the open
            // launches alone, a project's are its open ones. A part naming launches drives first.
            let mut drivers: Vec<Vec<Part>> = terms
                .into_iter()
                .filter_map(|term| match term {
                    Term::Sources(parts) => Some(parts),
                    _ => None,
                })
                .collect();
            drivers.sort_by_key(|parts| {
                parts.iter().filter(|part| matches!(part, Part::Project(_) | Part::LaunchOf(_) | Part::Role(_))).count()
            });
            match drivers.into_iter().next() {
                Some(parts) if live => Term::Sources(parts.into_iter().map(open).collect()),
                Some(parts) => Term::Sources(parts),
                None if live => Term::Live,
                None => Term::Unknown,
            }
        }
        IndexCover::Any(parts) => {
            let terms: Vec<Term> = parts.iter().map(term).collect();
            if !terms.is_empty() && terms.iter().all(|term| matches!(term, Term::Live)) {
                return Term::Live;
            }
            let mut union = Vec::new();
            for term in terms {
                match term {
                    Term::Sources(parts) => union.extend(parts),
                    Term::Live | Term::Unknown => return Term::Unknown,
                }
            }
            Term::Sources(union)
        }
    }
}

/// A project's launches as its open ones, where another part keeps the open launches alone.
fn open(part: Part) -> Part {
    match part {
        Part::Project(project) => Part::OpenIn(project),
        part => part,
    }
}

impl std::fmt::Debug for ConvoyViews {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("ConvoyViews").field("namespace", &self.namespace).finish_non_exhaustive()
    }
}

/// `launch` as the engine port hands it out, at the version its view shows.
fn record(launch: Composed) -> Record<Convoy> {
    flotilla_schema::reads::converted("Convoy", 1);
    let version = launch.view.object.metadata.resource_version.parse().unwrap_or_default();
    crate::held_listing::view_record(launch.view.object, launch.home, version)
}

/// Each name's record among `records`, this home's where it holds one, else the lexically first
/// home's.
fn chosen(records: Vec<Record<Convoy>>, here: &str) -> BTreeMap<String, Record<Convoy>> {
    let mut chosen = BTreeMap::<String, Record<Convoy>>::new();
    for object in records {
        let keep = chosen.get(&object.name).is_some_and(|held| held.home == here || (object.home != here && held.home <= object.home));
        if !keep {
            chosen.insert(object.name.clone(), object);
        }
    }
    chosen
}

impl crate::EnginePort for ConvoyViews {
    fn install(self: Arc<Self>, backend: &crate::ResourceBackend) {
        backend.install_engine_kind::<Convoy>(self);
    }
}

#[async_trait::async_trait]
impl KindRecords<Convoy> for ConvoyViews {
    fn codes(&self) -> Vec<flotilla_resources::engine_records::KindCode> {
        use flotilla_resources::lifecycle_reads::{record_codes, RecordSet};
        record_codes(self.reads.as_ref(), &[RecordSet::Convoys, RecordSet::Vessels, RecordSet::Terminals])
    }

    /// From the followers of the launches, the Vessel launches and the sessions, and this home's
    /// notes.
    fn ticks(&self) -> flotilla_resources::engine_records::Ticks {
        flotilla_resources::engine_records::Ticks::Followed
    }

    /// This home's notes on its launches.
    fn unlogged_changes(&self) -> Option<u64> {
        Some(self.reads.convoys().map_or(0, |convoys| convoys.memory().changed()))
    }

    fn writes(&self) -> bool {
        false
    }

    fn home(&self) -> String {
        self.here()
    }

    /// This home's launch of the name where it has one, else another home's: the lexically first
    /// home's, as a listing chooses. A retired one reads with how it ended.
    async fn get(&self, name: &str) -> Result<Option<Record<Convoy>>, String> {
        let here = self.here();
        Ok(chosen(self.copies_of(name, true).await?, &here).remove(name))
    }

    /// Each launch, this home's where it holds one of the name, else the lexically first home's.
    async fn list(&self) -> Result<Vec<Record<Convoy>>, String> {
        let here = self.here();
        Ok(chosen(self.sources().await?, &here).into_values().collect())
    }

    async fn sources(&self) -> Result<Vec<Record<Convoy>>, String> {
        Ok(self.blocking(|views| views.every()).await?.into_iter().map(record).collect())
    }

    /// Every home's launch of the name, a retired one with how it ended.
    async fn copies(&self, name: &str) -> Result<Vec<Record<Convoy>>, String> {
        self.copies_of(name, true).await
    }

    /// The newest launch holding the role `labels` name, in the project they name or in none,
    /// a retired one with how it ended: a plain name's last resort. Labels other than those two
    /// select nothing here.
    async fn newest_labelled(&self, labels: &BTreeMap<String, String>) -> Option<Result<Option<Record<Convoy>>, String>> {
        use flotilla_resources::labels::{PROJECT_LABEL, ROLE_LABEL};
        let role = labels.get(ROLE_LABEL)?.clone();
        if labels.keys().any(|label| label != ROLE_LABEL && label != PROJECT_LABEL) {
            return None;
        }
        let project = labels.get(PROJECT_LABEL).cloned();
        Some(
            self.blocking(move |views| {
                let Some(newest) = views.convoys()?.newest_holding(&role, project.as_deref())? else { return Ok(None) };
                let composed = compose_named(views.reads.as_ref(), &views.namespace, &newest.name, Some(&newest.home))?;
                Ok(composed.into_iter().next().map(record))
            })
            .await,
        )
    }

    /// The launches of the name a listing holds: never a retired one.
    async fn sources_named(&self, name: &str) -> Result<Vec<Record<Convoy>>, String> {
        self.copies_of(name, false).await
    }

    /// The launches `cover` selects, where it selects by name, by project, by a name's launches
    /// in a project or by a role's open launches in one: those are read by key and through the
    /// kind's indexes, and only they are composed. `None` for any other cover, which a reader
    /// answers from every launch.
    async fn covered(&self, cover: &IndexCover, local: bool) -> Option<Result<Vec<Record<Convoy>>, String>> {
        let sources = sources_of(cover)?;
        Some(self.blocking(move |views| views.covering(&sources, local)).await.map(|composed| composed.into_iter().map(record).collect()))
    }

    async fn positioned(&self) -> Result<(Vec<Record<Convoy>>, LogPosition), String> {
        // The position before the listing: every change after it is named again.
        let (position, composed) = self.blocking(|views| Ok((views.position_now()?, views.every()?))).await?;
        Ok((composed.into_iter().map(record).collect(), position))
    }

    async fn position(&self) -> Result<LogPosition, String> {
        self.blocking(|views| views.position_now()).await
    }

    async fn changed_since(&self, position: &LogPosition) -> Result<Since, String> {
        let position = position.clone();
        self.blocking(move |views| views.changed_since_now(&position)).await
    }

    async fn apply(
        &self,
        name: &str,
        _version: Option<u64>,
        _labels: &BTreeMap<String, String>,
        _annotations: &BTreeMap<String, String>,
        _spec: &ConvoySpec,
    ) -> Result<Record<Convoy>, WriteError> {
        Err(format!("convoy {name} is admitted on the engine, not written").into())
    }

    async fn set_status(&self, name: &str, _version: u64, _status: &ConvoyStatus) -> Result<Record<Convoy>, WriteError> {
        Err(format!("convoy {name}'s status is derived from the engine's records; it is not written").into())
    }

    async fn delete(&self, name: &str) -> Result<(), String> {
        Err(format!("convoy {name} ends on the engine; it is not deleted"))
    }

    /// A tick at each change of a launch's record at any home, of what this home observes of one,
    /// or of a Vessel launch or a session at any home.
    async fn changes(&self) -> Result<BoxStream<'static, ()>, String> {
        let convoys = self.convoys()?;
        let vessels = self.reads.vessels()?;
        let terminals = self.reads.terminals()?;
        Ok(futures::stream::select_all([
            ticks(convoys.changes()),
            ticks(convoys.memory().subscribe()),
            ticks(vessels.changes()),
            ticks(terminals.changes()),
        ])
        .boxed())
    }
}
