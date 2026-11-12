use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use flotilla_protocol::{Leaf, LeafAddress};
use serde::{Deserialize, Serialize};

use super::{
    expected_change_request_leaves, expected_checkout_refs, instantiate_exit, Convoy, ConvoyPhase, CrewCompletionRefusalCause,
    InstantiatedExit, VesselRequirement, WorkflowSnapshot,
};
use crate::{
    address::leaf_record_name,
    checkout_records::CheckoutRecord,
    resource::ResourceObject,
    vessel::Vessel,
    workflow_template::{validate, visit_template_tokens, CrewSource, CrewSpec, ExitDeclaration},
    ChangeRequest, ChangeRequestLeafSubject, ForgeSpec, InputValue, ObservedChangeRequestState, RepositoryKey, ThreeValue,
    ENSURED_FROM_ANNOTATION,
};

pub fn is_ensured(convoy: &ResourceObject<Convoy>) -> bool {
    convoy.metadata.annotations.contains_key(ENSURED_FROM_ANNOTATION)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SettlementMode {
    NoExit,
    ClaimExit,
    WorldTerminal,
    ObservedDigest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum UnmetSettlementExpectation {
    ChangeRequestNotReady {
        record: String,
        detail: String,
    },
    SubjectDiscoveryPending {
        convoy: String,
        error: Option<String>,
    },
    InvalidExpectedCheckouts {
        message: String,
    },
    ExitEntryAwaitingBinding {
        disposition: String,
        subject: String,
    },
    MissingCheckout {
        checkout: String,
    },
    MissingCheckoutStatus {
        checkout: String,
    },
    CheckoutConditionFalse {
        checkout: String,
        condition: String,
    },
    CheckoutConditionUnknown {
        checkout: String,
        condition: String,
    },
    StaleCheckoutEvidence {
        checkout: String,
        condition: String,
        observed_at: Option<String>,
    },
    MissingChangeRequest {
        record: String,
    },
    StaleChangeRequest {
        record: String,
        observed_at: Option<DateTime<Utc>>,
    },
    ChangeRequestConditionFalse {
        record: String,
        value: Option<String>,
    },
    CompletionConditionUnsatisfied {
        subject: String,
        field_path: String,
        value: Option<String>,
        // Previous serialized evaluations omit causes; remove this compatibility
        // default one fleet roll after deployment (ADR 0047).
        #[serde(default)]
        causes: Vec<CrewCompletionRefusalCause>,
    },
    InvalidCondition {
        subject: String,
        message: String,
    },
    MissingObservedRef {
        reference: String,
    },
    StaleObservedRef {
        reference: String,
        observed_at: String,
    },
    ObservedDigestMismatch {
        reference: String,
        claimed: String,
        observed: String,
    },
}

/// Each repository of `convoy` whose change request landed on a target other than the one it
/// declared, as the landing evidence of its checkout in `checkouts` shows.
pub fn target_mismatches(convoy: &ResourceObject<Convoy>, checkouts: &BTreeMap<String, CheckoutRecord>) -> Vec<super::TargetMismatch> {
    convoy
        .spec
        .repositories
        .iter()
        .filter_map(|repository| {
            let evidence = checkouts
                .values()
                .find(|checkout| checkout.spec.repo_ref() == &repository.repo_ref)?
                .inspection()?
                .landed_evidence
                .as_ref()?;
            let observed_target_ref = evidence.target_ref.as_ref()?;
            (observed_target_ref != &repository.target_ref).then(|| super::TargetMismatch {
                repo_ref: repository.repo_ref.clone(),
                change_request_id: evidence.change_request.to_string(),
                declared_target_ref: repository.target_ref.clone(),
                observed_target_ref: observed_target_ref.clone(),
            })
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct SettlementEvaluation {
    /// ADR 0047: remove default one roll after introduction.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[builder(default)]
    pub promises: Vec<super::promises::Promise>,
    /// Exact instantiated exit-table subjects and their state at evaluation.
    /// ADR 0047: remove the decoder default one roll after deployment.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[builder(default)]
    pub subjects: Vec<SettlementSubject>,
    pub mode: SettlementMode,
    pub satisfied: bool,
    pub unmet: Vec<UnmetSettlementExpectation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettlementSubject {
    pub address: LeafAddress,
    pub state: Option<ObservedChangeRequestState>,
    pub observed_at: Option<DateTime<Utc>>,
}

pub struct LandingSettlement {
    pub evaluation: SettlementEvaluation,
    pub disposition: Option<String>,
}

/// Evaluate the exact Landing settlement condition and retain the evidence for
/// every branch that held it false. Reconciliation and diagnostics share this
/// function so an explanation cannot drift from the condition writer. Landed
/// preserves its recognised terminal outcome after the supporting records expire.
#[allow(clippy::too_many_arguments)]
pub fn evaluate_landing_settlement(
    convoy: &ResourceObject<Convoy>,
    checkouts: &BTreeMap<String, CheckoutRecord>,
    change_requests: &BTreeMap<String, ResourceObject<ChangeRequest>>,
    forges: &[ForgeSpec],
    change_request_stale_after: std::time::Duration,
    landing_evidence_stale_after: std::time::Duration,
    now: DateTime<Utc>,
) -> SettlementEvaluation {
    evaluate_landing_settlement_with_disposition(
        convoy,
        checkouts,
        change_requests,
        forges,
        change_request_stale_after,
        landing_evidence_stale_after,
        now,
    )
    .evaluation
}

#[allow(clippy::too_many_arguments)]
pub fn evaluate_landing_settlement_with_disposition(
    convoy: &ResourceObject<Convoy>,
    checkouts: &BTreeMap<String, CheckoutRecord>,
    change_requests: &BTreeMap<String, ResourceObject<ChangeRequest>>,
    forges: &[ForgeSpec],
    change_request_stale_after: std::time::Duration,
    landing_evidence_stale_after: std::time::Duration,
    now: DateTime<Utc>,
) -> LandingSettlement {
    let mut result = evaluate_exit_with_disposition(
        convoy,
        checkouts,
        change_requests,
        forges,
        change_request_stale_after,
        landing_evidence_stale_after,
        now,
    );
    if let Some(status) = convoy.status.as_ref().filter(|status| status.phase != ConvoyPhase::Landed) {
        result.evaluation.promises = status.promises.values().flat_map(BTreeMap::values).flatten().cloned().collect();
        if result.evaluation.promises.iter().any(|p| !p.state.terminal()) {
            result.evaluation.satisfied = false;
            result.disposition = None;
        }
    }
    result
}

#[allow(clippy::too_many_arguments)]
fn evaluate_exit_with_disposition(
    convoy: &ResourceObject<Convoy>,
    checkouts: &BTreeMap<String, CheckoutRecord>,
    change_requests: &BTreeMap<String, ResourceObject<ChangeRequest>>,
    forges: &[ForgeSpec],
    change_request_stale_after: std::time::Duration,
    landing_evidence_stale_after: std::time::Duration,
    now: DateTime<Utc>,
) -> LandingSettlement {
    // Landed materialises a recognised terminal outcome (ADR 0021). Its
    // evidence may subsequently be collected by the checkout authority or
    // change-request GC; consulting it again would turn settled history into
    // a missing-record hold. Legacy Landed records need no new receipt.
    if let Some(status) = convoy.status.as_ref().filter(|status| status.phase == ConvoyPhase::Landed) {
        if let Some(evaluation) = &status.landing_settlement {
            return LandingSettlement { evaluation: evaluation.clone(), disposition: status.disposition.clone() };
        }
        let mode = match status.disposition.as_deref() {
            Some("observed-digest") => SettlementMode::ObservedDigest,
            Some("claim") => SettlementMode::ClaimExit,
            // This is a diagnostic label for legacy or unrecognised dispositions;
            // Landed itself records the recognised terminal settlement.
            _ => SettlementMode::WorldTerminal,
        };
        return LandingSettlement {
            evaluation: SettlementEvaluation::builder().mode(mode).satisfied(true).unmet(Vec::new()).build(),
            disposition: status.disposition.clone(),
        };
    }
    let observed_digest = evaluate_observed_digest_anchor(convoy, checkouts, landing_evidence_stale_after, now);
    let expected = expected_checkout_refs(convoy, checkouts.keys().map(String::as_str));
    let unheld = unheld_declared_repositories(convoy, checkouts);
    let discovery_pending = !(expected.is_empty() && unheld.is_empty())
        && convoy.status.as_ref().is_some_and(|status| {
            status.branch_subject_scan_at.is_none()
                && status
                    .workflow_snapshot
                    .as_ref()
                    .and_then(|snapshot| snapshot.exit.as_ref())
                    .is_some_and(|exit| matches!(exit, ExitDeclaration::Table(_)))
                // A known subject lets the exit table decide settlement. Old
                // scan failures remain diagnostic, including stored pre-fix state.
                && expected_change_request_leaves(convoy, forges).is_ok_and(|leaves| leaves.is_empty())
        });
    if discovery_pending {
        return LandingSettlement {
            evaluation: SettlementEvaluation::builder()
                .mode(SettlementMode::WorldTerminal)
                .satisfied(false)
                .unmet(vec![UnmetSettlementExpectation::SubjectDiscoveryPending {
                    convoy: convoy.metadata.name.clone(),
                    error: convoy.status.as_ref().and_then(|status| status.branch_subject_scan_error.clone()),
                }])
                .build(),
            disposition: None,
        };
    }
    let exit = match instantiate_exit(convoy, forges) {
        Ok(exit) => exit,
        Err(message) => {
            return LandingSettlement {
                evaluation: SettlementEvaluation::builder()
                    .mode(SettlementMode::WorldTerminal)
                    .satisfied(false)
                    .unmet(vec![UnmetSettlementExpectation::InvalidCondition { subject: convoy.metadata.name.clone(), message }])
                    .build(),
                disposition: None,
            };
        }
    };
    let entries = match exit {
        InstantiatedExit::None => {
            if let Some(observed_digest) = observed_digest {
                return observed_digest;
            }
            return LandingSettlement {
                evaluation: SettlementEvaluation::builder().mode(SettlementMode::NoExit).satisfied(false).unmet(Vec::new()).build(),
                disposition: None,
            };
        }
        // A table with no bound subject, over no expected checkout, is the claim exit.
        InstantiatedExit::Table(entries)
            if !(expected.is_empty() && unheld.is_empty() && entries.iter().all(|entry| entry.leaves.is_empty())) =>
        {
            entries
        }
        InstantiatedExit::Claim | InstantiatedExit::Table(_) => {
            return LandingSettlement {
                evaluation: SettlementEvaluation::builder().mode(SettlementMode::ClaimExit).satisfied(true).unmet(Vec::new()).build(),
                disposition: Some("claim".to_string()),
            };
        }
    };

    let evaluate_terminal = |leaf: &Leaf| {
        if !matches!(leaf.address, LeafAddress::ChangeRequest { .. }) {
            return Err(UnmetSettlementExpectation::InvalidCondition {
                subject: convoy.metadata.name.clone(),
                message: "exit table leaf did not address a change request".to_string(),
            });
        }
        let name = leaf_record_name(&leaf.address, forges)
            .map_err(|message| UnmetSettlementExpectation::InvalidCondition { subject: leaf.address.to_string(), message })?;
        let subject = change_requests.get(&name).map(|change_request| ChangeRequestLeafSubject {
            change_request,
            now,
            stale_after: change_request_stale_after,
        });
        match crate::evaluate_leaf(leaf, subject.as_ref().map(|subject| subject as &dyn crate::LeafSubject), None) {
            Ok(evaluation) if evaluation.result == ThreeValue::True => Ok(()),
            Ok(evaluation) => match change_requests.get(&name) {
                None => Err(UnmetSettlementExpectation::MissingChangeRequest { record: name }),
                Some(record) => {
                    if subject.as_ref().is_none_or(|subject| !subject.is_fresh(".state")) {
                        let observed_at = record.status.as_ref().map(|status| status.state.observed_at);
                        Err(UnmetSettlementExpectation::StaleChangeRequest { record: name, observed_at })
                    } else {
                        Err(UnmetSettlementExpectation::ChangeRequestConditionFalse {
                            record: name,
                            value: evaluation.value.map(|value| value.to_string()),
                        })
                    }
                }
            },
            Err(message) => Err(UnmetSettlementExpectation::InvalidCondition { subject: name, message }),
        }
    };
    let mut table_unmet = Vec::new();
    let mut disposition = None;
    let mut addresses = Vec::new();
    for leaf in entries.iter().flat_map(|entry| &entry.leaves) {
        if !addresses.contains(&leaf.address) {
            addresses.push(leaf.address.clone());
        }
    }
    if addresses.is_empty() {
        // A checkout with landed evidence predates a bound change-request
        // record. The default merged entry remains its concrete exit.
        for entry in entries {
            if entry.template.path.segments() == ["state"]
                && entry.template.operator == flotilla_address::Operator::Equal
                && entry.template.literal == "merged"
            {
                disposition = Some(entry.disposition);
                break;
            }
            table_unmet
                .push(UnmetSettlementExpectation::ExitEntryAwaitingBinding { disposition: entry.disposition, subject: "$cr".to_string() });
        }
    } else {
        let mut highest_matched_entry = None;
        for address in &addresses {
            let mut failures = Vec::new();
            let mut matched = None;
            for (index, entry) in entries.iter().enumerate() {
                let Some(leaf) = entry.leaves.iter().find(|leaf| &leaf.address == address) else { continue };
                match evaluate_terminal(leaf) {
                    Ok(()) => {
                        matched = Some(index);
                        break;
                    }
                    Err(expectation) => failures.push(expectation),
                }
            }
            if let Some(index) = matched {
                highest_matched_entry = Some(highest_matched_entry.map_or(index, |highest: usize| highest.max(index)));
            } else {
                for expectation in failures {
                    if !table_unmet.contains(&expectation) {
                        table_unmet.push(expectation);
                    }
                }
            }
        }
        if table_unmet.is_empty() {
            disposition = highest_matched_entry.map(|index| entries[index].disposition.clone());
        }
    }

    let mut unmet = if disposition.is_some() { Vec::new() } else { table_unmet };
    // The exit table evaluates bound and observed change requests. A checkout whose inspection
    // was read is context, so its landed condition cannot block settlement. One never read (never
    // held, or held but never inspected, whether or not it has since ended) is not evidence of
    // anything and holds settlement; a held checkout's last reading stands after it ends.
    for name in expected {
        match checkouts.get(&name) {
            None => unmet.push(UnmetSettlementExpectation::MissingCheckout { checkout: name }),
            Some(checkout) if checkout.inspection().is_none() => {
                unmet.push(UnmetSettlementExpectation::MissingCheckoutStatus { checkout: name })
            }
            Some(_) => {}
        }
    }
    for repository in unheld {
        unmet.push(UnmetSettlementExpectation::MissingCheckout { checkout: repository.to_string() });
    }

    let subjects: Vec<_> = addresses
        .into_iter()
        .map(|address| {
            let record = match &address {
                LeafAddress::ChangeRequest { .. } => leaf_record_name(&address, forges).ok().and_then(|name| change_requests.get(&name)),
                _ => None,
            };
            let observation = record.and_then(|record| record.status.as_ref()).map(|status| &status.state);
            SettlementSubject {
                address,
                state: observation.and_then(|state| state.value),
                observed_at: observation.map(|state| state.observed_at),
            }
        })
        .collect();
    let satisfied = disposition.is_some() && unmet.is_empty();
    if !satisfied {
        if let Some(mut observed_digest) = observed_digest {
            observed_digest.evaluation.subjects = subjects;
            return observed_digest;
        }
    }
    LandingSettlement {
        evaluation: SettlementEvaluation::builder()
            .subjects(subjects)
            .mode(SettlementMode::WorldTerminal)
            .satisfied(satisfied)
            .unmet(unmet)
            .build(),
        disposition: satisfied.then_some(disposition).flatten(),
    }
}

/// The Done crews' claims whose head digests anchor settlement (mode
/// ObservedDigest), with or without a declared exit. Stall evidence shares
/// this predicate so a digest-anchored convoy is never read as standing.
pub fn observed_digest_claims(status: &super::ConvoyStatus) -> Vec<&crate::SettlementClaimEvidence> {
    status
        .crew_work
        .values()
        .flat_map(BTreeMap::values)
        .filter(|work| work.phase == crate::CrewWorkPhase::Done)
        .filter_map(|work| work.claim_evidence.as_ref())
        .collect()
}

fn evaluate_observed_digest_anchor(
    convoy: &ResourceObject<Convoy>,
    checkouts: &BTreeMap<String, CheckoutRecord>,
    stale_after: std::time::Duration,
    now: DateTime<Utc>,
) -> Option<LandingSettlement> {
    let claims = observed_digest_claims(convoy.status.as_ref()?);
    if claims.is_empty() {
        return None;
    }

    let mut unmet = Vec::new();
    for claim in claims {
        // A checkout's inspection reads its own branch's ref on its remote.
        let branch = claim.refs.head.strip_prefix("refs/heads/").unwrap_or(&claim.refs.head);
        // A checkout that has ended confirms nothing: it is no longer inspected.
        let observations = checkouts
            .values()
            .filter(|checkout| !checkout.ended() && checkout.branch() == branch)
            .filter_map(|checkout| {
                let integration = checkout.integration.as_ref()?;
                Some((integration.inspection.remote_digest.as_ref()?, integration))
            })
            .collect::<Vec<_>>();
        let Some((digest, integration)) = observations.into_iter().max_by_key(|(_, integration)| integration.confirmed_at) else {
            unmet.push(UnmetSettlementExpectation::MissingObservedRef { reference: claim.refs.head.clone() });
            continue;
        };
        if !integration.confirmed_within(now, stale_after) {
            unmet.push(UnmetSettlementExpectation::StaleObservedRef {
                reference: claim.refs.head.clone(),
                observed_at: integration.confirmed_at.to_rfc3339(),
            });
        } else if *digest != claim.claimed_head_digest {
            unmet.push(UnmetSettlementExpectation::ObservedDigestMismatch {
                reference: claim.refs.head.clone(),
                claimed: claim.claimed_head_digest.clone(),
                observed: digest.clone(),
            });
        }
    }
    let satisfied = unmet.is_empty();
    Some(LandingSettlement {
        evaluation: SettlementEvaluation::builder().mode(SettlementMode::ObservedDigest).satisfied(satisfied).unmet(unmet).build(),
        disposition: satisfied.then(|| "observed-digest".to_string()),
    })
}

/// The repositories `convoy`'s frozen vessels declare a checkout of (a vessel's own scope, or every
/// repository of the convoy) that neither an adopted ref nor a checkout in `held` stands for yet. A
/// checkout's name is chosen only when its vessel is placed, so until then the declaration is all
/// that says one is expected.
fn unheld_declared_repositories(convoy: &ResourceObject<Convoy>, held: &BTreeMap<String, CheckoutRecord>) -> BTreeSet<RepositoryKey> {
    let Some(snapshot) = convoy.status.as_ref().and_then(|status| status.workflow_snapshot.as_ref()) else { return BTreeSet::new() };
    let mut declared = BTreeSet::new();
    for vessel in &snapshot.vessels {
        match &vessel.repository_refs {
            Some(scope) => declared.extend(scope.iter().cloned()),
            None => declared.extend(convoy.spec.repositories.iter().map(|repository| repository.repo_ref.clone())),
        }
    }
    for repository in convoy.spec.adopted_checkout_refs.keys() {
        declared.remove(repository);
    }
    for checkout in held.values() {
        declared.remove(checkout.spec.repo_ref());
    }
    declared
}

/// The vessel serving `work`: its newest launch, by when it was requested. Each launch is read
/// under its own name, so a work replaced at another home has a launch at each.
pub fn work_vessel<'a>(vessels: &'a BTreeMap<String, ResourceObject<Vessel>>, work: &str) -> Option<&'a ResourceObject<Vessel>> {
    vessels.values().filter(|vessel| vessel.spec.vessel_name == work).max_by_key(|vessel| {
        let launch = vessel.metadata.name.rsplit_once('#').and_then(|(_, n)| n.parse::<u64>().ok()).unwrap_or(0);
        (vessel.metadata.creation_timestamp, launch)
    })
}

/// Why a workflow cannot be admitted as a convoy's frozen snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapshotRefusal {
    /// The template is invalid.
    Invalid(Vec<crate::workflow_template::ValidationError>),
    /// The convoy names no value for this input of the template.
    MissingInput(String),
}

/// The workflow snapshot a convoy named `name` in `namespace` with `spec` freezes from `template`:
/// its crews' prompts and commands interpolated with the convoy's inputs and name. Refused when
/// the template is invalid or an input it declares has no value.
pub fn admitted_snapshot(
    name: &str,
    namespace: &str,
    spec: &crate::ConvoySpec,
    template: &crate::workflow_template::WorkflowTemplateSpec,
) -> Result<WorkflowSnapshot, SnapshotRefusal> {
    validate(template).map_err(SnapshotRefusal::Invalid)?;
    if let Some(input) = template.inputs.iter().find(|input| !spec.inputs.contains_key(&input.name)) {
        return Err(SnapshotRefusal::MissingInput(input.name.clone()));
    }
    let names = Interpolation { name, namespace, inputs: &spec.inputs };
    Ok(WorkflowSnapshot {
        cascade: template.cascade.clone(),
        stall_nudges: template.stall_nudges.clone(),
        supervision: template.supervision.clone(),
        exit: template.exit.clone(),
        turn_delivery: template.turn_delivery.clone(),
        vessels: template
            .vessels
            .iter()
            .map(|vessel| VesselRequirement {
                name: vessel.name.clone(),
                depends_on: vessel.depends_on.clone(),
                repository_refs: vessel.repository_refs.clone(),
                credential_refs: vessel.credential_refs.clone(),
                credential_scopes: vessel.credential_scopes.clone(),
                credential_permissions: vessel.credential_permissions.clone(),
                crew: vessel.crew.iter().map(|member| instantiate_process(&names, member)).collect(),
                needs: vessel.needs.clone(),
            })
            .collect(),
    })
}

/// What a template's `{{ ... }}` tokens read: the convoy's name, namespace and inputs.
struct Interpolation<'a> {
    name: &'a str,
    namespace: &'a str,
    inputs: &'a BTreeMap<String, InputValue>,
}

fn instantiate_process(convoy: &Interpolation<'_>, process: &CrewSpec) -> CrewSpec {
    let mut process = process.clone();
    match &mut process.source {
        CrewSource::Agent { prompt, .. } => {
            if let Some(prompt) = prompt {
                *prompt = interpolate_template_text(convoy, prompt);
            }
        }
        CrewSource::Tool { command } => {
            *command = interpolate_template_text(convoy, command);
        }
    }
    process
}

fn interpolate_template_text(convoy: &Interpolation<'_>, text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut search_from = 0;
    visit_template_tokens(text, |token| {
        output.push_str(&text[search_from..token.open]);
        match token.end {
            Some(end) => {
                if let Some(value) = interpolation_value(convoy, token.text) {
                    output.push_str(&value);
                } else {
                    output.push_str(&text[token.open..end]);
                }
                search_from = end;
            }
            None => {
                output.push_str(&text[token.open..]);
                search_from = text.len();
            }
        }
    });
    output.push_str(&text[search_from..]);
    output
}

fn interpolation_value(convoy: &Interpolation<'_>, token: &str) -> Option<String> {
    let segments = token.split('.').collect::<Vec<_>>();
    match segments.as_slice() {
        ["inputs", input_name] => convoy.inputs.get(*input_name).map(input_value_string),
        ["workflow", "name"] => Some(convoy.name.to_owned()),
        ["workflow", "namespace"] => Some(convoy.namespace.to_owned()),
        _ => None,
    }
}

fn input_value_string(value: &InputValue) -> String {
    match value {
        InputValue::String(value) => value.clone(),
    }
}
