//! The import of ChangeRequest, Issue and ForgeRead onto the engine (ADR 0047).
//!
//! | Old copy | Imported |
//! |---|---|
//! | Every home's copies of one subject or read | one record, at the home that observes its source, from the copy readers pick today |
//! | Each observed value | its column; its `observed_at` is dropped (a judgement) |
//! | `observing_authority`, ForgeRead's `authority`, `attempted_at`, `observed_at`, `demanded_at` | dropped: the home is the observer |
//! | ChangeRequest `subject_of` | dropped and counted: the convoys hold their links |
//! | A copy with no status | dropped: it never observed anything |
//! | A ForgeRead idle past its retention, or of incremental changes | dropped |
//! | Deleted copies; labels, annotations and namespaces (#3212) | dropped |
//!
//! Hosts' old stores hold different copies of one subject, so a subject is written only at its
//! observer: every host's import reads the same declarations, names the same home, and the others
//! write nothing for it. A source no Project declares has no observer to import at, and its rows are
//! dropped; the observer that takes it up writes the record afresh.

use std::collections::BTreeMap;

use chrono::{DateTime, Duration, Utc};
use flotilla_engine_store::ImportTarget;
use flotilla_protocol::IssueSource;
use flotilla_resources::{select_change_request_sources, ChangeRequest, ForgeRead, ForgeReadRequest, Issue, ResourceObject};

use super::import::{created, ImportRefusal, ImportReport};
use crate::stored::forge_observation::{ChangeRequestRecord, ForgeReadRecord, IssueRecord};

/// How long a forge read with no demand is kept: the observer retires it after this.
pub const READ_RETENTION: Duration = Duration::seconds(3600);

/// One home's old copy of an observation: the object, and the home that wrote it.
pub struct ObservationCopy<R: flotilla_resources::Resource> {
    pub object: ResourceObject<R>,
    pub home: String,
}

/// The home that observes a source, when a declaration names one.
pub type ObserverOf<'a> = dyn Fn(&IssueSource) -> Option<String> + 'a;

/// The home `copy` is a candidate to import at: its source's observer, when `target` imports there.
/// A copy that cannot be imported (`unusable`) or is imported elsewhere is counted as dropped when
/// `target` imports its own home.
fn candidate<R: flotilla_resources::Resource>(
    kind: &'static str,
    copy: &ObservationCopy<R>,
    unusable: Option<&'static str>,
    observer: Option<String>,
    target: &impl ImportTarget,
    report: &mut ImportReport,
) -> Option<String> {
    let reason = if copy.object.metadata.deletion_timestamp.is_some() {
        Some("deleted")
    } else if unusable.is_some() {
        unusable
    } else if observer.is_none() {
        Some("no declared observer")
    } else if observer.as_deref() != Some(copy.home.as_str()) {
        Some("imported at its observer")
    } else {
        None
    };
    if let Some(reason) = reason.filter(|_| target.imports(&copy.home)) {
        *report.dropped.entry((kind, reason)).or_default() += 1;
    }
    match reason {
        None | Some("imported at its observer") => observer.filter(|home| target.imports(home)),
        Some(_) => None,
    }
}

/// Writes `record` as `name` at `home` and counts it, or names the refusal.
fn write<K: flotilla_schema::Kind + PartialEq + Clone, R: flotilla_resources::Resource>(
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

/// Imports every change request whose source's observer `target` imports, one record per subject,
/// from the copy `select_change_request_sources` picks among every home's copies; counts what each
/// home `target` imports did not write.
pub fn import_change_requests(
    copies: &[ObservationCopy<ChangeRequest>],
    observer_of: &ObserverOf<'_>,
    target: &mut impl ImportTarget,
    report: &mut ImportReport,
) {
    const KIND: &str = "ChangeRequest";
    let mut live = Vec::new();
    for copy in copies {
        let source = IssueSource { service: copy.object.spec.service.clone(), scope: copy.object.spec.scope.clone() };
        let unusable = copy.object.status.is_none().then_some("never observed");
        if let Some(home) = candidate(KIND, copy, unusable, observer_of(&source), target, report) {
            live.push((&copy.object, home));
        }
    }
    for (_, (object, home)) in select_change_request_sources(live) {
        let Some(status) = &object.status else { continue };
        let name = object.metadata.name.clone();
        let record = ChangeRequestRecord::from_old(&object.spec, status);
        if write(KIND, target, &home, &name, &object, &record, report) && !object.spec.subject_of.is_empty() {
            *report.dropped.entry((KIND, "convoy history")).or_default() += 1;
        }
    }
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
    let mut selected = BTreeMap::<(String, String, u64), (&ResourceObject<Issue>, String)>::new();
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
        let Some(home) = candidate(KIND, copy, unusable, observer_of(&source), target, report) else { continue };
        let key = (copy.object.spec.service.clone(), copy.object.spec.scope.clone(), copy.object.spec.number);
        match selected.get(&key) {
            Some((prior, _)) if rank(prior) >= rank(&copy.object) => {}
            _ => {
                selected.insert(key, (&copy.object, home));
            }
        }
    }
    for (object, home) in selected.into_values() {
        let Some(status) = &object.status else { continue };
        let name = object.metadata.name.clone();
        write(KIND, target, &home, &name, object, &IssueRecord::from_old(&object.spec, status), report);
    }
}

/// The last time anything demanded each forge read, by name: its creation's demand, or a later
/// heartbeat's renewal.
pub type Demands = BTreeMap<String, DateTime<Utc>>;

/// Imports every forge read whose source's observer `target` imports, one record per name, from the
/// latest result among every home's (its attempt time, then its authority). A read no one has
/// demanded within [`READ_RETENTION`] of `now`, and an incremental read, are dropped.
pub fn import_forge_reads(
    copies: &[ObservationCopy<ForgeRead>],
    renewed: &Demands,
    observer_of: &ObserverOf<'_>,
    now: DateTime<Utc>,
    target: &mut impl ImportTarget,
    report: &mut ImportReport,
) {
    const KIND: &str = "ForgeRead";
    let mut demanded = renewed.clone();
    for copy in copies {
        let at = demanded.entry(copy.object.metadata.name.clone()).or_insert(copy.object.spec.demanded_at);
        *at = (*at).max(copy.object.spec.demanded_at);
    }
    let rank = |object: &ResourceObject<ForgeRead>| object.status.as_ref().map(|status| (status.attempted_at, status.authority.clone()));
    let mut selected = BTreeMap::<String, (&ResourceObject<ForgeRead>, String)>::new();
    for copy in copies {
        let name = &copy.object.metadata.name;
        let unusable = if matches!(copy.object.spec.request, ForgeReadRequest::Changes { .. }) {
            Some("incremental")
        } else if demanded.get(name).is_some_and(|at| now.signed_duration_since(*at) > READ_RETENTION) {
            Some("idle")
        } else if !copy.object.status.as_ref().is_some_and(|status| status.value.is_some() || status.error.is_some()) {
            Some("never read")
        } else {
            None
        };
        let Some(home) = candidate(KIND, copy, unusable, observer_of(&copy.object.spec.source), target, report) else { continue };
        match selected.get(name) {
            Some((prior, _)) if rank(prior) >= rank(&copy.object) => {}
            _ => {
                selected.insert(name.clone(), (&copy.object, home));
            }
        }
    }
    for (name, (object, home)) in selected {
        let Some(status) = &object.status else { continue };
        write(KIND, target, &home, &name, object, &ForgeReadRecord::from_old(&object.spec.source, &object.spec.request, status), report);
    }
}
