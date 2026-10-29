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
mod lifecycle;
mod messages;
mod observations;
pub mod plan;
mod records;

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use flotilla_engine_store::{Import, ImportTarget};
use flotilla_provisioning::engine::import::ImportReport;
use flotilla_resources::{K8sResourceObject, Resource, ResourceObject};
use flotilla_store::REGISTERED_RESOURCE_KINDS;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use self::plan::Plan;

/// The engine store a daemon with `[primitives]` on opens.
pub const ENGINE_STORE: &str = "decided.sqlite";
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
    if let Some(error) = document.get(UNREADABLE).and_then(Value::as_str) {
        return Err(format!("its stored body is not JSON: {error}"));
    }
    let object = K8sResourceObject::<R>::deserialize(document).map_err(|error| error.to_string())?;
    ResourceObject::from_k8s_object(object).map_err(|error| error.to_string())
}

impl Source {
    pub fn new(rows: Vec<Row>) -> Self {
        Self { rows }
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
        let mut decoded = Vec::new();
        for row in self.of(plural, namespace) {
            match decode::<R>(&row.document) {
                Ok(object) => decoded.push(Decoded { namespace: row.namespace.clone(), object, home: row.home.clone() }),
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
    /// Each imported message's engine reference and inbox, by namespace and old record name.
    messages: BTreeMap<(String, String), (flotilla_state_machines::machines::message::MessageRef, flotilla_address::Address)>,
    /// The last admission sequence the imported messages took, by home.
    admitted: BTreeMap<String, u64>,
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
    Step { name: "artifacts", kinds: &["artifacts"], run: records::artifacts },
    Step { name: "forge observations", kinds: &observations::KINDS, run: observations::observations },
    Step { name: "accounts", kinds: &records::ACCOUNT_KINDS, run: records::accounts },
    Step { name: "messages", kinds: &["messages"], run: messages::messages },
    Step { name: "attentions", kinds: &["demands"], run: attentions::attentions },
    Step { name: "convoys", kinds: &["convoys"], run: convoys::convoys },
    Step { name: "lifecycles", kinds: &lifecycle::KINDS, run: lifecycle::lifecycles },
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

/// What an import did with the records homed at its store's home.
#[derive(Debug, Default)]
pub struct Summary {
    pub kinds: BTreeMap<String, Counts>,
    pub refusals: Vec<Refusal>,
    /// Imported, but on an assumption worth reading, or kept in the old store by design.
    pub notes: Vec<String>,
    /// Rows of kinds still on the old store, by kind: the layer that moves them, and the count.
    pub staying: BTreeMap<&'static str, (&'static str, usize)>,
}

impl Summary {
    fn absorb(&mut self, report: ImportReport) {
        for ((kind, _), count) in report.imported {
            self.kinds.entry(kind.to_owned()).or_default().imported += count;
        }
        for ((kind, _), count) in report.dropped {
            self.kinds.entry(kind.to_owned()).or_default().dropped += count;
        }
        for refusal in report.refused {
            self.refuse(refusal.kind, refusal.record, refusal.reason);
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
}

/// Counts the rows of kinds no importer reads, homed where `target` imports: a retired kind's are
/// dropped, a moving kind's stay on the old store, and a kind with no plan refuses its rows.
fn plan_rows(source: &Source, target: &impl ImportTarget, summary: &mut Summary) {
    for row in source.rows.iter().filter(|row| target.imports(&row.home)) {
        match plan::plan(&row.plural) {
            Some((_, Plan::Imported(_))) => {}
            Some((kind, Plan::Retired(_))) => summary.kinds.entry(kind.to_owned()).or_default().dropped += 1,
            Some((kind, Plan::Moving(layer))) => summary.staying.entry(kind).or_insert((layer, 0)).1 += 1,
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

/// Imports `source` into `target`: every record homed at the target's home, step by step.
pub fn import(source: &Source, target: &mut Import) -> Summary {
    let mut summary = Summary::default();
    plan_rows(source, target, &mut summary);
    let mut carried = Carried::default();
    for step in STEPS {
        let mut report = ImportReport::default();
        (step.run)(source, target, &mut carried, &mut report);
        summary.absorb(report);
    }
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
    pub refusals: Vec<Refusal>,
}

impl Report {
    pub fn new(summary: &Summary) -> Self {
        Self {
            build: flotilla_daemon_api::build_info::build_id().to_owned(),
            source: OLD_STORE.to_owned(),
            outcome: if summary.refused() { Outcome::Refused } else { Outcome::Imported },
            kinds: summary.kinds.clone(),
            refusals: summary.refusals.clone(),
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
    for (kind, (layer, rows)) in &summary.staying {
        tracing::info!(kind, rows, layer, "engine import: not imported yet, stays on the old store");
    }
    for note in &summary.notes {
        tracing::info!(%note, "engine import note");
    }
    for refusal in &summary.refusals {
        tracing::error!(kind = %refusal.kind, name = %refusal.name, reason = %refusal.reason, "engine import refused a record");
    }
    tracing::info!(report = %report.display(), refused = summary.refusals.len(), "engine import report written");
}

/// The cut-over import at a daemon's start with `[primitives]` on, homed at `home`: when
/// `state_dir` holds an old store and no engine store, imports the old store into a new engine
/// store, writes the report, and puts the engine store in place only if nothing was refused. A
/// refusal is an error, leaving `decided.sqlite.importing` for inspection; the next start imports
/// again from the beginning. Returns the report's path, or `None` when there was nothing to import.
/// The report is written before the rename, so a failed rename leaves an `imported` report beside
/// no engine store; the next start imports again and writes a new one.
pub fn first_start(state_dir: &Path, home: &str) -> Result<Option<PathBuf>, String> {
    let engine = state_dir.join(ENGINE_STORE);
    let old = state_dir.join(OLD_STORE);
    if engine.exists() || !old.exists() {
        return Ok(None);
    }
    let importing = state_dir.join(IMPORTING);
    remove_store(&importing)?;
    tracing::info!(source = %old.display(), target = %importing.display(), home, "engine import: first start with [primitives]");
    let source = Source::read(&old, home)?;
    let registry = crate::runtime::decided_registry().map_err(|error| format!("decided store registry: {error:?}"))?;
    let summary = {
        let mut target = Import::create(&importing, home, registry)?;
        import(&source, &mut target)
    };
    let report = write_report(state_dir, &Report::new(&summary))?;
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
