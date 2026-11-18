//! The cut-over import: the first start with `[primitives]` on and no engine store fills the
//! engine store from the old one.
//!
//! [`first_start`] reads `resources.sqlite` read-only, imports into `decided.sqlite.importing`
//! beside it, and renames that to `decided.sqlite` only when nothing is refused. It runs before
//! the daemon serves or connects to any peer, and writes only records homed at this host: other
//! hosts' records reach the engine store from their home by replication. [`rehearse`] runs the
//! same import into a store in memory, for the pre-roll validation.
//!
//! [`STEPS`] lists the importers in the order they write, each with the old kinds it imports;
//! every kind [`plan::PLAN`] marks imported has exactly one.

mod attentions;
mod convoys;
mod definitions;
mod first_start;
mod lifecycle;
mod messages;
mod observations;
pub mod plan;
mod records;

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use flotilla_core::in_process::DEFAULT_PROVISIONING_NAMESPACE;
use flotilla_engine_store::{Import, ImportTarget};
use flotilla_provisioning::engine::import::{ImportReport, RowCounts};
use flotilla_resources::{
    convoy::engine::{ImportedLaunches, NoLaunch},
    K8sResourceObject, Resource, ResourceObject,
};
use flotilla_store::REGISTERED_RESOURCE_KINDS;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use self::plan::Plan;

/// The engine store every daemon opens.
pub use flotilla_core::in_process::decided::ENGINE_STORE;
/// The old store the import reads.
pub const OLD_STORE: &str = "resources.sqlite";
/// The engine store while the import fills it.
pub const IMPORTING: &str = "decided.sqlite.importing";

/// One record of the old store: its namespace, the plural of its kind, the home that wrote it,
/// and its stored document.
#[derive(Debug, Clone)]
pub struct Row {
    pub namespace: String,
    pub plural: String,
    pub home: String,
    pub document: Value,
}

/// The old store's records the import reads, own and replicated.
#[derive(Debug, Default)]
pub struct Source {
    rows: Vec<Row>,
    /// The host this daemon describes, which its Host records are homed with; None when unknown.
    own_host: Option<String>,
}

/// A row decoded as its old resource.
struct Decoded<R: Resource> {
    namespace: String,
    object: ResourceObject<R>,
    home: String,
}

fn record_name(document: &Value) -> String {
    document.pointer("/metadata/name").and_then(Value::as_str).unwrap_or("<unnamed>").to_owned()
}

/// Where a stored body that is not JSON keeps its parse error, so its importer refuses it by name.
const UNREADABLE: &str = "flotilla.work/unreadable-body";

fn decode<R: Resource>(document: &Value) -> Result<ResourceObject<R>, String> {
    let object = K8sResourceObject::<R>::deserialize(document).map_err(|error| error.to_string())?;
    ResourceObject::from_k8s_object(object).map_err(|error| error.to_string())
}

impl Source {
    pub fn new(rows: Vec<Row>) -> Self {
        Self { rows, own_host: None }
    }

    /// This source, read by the daemon describing `own_host`.
    pub fn with_own_host(self, own_host: Option<String>) -> Self {
        Self { own_host, ..self }
    }

    /// The records of the store at `path`, its own homed at `home`. The store is only read.
    pub fn read(path: &Path, home: &str) -> Result<Self, String> {
        let rows = flotilla_store_sqlite::read_stored_rows(path).map_err(|error| error.to_string())?;
        Ok(Self::new(
            rows.into_iter()
                .map(|row| {
                    let plural = REGISTERED_RESOURCE_KINDS
                        .iter()
                        .find(|entry| entry.kind == row.kind)
                        .map(|entry| entry.plural)
                        .or_else(|| {
                            flotilla_store::RETIRED_RESOURCE_KINDS
                                .iter()
                                .find(|retired| retired.kind == row.kind)
                                .map(|retired| retired.plural)
                        })
                        .map_or_else(|| row.kind.clone(), str::to_owned);
                    let document = serde_json::from_str(&row.body)
                        .unwrap_or_else(|error| serde_json::json!({"metadata": {"name": row.name}, UNREADABLE: error.to_string()}));
                    Row { namespace: row.namespace, plural, home: row.origin_root.unwrap_or_else(|| home.to_owned()), document }
                })
                .collect(),
        ))
    }

    /// The host id of the daemon that wrote this store, read from its records: the one Host
    /// homed at `home` that describes a daemon, if exactly one does.
    pub fn own_daemon_host(&self, home: &str) -> Option<String> {
        let mut daemons = self
            .rows
            .iter()
            .filter(|row| row.plural == "hosts" && row.home == home)
            .filter_map(|row| decode::<flotilla_resources::Host>(&row.document).ok())
            .filter(|host| host.spec.connection.is_daemon())
            .map(|host| host.metadata.name);
        let own = daemons.next();
        own.filter(|_| daemons.next().is_none())
    }

    fn namespaces(&self, plurals: &[&str]) -> BTreeSet<&str> {
        self.rows.iter().filter(|row| plurals.contains(&row.plural.as_str())).map(|row| row.namespace.as_str()).collect()
    }

    fn of<'a>(&'a self, plural: &'a str, namespace: Option<&'a str>) -> impl Iterator<Item = &'a Row> {
        self.rows.iter().filter(move |row| row.plural == plural && namespace.is_none_or(|namespace| row.namespace == namespace))
    }

    /// The rows of a kind the caller imports, every home's, decoded; one homed where `target`
    /// imports that does not decode is refused as `kind`. Another home's is left to that home's
    /// import and its pre-roll rehearsal.
    fn owned<R: Resource>(
        &self,
        plural: &str,
        namespace: Option<&str>,
        kind: &'static str,
        target: &impl ImportTarget,
        report: &mut ImportReport,
    ) -> Vec<Decoded<R>> {
        self.owned_as(plural, namespace, kind, target, report, decode::<R>)
            .into_iter()
            .map(|(namespace, object, home)| Decoded { namespace, object, home })
            .collect()
    }

    /// [`Self::owned`], each row decoded by `decode`: a retired kind's, by the type its importer keeps.
    fn owned_as<T>(
        &self,
        plural: &str,
        namespace: Option<&str>,
        kind: &'static str,
        target: &impl ImportTarget,
        report: &mut ImportReport,
        decode: impl Fn(&Value) -> Result<T, String>,
    ) -> Vec<(String, T, String)> {
        let mut decoded = Vec::new();
        for row in self.of(plural, namespace) {
            let object = match row.document.get(UNREADABLE).and_then(Value::as_str) {
                Some(error) => Err(format!("its stored body is not JSON: {error}")),
                None => decode(&row.document),
            };
            match object {
                Ok(object) => decoded.push((row.namespace.clone(), object, row.home.clone())),
                Err(error) if target.imports(&row.home) => report.refused.push(flotilla_provisioning::engine::import::ImportRefusal {
                    kind,
                    record: record_name(&row.document),
                    reason: format!("does not decode: {error}"),
                }),
                Err(_) => {}
            }
        }
        decoded
    }

    /// The rows of a kind another importer owns, every home's, that decode: what an importer reads
    /// beside its own kind.
    fn context<R: Resource>(&self, plural: &str, namespace: Option<&str>) -> Vec<Decoded<R>> {
        self.of(plural, namespace)
            .filter_map(|row| {
                decode::<R>(&row.document).ok().map(|object| Decoded { namespace: row.namespace.clone(), object, home: row.home.clone() })
            })
            .collect()
    }
}

/// What the import read beside each record, carried from one step to a later one.
#[derive(Default)]
struct Carried {
    /// The launch each old convoy record imports as, every home's: what a record naming the
    /// convoy names.
    convoy_launches: ImportedLaunches,
    /// Each imported message's engine reference and inbox, by namespace and old record name.
    messages: BTreeMap<(String, String), (flotilla_state_machines::machines::message::MessageRef, flotilla_address::Address)>,
    /// The last admission sequence the imported messages took, by home.
    admitted: BTreeMap<String, u64>,
    /// The launch each imported live old Convoy became, and its version as written, by home and
    /// old record name: the checkouts held for the old record are held for that launch, created
    /// under that version.
    convoys: BTreeMap<(String, String), (String, u64)>,
    /// Each imported convoy that ended, as ended, with its end's time, by home and old record
    /// name: once its dependents are imported, one nothing holds retires then. What was held for
    /// it is not held for its launch: nothing on the engine reacts to an imported end.
    ended: BTreeMap<
        (String, String),
        (flotilla_state_machines::Resource<flotilla_state_machines::machines::convoy::Convoy>, flotilla_schema::Timestamp),
    >,
    /// The ended convoys the import retired, by home and old record name.
    retired: BTreeSet<(String, String)>,
    /// The launch each imported TerminalSession became, by home and old name: a message delivered
    /// to the session names that launch's consumer.
    launches: BTreeMap<(String, String), String>,
    /// The launch each imported Vessel became, by home and old name: what was held for the
    /// vessel is held for that launch.
    vessels: flotilla_provisioning::engine::import::VesselLaunches,
}

impl Carried {
    /// The launch old convoy `convoy`, homed at `home`, imported as: what releases the records
    /// held for it.
    fn launch(&self, home: &str, convoy: &str) -> Option<String> {
        self.convoys.get(&(home.to_owned(), convoy.to_owned())).map(|(launch, _)| launch.clone())
    }
}

/// One importer: the old kinds it imports, by plural, and its writes.
pub struct Step {
    pub name: &'static str,
    pub kinds: &'static [&'static str],
    run: fn(&Source, &mut Import, &mut Carried, &mut ImportReport),
}

/// Every importer, in the order it writes: a record after the records it names. A kind moving
/// onto the engine adds its importer here and marks itself imported in [`plan::PLAN`].
pub const STEPS: &[Step] = &[
    Step { name: "manifest roots", kinds: &["manifestroots"], run: records::manifest_roots },
    Step { name: "definitions", kinds: &definitions::KINDS, run: definitions::definitions },
    Step { name: "repositories", kinds: &["repositories"], run: records::repositories },
    Step { name: "hosts", kinds: &["hosts"], run: records::hosts },
    Step { name: "artifacts", kinds: &["artifacts"], run: records::artifacts },
    Step { name: "forge observations", kinds: &observations::KINDS, run: observations::observations },
    Step { name: "accounts", kinds: &records::ACCOUNT_KINDS, run: records::accounts },
    Step { name: "vessels", kinds: &["vessels"], run: lifecycle::vessels },
    Step { name: "terminal sessions", kinds: &["terminalsessions"], run: lifecycle::terminal_sessions },
    Step { name: "messages", kinds: &["messages"], run: messages::messages },
    Step { name: "attentions", kinds: &["demands"], run: attentions::attentions },
    Step { name: "convoys", kinds: &["convoys"], run: convoys::convoys },
    // After the convoys: a placement snapshot is labelled with the launch that releases it.
    Step { name: "fulfilment kinds", kinds: &records::PLACEMENT_KINDS, run: records::placement },
    Step { name: "access deliveries", kinds: &["accessdeliveries"], run: records::access_deliveries },
    Step { name: "lifecycles", kinds: &lifecycle::KINDS, run: lifecycle::lifecycles },
    // Last: an ended convoy retires once every record that could hold it is imported.
    Step { name: "retirements", kinds: &[], run: convoys::retirements },
    // After every write: a launch made here next numbers above every imported one.
    Step { name: "launch numbers", kinds: &[], run: convoys::launch_numbers },
    // Writes nothing: counts what the first start would do to the history imported.
    Step { name: "first start", kinds: &[], run: first_start::first_start },
];

/// One kind's counts in the report.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
pub struct Counts {
    pub imported: usize,
    pub dropped: usize,
    pub refused: usize,
}

/// A refused record, named.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Refusal {
    pub kind: String,
    pub name: String,
    pub reason: String,
}

/// One old kind's rows, every home's, by what became of them. When none of them is refused,
/// imported, dropped and other-home add up to its rows; `unaccounted` is what they miss by.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
pub struct Rows {
    pub rows: usize,
    pub imported: usize,
    pub dropped: usize,
    /// Rows another home imports: its own records, and the copies held here of them.
    pub other_home: usize,
    pub unaccounted: i64,
    /// The dropped rows by the reason they were dropped for, adding up to `dropped`.
    pub drops: BTreeMap<String, usize>,
}

/// What an import did with the records homed at its store's home.
#[derive(Debug, Default)]
pub struct Summary {
    /// Records written and dropped, by the kind the importer counts them under.
    pub kinds: BTreeMap<String, Counts>,
    /// Every old row, by its old kind.
    pub rows: BTreeMap<String, Rows>,
    pub refusals: Vec<Refusal>,
    /// Imported, but on an assumption worth reading, or kept in the old store by design.
    pub notes: Vec<String>,
    /// Counts beside the rows, by kind and what they count: never a name.
    pub tallies: BTreeMap<String, BTreeMap<String, usize>>,
    ledger: BTreeMap<String, RowCounts>,
    /// The ledger's dropped rows by old kind and reason.
    drops: BTreeMap<String, BTreeMap<String, usize>>,
}

impl Summary {
    fn absorb(&mut self, report: ImportReport) {
        for (kind, counts) in report.rows {
            let ledger = self.ledger.entry(kind.to_owned()).or_default();
            ledger.imported += counts.imported;
            ledger.dropped += counts.dropped;
            ledger.other_home += counts.other_home;
            ledger.copies_imported += counts.copies_imported;
            ledger.copies_dropped += counts.copies_dropped;
        }
        for ((kind, reason), count) in report.row_drops {
            *self.drops.entry(kind.to_owned()).or_default().entry(reason.to_owned()).or_default() += count;
        }
        for ((kind, _), count) in report.imported {
            self.kinds.entry(kind.to_owned()).or_default().imported += count;
        }
        for ((kind, _), count) in report.dropped {
            self.kinds.entry(kind.to_owned()).or_default().dropped += count;
        }
        for refusal in report.refused {
            // A record naming a convoy that imports as no launch, counted by why: the work list.
            if let Some(class) = NoLaunch::class_in(&refusal.reason) {
                *self.tallies.entry(refusal.kind.to_owned()).or_default().entry(format!("refused: {class}")).or_default() += 1;
            }
            self.refuse(refusal.kind, refusal.record, refusal.reason);
        }
        for ((kind, what), count) in report.tallies {
            *self.tallies.entry(kind.to_owned()).or_default().entry(what).or_default() += count;
        }
        self.notes.extend(report.notes);
    }

    fn refuse(&mut self, kind: &str, name: String, reason: String) {
        self.kinds.entry(kind.to_owned()).or_default().refused += 1;
        self.refusals.push(Refusal { kind: kind.to_owned(), name, reason });
    }

    pub fn refused(&self) -> bool {
        !self.refusals.is_empty()
    }

    /// Accounts for every row of `source` by its old kind: a row homed where `target` imports is
    /// as its importer counted it, and one homed elsewhere is that home's unless the import
    /// consumed it here.
    fn account(&mut self, source: &Source, target: &impl ImportTarget) {
        let mut held = BTreeMap::<String, (usize, usize)>::new();
        for row in &source.rows {
            let kind = plan::plan(&row.plural).map_or_else(|| row.plural.clone(), |(kind, _)| kind.to_owned());
            let (here, elsewhere) = held.entry(kind).or_default();
            if target.imports(&row.home) {
                *here += 1;
            } else {
                *elsewhere += 1;
            }
        }
        for (kind, (here, elsewhere)) in held {
            let ledger = self.ledger.get(&kind).copied().unwrap_or_default();
            let consumed = ledger.copies_imported + ledger.copies_dropped;
            let rows = Rows {
                rows: here + elsewhere,
                imported: ledger.imported + ledger.copies_imported,
                dropped: ledger.dropped + ledger.copies_dropped,
                other_home: ledger.other_home + elsewhere.saturating_sub(consumed),
                unaccounted: 0,
                drops: self.drops.get(&kind).cloned().unwrap_or_default(),
            };
            let carried = rows.imported + rows.dropped + rows.other_home;
            self.rows.insert(kind, Rows { unaccounted: rows.rows as i64 - carried as i64, ..rows });
        }
        for kind in self.ledger.keys().filter(|kind| !self.rows.contains_key(*kind)) {
            self.notes.push(format!("the ledger counts rows of `{kind}`, which the old store does not hold"));
        }
    }
}

/// Counts the rows of kinds no importer reads, homed where `target` imports: a retired kind's are
/// dropped, and a kind with no plan refuses its rows.
fn plan_rows(source: &Source, target: &impl ImportTarget, summary: &mut Summary) {
    for row in source.rows.iter().filter(|row| target.imports(&row.home)) {
        match plan::plan(&row.plural) {
            Some((_, Plan::Imported(_))) => {}
            Some((kind, Plan::Retired(ruling))) => {
                summary.kinds.entry(kind.to_owned()).or_default().dropped += 1;
                summary.ledger.entry(kind.to_owned()).or_default().dropped += 1;
                *summary.drops.entry(kind.to_owned()).or_default().entry(format!("retired: {ruling}")).or_default() += 1;
            }
            None => {
                let unreadable =
                    row.document.get(UNREADABLE).and_then(Value::as_str).map(|error| format!("; its stored body is not JSON: {error}"));
                summary.refuse(
                    &row.plural,
                    record_name(&row.document),
                    format!("no import plan for its kind{}", unreadable.unwrap_or_default()),
                )
            }
        }
    }
}

/// Refuses each namespace other than the engine's that the old store holds rows in, every home's,
/// naming its row count by kind. The engine has one namespace and identifies a record by kind,
/// name and home, so a namespace's records would collide with the engine's same-named ones.
fn namespace_rows(source: &Source, summary: &mut Summary) {
    let mut outside = BTreeMap::<&str, BTreeMap<&str, usize>>::new();
    for row in source.rows.iter().filter(|row| row.namespace != DEFAULT_PROVISIONING_NAMESPACE) {
        *outside.entry(row.namespace.as_str()).or_default().entry(row.plural.as_str()).or_default() += 1;
    }
    for (namespace, kinds) in outside {
        let total: usize = kinds.values().sum();
        let counts = kinds.iter().map(|(plural, count)| format!("{count} {plural}")).collect::<Vec<_>>().join(", ");
        summary.refuse(
            "namespace",
            namespace.to_owned(),
            format!(
                "{total} {} outside the engine's namespace `{DEFAULT_PROVISIONING_NAMESPACE}` ({counts}); the engine has one \
                 namespace, so they would collide with its records of the same names",
                if total == 1 { "record" } else { "records" }
            ),
        );
    }
}

/// Imports `source` into `target`: every record homed at the target's home, step by step.
pub fn import(source: &Source, target: &mut Import) -> Summary {
    let mut summary = Summary::default();
    namespace_rows(source, &mut summary);
    plan_rows(source, target, &mut summary);
    let convoys = source.context::<flotilla_resources::Convoy>("convoys", None);
    let launches = ImportedLaunches::number(convoys.iter().map(|convoy| (convoy.home.as_str(), &convoy.object)));
    let mut carried = Carried { convoy_launches: launches, ..Carried::default() };
    for step in STEPS {
        let mut report = ImportReport::default();
        (step.run)(source, target, &mut carried, &mut report);
        summary.absorb(report);
    }
    summary.account(source, target);
    summary
}

/// [`import`] into a store in memory homed at `home`: what an import there would write.
pub fn rehearse(source: &Source, home: &str) -> Summary {
    let mut target = match crate::runtime::decided_registry()
        .map_err(|error| format!("decided store registry: {error:?}"))
        .and_then(|registry| Import::in_memory(home, registry))
    {
        Ok(target) => target,
        Err(reason) => {
            let mut summary = Summary::default();
            summary.refuse("engine store", "<all>".to_owned(), reason);
            return summary;
        }
    };
    import(source, &mut target)
}

/// Whether an import ended with every record imported.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    Imported,
    Refused,
}

/// The import's report, as written beside the stores.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Report {
    pub build: String,
    pub source: String,
    pub outcome: Outcome,
    pub kinds: BTreeMap<String, Counts>,
    /// Every old row, by its old kind.
    pub rows: BTreeMap<String, Rows>,
    pub refusals: Vec<Refusal>,
    /// Counts beside the rows, by kind and what they count.
    pub tallies: BTreeMap<String, BTreeMap<String, usize>>,
    /// The home sequence of the import's last write: every log entry after it is the daemon's own.
    pub last_sequence: u64,
}

impl Report {
    pub fn new(summary: &Summary, last_sequence: u64) -> Self {
        Self {
            build: flotilla_daemon_api::build_info::build_id().to_owned(),
            source: OLD_STORE.to_owned(),
            outcome: if summary.refused() { Outcome::Refused } else { Outcome::Imported },
            kinds: summary.kinds.clone(),
            rows: summary.rows.clone(),
            last_sequence,
            refusals: summary.refusals.clone(),
            tallies: summary.tallies.clone(),
        }
    }
}

fn remove_store(path: &Path) -> Result<(), String> {
    for suffix in ["", "-journal", "-wal", "-shm"] {
        let file = PathBuf::from(format!("{}{suffix}", path.display()));
        match std::fs::remove_file(&file) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("remove {}: {error}", file.display())),
        }
    }
    Ok(())
}

fn write_report(state_dir: &Path, report: &Report) -> Result<PathBuf, String> {
    let path = state_dir.join(format!("import-{}.json", chrono::Utc::now().format("%Y%m%dT%H%M%S%.6fZ")));
    let body = serde_json::to_vec_pretty(report).map_err(|error| error.to_string())?;
    let write = |path: &Path| {
        use std::io::Write;
        std::fs::OpenOptions::new().write(true).create_new(true).open(path).and_then(|mut file| file.write_all(&body))
    };
    write(&path).map_err(|error| format!("write {}: {error}", path.display()))?;
    Ok(path)
}

fn log_summary(summary: &Summary, report: &Path) {
    for (kind, counts) in &summary.kinds {
        tracing::info!(kind, imported = counts.imported, dropped = counts.dropped, refused = counts.refused, "engine import");
    }
    for (kind, tallies) in &summary.tallies {
        for (what, count) in tallies {
            tracing::info!(kind, what, count, "engine import tally");
        }
    }
    for note in &summary.notes {
        tracing::info!(%note, "engine import note");
    }
    for refusal in &summary.refusals {
        tracing::error!(kind = %refusal.kind, name = %refusal.name, reason = %refusal.reason, "engine import refused a record");
    }
    tracing::info!(report = %report.display(), refused = summary.refusals.len(), "engine import report written");
}

/// The cut-over import at a daemon's start with `[primitives]` on, homed at `home` and describing
/// `own_host`: when
/// `state_dir` holds an old store and no engine store, imports the old store into a new engine
/// store, writes the report, and puts the engine store in place only if nothing was refused. A
/// refusal is an error, leaving `decided.sqlite.importing` for inspection; the next start imports
/// again from the beginning. Returns the report's path, or `None` when there was nothing to import.
/// The report is written before the rename, so a failed rename leaves an `imported` report beside
/// no engine store; the next start imports again and writes a new one.
pub fn first_start(state_dir: &Path, home: &str, own_host: Option<&str>) -> Result<Option<PathBuf>, String> {
    let engine = state_dir.join(ENGINE_STORE);
    let old = state_dir.join(OLD_STORE);
    if engine.exists() || !old.exists() {
        return Ok(None);
    }
    let importing = state_dir.join(IMPORTING);
    remove_store(&importing)?;
    tracing::info!(source = %old.display(), target = %importing.display(), home, "engine import: first start with [primitives]");
    let source = Source::read(&old, home)?.with_own_host(own_host.map(str::to_owned));
    let registry = crate::runtime::decided_registry().map_err(|error| format!("decided store registry: {error:?}"))?;
    let (summary, last_sequence) = {
        let mut target = Import::create(&importing, home, registry)?;
        let summary = import(&source, &mut target);
        let last = target.store().read().and_then(|mut read| read.last_sequence(home)).map_err(|error| error.to_string())?;
        (summary, last)
    };
    let report = write_report(state_dir, &Report::new(&summary, last_sequence))?;
    log_summary(&summary, &report);
    if summary.refused() {
        return Err(format!(
            "engine import refused {} records: {} is kept for inspection and the report is {}",
            summary.refusals.len(),
            importing.display(),
            report.display()
        ));
    }
    std::fs::rename(&importing, &engine).map_err(|error| format!("rename {} to {}: {error}", importing.display(), engine.display()))?;
    // A crash before this sync can lose the rename; the next start then imports again from the old store, untouched.
    #[cfg(unix)]
    std::fs::File::open(state_dir).and_then(|dir| dir.sync_all()).map_err(|error| format!("sync {}: {error}", state_dir.display()))?;
    Ok(Some(report))
}

#[cfg(test)]
mod tests;
