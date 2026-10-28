//! The ManifestRoot port (#3824 layer 2c): every reader and writer of manifest roots goes through
//! [`ManifestRoots`]. With `[primitives]` off it is the old store's ManifestRoot resources, their
//! spec and status; with it on, the engine's ManifestRoot machine, whose records the 2026-10-26
//! ruling keeps (applied revision, baselines, refusals, source error) and whose phases are a
//! judgement.
//!
//! Callers see one shape, [`RootView`], in the old resource's vocabulary, so the reconciler, the
//! views and the CLI read either store alike. A root is written only at its home: the host that
//! declares it.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};

use crate::{CharterSource, DocumentKey, DocumentState, ManifestRootSpec, ManifestRootStatus, ResolutionAction};

/// Who declares a root, and what it reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootDeclaration {
    pub host: String,
    pub path: String,
    pub source: String,
    pub binding: Option<CharterSource>,
    /// A project's operational-entry source (an `ops-` root), rather than a host's manifest tree.
    pub ops: bool,
}

/// A root as one store holds it.
#[derive(Debug, Clone, PartialEq)]
pub struct RootView {
    pub name: String,
    /// Whether this daemon is the root's home, and so its only writer.
    pub local: bool,
    pub spec: ManifestRootSpec,
    /// The applied revision, the source error, each document's records and, at the root's home,
    /// its last pass's judgement; and the stall the records amount to.
    pub status: ManifestRootStatus,
}

impl RootView {
    /// The baseline the root recorded for `key`: the hash its reconciler last applied.
    pub fn baseline(&self, key: &DocumentKey) -> Option<&str> {
        self.status.documents.get(key).and_then(|document| document.baseline_hash.as_deref())
    }
}

/// One reconciliation pass, as its reconciler judged every document it reached.
#[derive(Debug, Clone, PartialEq)]
pub struct PassReport {
    /// The source revision the pass read.
    pub revision: String,
    /// Whether the pass reached every declared document, refused or not: only a complete pass
    /// advances the applied revision and drops the records of documents it did not reach.
    pub complete: bool,
    /// Each reached document's state: its phase, reason, hashes and resolution outcome. A document
    /// no longer declared is reached while its removal is reported (Deleted or Orphaned).
    pub documents: BTreeMap<DocumentKey, DocumentState>,
    /// When the pass ran.
    pub at: DateTime<Utc>,
}

/// The ManifestRoot port. Every write is refused unless this daemon is the root's home.
#[async_trait::async_trait]
pub trait ManifestRoots: Send + Sync {
    /// Root `name`, from whichever home holds it.
    async fn root(&self, namespace: &str, name: &str) -> Result<Option<RootView>, String>;
    /// Every root of every home.
    async fn roots(&self, namespace: &str) -> Result<Vec<RootView>, String>;
    /// Declares root `name` homed here, or brings its binding up to `declaration`; refused when a
    /// root of that name declares another host, path or source, and for a name containing `.`
    /// (the engine's generation separator; declared names are `manifest-<hex>` and `ops-<hex>`).
    async fn declare(&self, namespace: &str, name: &str, declaration: RootDeclaration) -> Result<RootView, String>;
    /// Ends root `name`: its source was removed. Its records stay for attribution; absent is done.
    async fn retire(&self, namespace: &str, name: &str) -> Result<(), String>;
    /// Records a pass: the reached documents' baselines and refusals, the applied revision of a
    /// complete pass, the settled resolutions, and the source error cleared.
    async fn record_pass(&self, namespace: &str, name: &str, pass: PassReport) -> Result<(), String>;
    /// Records the source's state: a load failure with `error`, or else `revision` loaded whole (a
    /// source with no documents of its own, such as a project's ops root).
    async fn record_source(&self, namespace: &str, name: &str, revision: Option<String>, error: Option<&str>) -> Result<(), String>;
    /// Claims `key`'s request `token` for the reconciler before it acts, so a restart never replays
    /// it. False when the request is no longer current or is claimed already.
    async fn claim_resolution(&self, namespace: &str, name: &str, key: &DocumentKey, token: &str) -> Result<bool, String>;
    /// Requests a resolution of `key`, under a new token, returned.
    async fn request_resolution(
        &self,
        namespace: &str,
        name: &str,
        key: &DocumentKey,
        action: ResolutionAction,
        requested_by: &str,
    ) -> Result<String, String>;
    /// Suspends `key`, or with `suspended` false resumes it.
    async fn set_suspended(&self, namespace: &str, name: &str, key: &DocumentKey, suspended: bool) -> Result<(), String>;
}

/// Whether `document` awaits an operator: refused, or orphaned (no longer declared, its resource
/// kept). Each leaves a refusal record with its reason, and together they make the root's stall.
pub fn awaits_operator(document: &DocumentState) -> bool {
    matches!(document.phase, crate::DocumentPhase::Refused | crate::DocumentPhase::Orphaned)
}

/// The documents `pass` refused or orphaned, with their reasons: the refusal records it leaves.
pub fn refusals(pass: &PassReport) -> BTreeMap<DocumentKey, String> {
    pass.documents
        .iter()
        .filter(|(_, document)| awaits_operator(document))
        .map(|(key, document)| (key.clone(), document.reason.clone().unwrap_or_else(|| "manifest document refused".to_owned())))
        .collect()
}

/// The documents `pass` judged no longer declared, with the revision that removed each: the
/// removal records it leaves.
pub fn removals(pass: &PassReport) -> BTreeMap<DocumentKey, String> {
    pass.documents.iter().filter_map(|(key, document)| Some((key.clone(), document.removed_in.clone()?))).collect()
}

/// The keys of `documents`, for the documents a pass reached.
pub fn reached(pass: &PassReport) -> BTreeSet<DocumentKey> {
    pass.documents.keys().cloned().collect()
}

/// The operator-rung stall of root `name`, with `evidence`, since `began_at`.
pub fn root_stall(name: &str, evidence: String, began_at: DateTime<Utc>) -> crate::StalledCondition {
    use crate::{ControllerRetry, LeafMaker, RetryCeiling, StallEvidenceSource, StallRung, StalledCondition};
    StalledCondition {
        leaves: Vec::new(),
        maker: Some(LeafMaker::Controller {
            resource_kind: "ManifestRoot".into(),
            name: Some(name.into()),
            retry: ControllerRetry::terminal(None, began_at, evidence.clone()),
            ceiling: RetryCeiling::default(),
        }),
        evidence,
        source: StallEvidenceSource::LeafEngine,
        cause: None,
        began_at,
        rung: StallRung::Operator,
        supervisor: None,
        supervision_message: None,
        supervision_index: None,
        supervision_exhausted: false,
        reason: None,
        proposed_disposition: None,
        nudge_history: Vec::new(),
    }
}

/// The evidence of a pass's refusals: each refused document's path and reason, joined.
pub fn refusal_evidence<'a>(refusals: impl IntoIterator<Item = (&'a DocumentKey, &'a str)>) -> Option<String> {
    let lines: Vec<_> = refusals.into_iter().map(|(key, reason)| format!("{}: {reason}", key.path)).collect();
    (!lines.is_empty()).then(|| lines.join("; "))
}

/// The status a source's load leaves: root-level refusals keep the last applied revision and the
/// documents; a load with no error records `revision` and clears the stall.
pub fn source_status(mut status: ManifestRootStatus, revision: Option<String>, error: Option<&str>, name: &str) -> ManifestRootStatus {
    status.source_error = error.map(str::to_string);
    if let Some(error) = error {
        if status.stalled.as_ref().is_none_or(|stall| stall.evidence != error) {
            status.stalled = Some(root_stall(name, error.into(), Utc::now()));
        }
    } else {
        status.applied_revision = revision;
        status.stalled = None;
    }
    status
}

/// The status a pass leaves over `previous`: its documents (and, for an incomplete pass, the
/// documents it did not reach, kept as they were: the ManifestRoot spec's rule), the applied
/// revision when complete, the source error cleared, and the stall its refusals amount to (kept
/// from `previous` while its evidence stands).
pub fn pass_status(previous: Option<&ManifestRootStatus>, pass: &PassReport, name: &str) -> ManifestRootStatus {
    let mut documents = pass.documents.clone();
    if !pass.complete {
        for (key, document) in previous.into_iter().flat_map(|status| &status.documents) {
            documents.entry(key.clone()).or_insert_with(|| document.clone());
        }
    }
    let refused: BTreeMap<_, _> = documents
        .iter()
        .filter(|(_, document)| awaits_operator(document))
        .map(|(key, document)| (key.clone(), document.reason.clone().unwrap_or_else(|| "manifest document refused".to_owned())))
        .collect();
    let evidence = refusal_evidence(refused.iter().map(|(key, reason)| (key, reason.as_str())));
    let stalled = evidence.map(|evidence| match previous.and_then(|status| status.stalled.as_ref()) {
        Some(stall) if stall.evidence == evidence => stall.clone(),
        _ => root_stall(name, evidence, pass.at),
    });
    ManifestRootStatus {
        applied_revision: if pass.complete {
            Some(pass.revision.clone())
        } else {
            previous.and_then(|status| status.applied_revision.clone())
        },
        source_error: None,
        documents,
        stalled,
    }
}
