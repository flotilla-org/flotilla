//! The import of ChangeRequest and Issue onto the engine (ADR 0047).
//!
//! | Old copy | Imported |
//! |---|---|
//! | Every home's copies of one subject | one record, at the home that observes its source, from the copy readers pick today |
//! | Each observed value | its column; its `observed_at` is dropped (a judgement) |
//! | `observing_authority` | dropped: the home is the observer |
//! | ChangeRequest `subject_of` | dropped and counted: the convoys hold their links |
//! | A change request's old name | its subject's record name, the one its observer writes |
//! | Copies under two spellings of one source | one record, the other copy dropped and counted |
//! | A change request whose source no Forge resolves | dropped: it has no record name |
//! | A copy with no status | dropped: it never observed anything |
//! | Deleted copies; labels, annotations and namespaces (#3212) | dropped |
//!
//! Hosts' old stores hold different copies of one subject, so a subject is written only at its
//! observer: every host's import reads the same declarations, names the same home, and the others
//! write nothing for it. A source no Project declares has no observer to import at, and its rows are
//! dropped; the observer that takes it up writes the record afresh.

use std::collections::BTreeMap;

use flotilla_engine_store::ImportTarget;
use flotilla_protocol::IssueSource;
use flotilla_resources::{
    address::record_name, select_change_request_sources, ChangeRequest, ChangeRequestSpec, ForgeSpec, Issue, Resource, ResourceObject,
};
use flotilla_schema::Kind;

use super::import::{created, ImportRefusal, ImportReport, RowOutcome};
use crate::stored::forge_observation::{ChangeRequestRecord, IssueRecord};

/// One home's old copy of an observation: the object, and the home that wrote it.
pub struct ObservationCopy<R: Resource> {
    pub object: ResourceObject<R>,
    pub home: String,
}

/// The home that observes a source, when a declaration names one.
pub type ObserverOf<'a> = dyn Fn(&IssueSource) -> Option<String> + 'a;

/// Why a copy is not written at its own home.
enum Skip {
    Deleted,
    /// It holds nothing to import, for the reason named.
    Unusable(&'static str),
    NoObserver,
    /// Its source's observer is another home, which imports it.
    Elsewhere,
}

impl Skip {
    fn label(&self) -> &'static str {
        match self {
            Self::Deleted => "deleted",
            Self::Unusable(reason) => reason,
            Self::NoObserver => "no declared observer",
            Self::Elsewhere => "imported at its observer",
        }
    }
}

/// The home `copy` is a candidate to import at: `source`'s observer, when `target` imports there.
/// A copy that cannot be imported (`unusable`) or is imported elsewhere is counted as dropped when
/// `target` imports its own home; a source with no observer is also named in a note.
fn candidate<R: Resource>(
    kind: &'static str,
    copy: &ObservationCopy<R>,
    source: &IssueSource,
    unusable: Option<&'static str>,
    observer_of: &ObserverOf<'_>,
    target: &impl ImportTarget,
    report: &mut ImportReport,
) -> Option<String> {
    let observer = observer_of(source);
    let skip = if copy.object.metadata.deletion_timestamp.is_some() {
        Some(Skip::Deleted)
    } else if let Some(reason) = unusable {
        Some(Skip::Unusable(reason))
    } else if observer.is_none() {
        Some(Skip::NoObserver)
    } else if observer.as_deref() != Some(copy.home.as_str()) {
        Some(Skip::Elsewhere)
    } else {
        None
    };
    if let Some(skip) = skip.as_ref().filter(|_| target.imports(&copy.home)) {
        *report.dropped.entry((kind, skip.label())).or_default() += 1;
        report.rows(kind, if matches!(skip, Skip::Elsewhere) { RowOutcome::OtherHome } else { RowOutcome::Dropped(skip.label()) }, 1);
        if matches!(skip, Skip::NoObserver) {
            let note = format!("{kind} of {}/{} not imported: no Project declares its source", source.service, source.scope);
            if !report.notes.contains(&note) {
                report.notes.push(note);
            }
        }
    }
    match skip {
        None | Some(Skip::Elsewhere) => observer.filter(|home| target.imports(home)),
        Some(_) => None,
    }
}

/// The candidate copies of one kind, homed here or elsewhere, and how many of each were chosen:
/// one copy per subject is imported, and the rest are staler copies of it.
#[derive(Default)]
struct Candidates {
    here: usize,
    elsewhere: usize,
    chosen_here: usize,
    chosen_elsewhere: usize,
}

impl Candidates {
    fn read(&mut self, here: bool) {
        if here {
            self.here += 1;
        } else {
            self.elsewhere += 1;
        }
    }

    fn chose(&mut self, here: bool) {
        if here {
            self.chosen_here += 1;
        } else {
            self.chosen_elsewhere += 1;
        }
    }

    /// Counts the copies not chosen as dropped: a staler copy homed here is counted where it is
    /// homed, and one homed elsewhere only in the ledger.
    fn account(self, kind: &'static str, report: &mut ImportReport) {
        let (stale_here, stale_elsewhere) = (self.here - self.chosen_here, self.elsewhere - self.chosen_elsewhere);
        const STALER: &str = "a staler copy of an imported subject";
        if stale_here > 0 {
            report.drop_rows(kind, STALER, stale_here);
        }
        report.rows(kind, RowOutcome::CopyDropped(STALER), stale_elsewhere);
    }
}

/// Counts a chosen copy written, homed here or elsewhere, in the ledger.
fn imported(kind: &'static str, here: bool, report: &mut ImportReport) {
    report.rows(kind, if here { RowOutcome::Imported } else { RowOutcome::CopyImported }, 1);
}

/// Counts a chosen copy not written for `reason`: where it is homed when here, else in the ledger.
fn dropped_copy(kind: &'static str, here: bool, reason: &'static str, report: &mut ImportReport) {
    if here {
        report.drop_rows(kind, reason, 1);
    } else {
        report.rows(kind, RowOutcome::CopyDropped(reason), 1);
    }
}

/// Writes `record` as `name` at `home` and counts it, or names the refusal.
fn write<K: Kind + PartialEq + Clone, R: Resource>(
    kind: &'static str,
    target: &mut impl ImportTarget,
    home: &str,
    name: &str,
    from: &ResourceObject<R>,
    record: &K,
    report: &mut ImportReport,
) -> bool {
    match target.write(home, name, &created(from), record) {
        Ok(()) => {
            *report.imported.entry((kind, "stored")).or_default() += 1;
            true
        }
        Err(reason) => {
            report.refused.push(ImportRefusal { kind, record: name.to_owned(), reason: format!("at `{home}`: {reason}") });
            false
        }
    }
}

/// The record name the observer writes a change request's record under, when `forges` resolve its
/// source.
fn change_request_name(spec: &ChangeRequestSpec, forges: &[ForgeSpec]) -> Option<String> {
    spec.address(forges).ok().and_then(|address| record_name(&address))
}

/// Imports every change request whose source's observer `target` imports, one record per subject,
/// from the copy `select_change_request_sources` picks among every home's copies, under the record
/// name the observer writes it under; counts what each home `target` imports did not write.
pub fn import_change_requests(
    copies: &[ObservationCopy<ChangeRequest>],
    forges: &[ForgeSpec],
    observer_of: &ObserverOf<'_>,
    target: &mut impl ImportTarget,
    report: &mut ImportReport,
) {
    const KIND: &str = "ChangeRequest";
    const RESPELLED: &str = "another spelling of an imported subject";
    let mut live = Vec::new();
    let mut candidates = Candidates::default();
    for copy in copies {
        let source = IssueSource { service: copy.object.spec.service.clone(), scope: copy.object.spec.scope.clone() };
        let name = change_request_name(&copy.object.spec, forges);
        let unusable = if copy.object.status.is_none() { Some("never observed") } else { name.is_none().then_some("no record name") };
        if let (Some(home), Some(name)) = (candidate(KIND, copy, &source, unusable, observer_of, target, report), name) {
            let here = target.imports(&copy.home);
            candidates.read(here);
            live.push((&copy.object, (name, home, here)));
        }
    }
    // Two spellings of one source (a host and its forge id, say) are two subjects to the selection
    // but one record: the fresher is written, then the one already under that name, then the first
    // by namespace and name.
    let rank = |object: &ResourceObject<ChangeRequest>, name: &str| {
        (
            object.status.as_ref().map(|status| status.state.observed_at),
            object.spec.observing_authority.clone(),
            object.metadata.name == name,
            object.metadata.namespace.clone(),
            object.metadata.name.clone(),
        )
    };
    let mut named = BTreeMap::<String, (ResourceObject<ChangeRequest>, String, bool)>::new();
    for (_, (object, (name, home, here))) in select_change_request_sources(live) {
        candidates.chose(here);
        let chosen = match named.remove(&name) {
            Some(prior) if rank(&prior.0, &name) >= rank(&object, &name) => {
                dropped_copy(KIND, here, RESPELLED, report);
                prior
            }
            Some(prior) => {
                dropped_copy(KIND, prior.2, RESPELLED, report);
                (object, home, here)
            }
            None => (object, home, here),
        };
        named.insert(name, chosen);
    }
    for (name, (object, home, here)) in named {
        let Some(status) = &object.status else { continue };
        let record = ChangeRequestRecord::from_old(&object.spec, status);
        if write(KIND, target, &home, &name, &object, &record, report) {
            imported(KIND, here, report);
            if !object.spec.subject_of.is_empty() {
                *report.dropped.entry((KIND, "convoy history")).or_default() += 1;
            }
        }
    }
    candidates.account(KIND, report);
}

/// Imports every issue whose source's observer `target` imports, one record per subject, from the
/// freshest copy among every home's (its state's observation time, then its authority).
pub fn import_issues(
    copies: &[ObservationCopy<Issue>],
    observer_of: &ObserverOf<'_>,
    target: &mut impl ImportTarget,
    report: &mut ImportReport,
) {
    const KIND: &str = "Issue";
    let mut selected = BTreeMap::<(String, String, u64), (&ResourceObject<Issue>, String, bool)>::new();
    let mut candidates = Candidates::default();
    let rank = |object: &ResourceObject<Issue>| {
        (
            object.status.as_ref().map(|status| status.state.observed_at),
            object.spec.observing_authority.clone(),
            object.metadata.namespace.clone(),
            object.metadata.name.clone(),
        )
    };
    for copy in copies {
        let source = IssueSource { service: copy.object.spec.service.clone(), scope: copy.object.spec.scope.clone() };
        let unusable = copy.object.status.is_none().then_some("never observed");
        let Some(home) = candidate(KIND, copy, &source, unusable, observer_of, target, report) else { continue };
        let here = target.imports(&copy.home);
        candidates.read(here);
        let key = (copy.object.spec.service.clone(), copy.object.spec.scope.clone(), copy.object.spec.number);
        match selected.get(&key) {
            Some((prior, _, _)) if rank(prior) >= rank(&copy.object) => {}
            _ => {
                selected.insert(key, (&copy.object, home, here));
            }
        }
    }
    for (object, home, here) in selected.into_values() {
        candidates.chose(here);
        let Some(status) = &object.status else { continue };
        let name = object.metadata.name.clone();
        if write(KIND, target, &home, &name, object, &IssueRecord::from_old(&object.spec, status), report) {
            imported(KIND, here, report);
        }
    }
    candidates.account(KIND, report);
}
