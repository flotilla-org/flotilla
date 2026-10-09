use std::collections::BTreeMap;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use flotilla_protocol::{Leaf, LeafAddress};
use flotilla_resources::{
    evaluate_leaf, expected_change_request_leaves, external_patches, select_convoy_children, Artifact, ArtifactLeafSubject, ChangeRequest,
    Checkout, Convoy, ConvoyAttention, ConvoyPhase, HoldAct, Issue, ObservedChecks, ResourceObject, StatusPatch, TerminalSession,
    TerminalSessionPhase, TerminalSessionSource, ThreeValue, TurnDeliveryEpisode, TurnDeliveryOutcome, TurnDeliveryRule, CONVOY_LABEL,
    ROLE_LABEL, VESSEL_LABEL,
};

use super::stalls::{is_active_change_request_probe, is_conflict_probe, is_merged_settlement_probe};
use super::{CrewTurnAdmission, CrewTurnIntent, LeafSubscriptionTable, TurnDeliveryActuator};

/// Immutable producer key shared by escalation publication and pre-replication replies.
pub(crate) fn turn_message_producer_key(source: &str, subject_revision: &str) -> String {
    format!("turn-delivery:{source}:{subject_revision}")
}
#[derive(Debug)]
pub(super) struct DeliveryError {
    pub(super) reason: String,
    kind: flotilla_resources::TurnDeliveryFailureKind,
}
impl From<String> for DeliveryError {
    fn from(reason: String) -> Self {
        Self { reason, kind: flotilla_resources::TurnDeliveryFailureKind::Transient }
    }
}
impl DeliveryError {
    pub(super) fn permanent(reason: impl Into<String>) -> Self {
        Self { reason: reason.into(), kind: flotilla_resources::TurnDeliveryFailureKind::Permanent }
    }
}

pub(super) struct UnavailableTurnDeliveryActuator;

#[async_trait]
impl TurnDeliveryActuator for UnavailableTurnDeliveryActuator {
    async fn deliver(&self, _request: &CrewTurnIntent) -> Result<CrewTurnAdmission, String> {
        Err("turn-delivery actuator unavailable".to_string())
    }

    async fn hold(&self, _request: &CrewTurnIntent, _act: &HoldAct, _reason: &str) -> Result<(), String> {
        Err("turn-delivery hold actuator unavailable".to_string())
    }
}

impl LeafSubscriptionTable {
    pub(super) async fn record_delivery_failure(
        &self,
        namespace: &str,
        convoy: &str,
        source: &str,
        error: &DeliveryError,
    ) -> Result<(), String> {
        let convoys = self.inner.backend.clone().using::<Convoy>(namespace);
        let current = convoys.get(convoy).await.map_err(|error| error.to_string())?;
        let prior =
            current.status.as_ref().and_then(|status| status.turn_deliveries.get(source)).and_then(|delivery| delivery.failure.as_ref());
        let changed = prior.is_none_or(|prior| prior.reason != error.reason || prior.kind != error.kind);
        let attempts = prior.map_or(1, |prior| prior.attempts.saturating_add(1));
        let delay = 5_i64.saturating_mul(1_i64 << attempts.saturating_sub(1).min(6)).min(300);
        let now = Utc::now();
        flotilla_resources::apply_status_patch(
            &convoys,
            convoy,
            &flotilla_resources::ConvoyStatusPatch::FailTurnDelivery {
                source: source.to_string(),
                failure: flotilla_resources::TurnDeliveryFailure::builder()
                    .reason(error.reason.clone())
                    .failed_at(now)
                    .kind(error.kind)
                    .attempts(attempts)
                    .retry_at(now + chrono::Duration::seconds(delay))
                    .build(),
            },
        )
        .await
        .map_err(|error| error.to_string())?;
        if changed {
            tracing::warn!(%convoy, %source, error = %error.reason, failure_kind = ?error.kind, attempts, retry_seconds = delay, "turn delivery failed");
        }
        Ok(())
    }

    pub(super) async fn deliver_turn(
        &self,
        subscription_id: uuid::Uuid,
        convoy_name: &str,
        source: &str,
        rule: &TurnDeliveryRule,
        leaf: &Leaf,
    ) -> Result<(), DeliveryError> {
        let namespace = self
            .inner
            .rows
            .lock()
            .await
            .get(&subscription_id)
            .map(|row| row.namespace.clone())
            .ok_or_else(|| "turn-delivery subscription disappeared".to_string())?;
        let convoys = self.inner.backend.clone().using::<Convoy>(&namespace);
        let convoy = convoys.get(convoy_name).await.map_err(|error| error.to_string())?;
        let status = convoy.status.as_ref().ok_or_else(|| format!("convoy `{convoy_name}` has no status"))?;
        if status
            .turn_deliveries
            .get(source)
            .and_then(|delivery| delivery.failure.as_ref())
            .is_some_and(|failure| failure.kind == flotilla_resources::TurnDeliveryFailureKind::Permanent || Utc::now() < failure.retry_at)
        {
            return Ok(());
        }
        // A stale firing after settlement has no work left to deliver.
        if status.phase.is_terminal() {
            return Ok(());
        }
        if status.phase == ConvoyPhase::Pending {
            return Err(DeliveryError::from("wrong convoy kind or phase for turn delivery".to_string()));
        }
        if !status.crew_work.get(&rule.to.vessel).is_some_and(|crew| crew.contains_key(&rule.to.role)) {
            return Err(DeliveryError::from("turn-delivery target crew is absent".to_string()));
        }
        let claim = status.crew_work.get(&rule.to.vessel).and_then(|crew| crew.get(&rule.to.role));
        let claim_at = claim.and_then(|claim| claim.finished_at);
        let active_probe = is_active_change_request_probe(status, rule, leaf);
        // A cached row can fire after its target stalls or settles, before the
        // reconciler removes it. Judge eligibility against the current status.
        if (status.phase == ConvoyPhase::Active || is_merged_settlement_probe(leaf)) && !active_probe {
            return Ok(());
        }
        let active_conflict = active_probe && is_conflict_probe(leaf);
        let (subject_revision, evidence_at, brief, message_subject) = match &leaf.address {
            LeafAddress::ChangeRequest { service, scope, number } => {
                let record_name = flotilla_resources::change_request_record_name(service, scope, *number);
                let record = self
                    .inner
                    .backend
                    .including_replicas::<ChangeRequest>(&namespace)
                    .get(&record_name)
                    .await
                    .map_err(|error| error.to_string())?;
                let cr =
                    record.object.status.as_ref().ok_or_else(|| format!("change-request observation `{record_name}` has no status"))?;
                let merged_settlement = is_merged_settlement_probe(leaf);
                // Once the world is terminal, active CI/review probes cannot add
                // competing reminders beside the explicit settlement delivery.
                if active_probe && !merged_settlement && cr.state.value == Some(flotilla_resources::ObservedChangeRequestState::Merged) {
                    return Ok(());
                }
                // The fallback is agent-facing firing context only; a merged
                // episode is identified by the PR address below, never its head.
                let head_sha = if merged_settlement {
                    cr.head_sha.value.clone().unwrap_or_else(|| "unknown".into())
                } else {
                    cr.head_sha.value.clone().ok_or_else(|| "change-request head SHA is unknown".to_string())?
                };
                if claim_at.is_some_and(|claim_at| cr.head_sha.observed_at <= claim_at) {
                    return Ok(());
                }
                let evidence_at = match leaf.field_path.as_str() {
                    ".state" => cr.state.observed_at,
                    ".checks" => cr.checks.observed_at,
                    ".review.actionable-at-head" => cr.review.actionable_at_head.observed_at,
                    ".mergeable" => cr.mergeable.observed_at,
                    _ => {
                        return Err(DeliveryError::permanent(format!(
                            "turn-delivery leaf path `{}` has no firing evidence timestamp",
                            leaf.field_path
                        )))
                    }
                };
                let brief = if active_conflict {
                    format!("{}\n\nPR #{number} is conflicting. Rebase onto the current base branch, rerun the gates, push, then file a settlement claim.", rule.brief.trim())
                } else if active_probe {
                    let checks = match cr.checks.value {
                        Some(ObservedChecks::Pass) => "pass",
                        Some(ObservedChecks::Fail) => "fail",
                        Some(ObservedChecks::Pending) => "pending",
                        None => "unknown",
                    };
                    let actionable_review = match cr.review.actionable_at_head.value {
                        Some(true) => "true",
                        Some(false) => "false",
                        None => "unknown",
                    };
                    format!(
                        "{}\n\n## Turn firing context\n\n- Condition source: `{source}`\n- Head SHA: `{head_sha}`\n- Checks: {checks}\n- Review actionable at head: {actionable_review}\n- Durable convoy record: `{namespace}/{convoy_name}`\n- Target crew: `{}/{}`\n",
                        rule.brief.trim(), rule.to.vessel, rule.to.role,
                    )
                } else {
                    claim_at
                        .map(|claim_at| {
                            compose_change_request_turn_brief(
                                &convoy,
                                source,
                                rule,
                                leaf,
                                cr,
                                claim_at,
                                claim.and_then(|claim| claim.decision_ledger_ref.as_deref()),
                            )
                        })
                        .unwrap_or_default()
                };
                // Merge is one terminal episode for the subject, independent of
                // later head observations (including a previously unknown head).
                let revision = if merged_settlement { format!("{}@merged", leaf.address) } else { head_sha };
                let subject = cr.head_sha.value.as_ref().map_or_else(
                    || flotilla_resources::MessageReference::ControlRecord {
                        resource: flotilla_protocol::ResourceRef::new("flotilla.work/v1", "ChangeRequest", &namespace, &record_name),
                        revision: record.object.metadata.resource_version.clone(),
                    },
                    |head| flotilla_resources::MessageReference::ChangeRequest {
                        service: service.clone(),
                        scope: scope.clone(),
                        number: *number,
                        revision: head.clone(),
                    },
                );
                (revision, evidence_at, brief, Some(subject))
            }
            LeafAddress::Issue { service, scope, number } => {
                let record_name = flotilla_resources::issue_record_name(service, scope, *number);
                let record = self
                    .inner
                    .backend
                    .including_replicas::<Issue>(&namespace)
                    .get(&record_name)
                    .await
                    .map_err(|error| error.to_string())?;
                let issue = record.object.status.as_ref().ok_or_else(|| format!("issue observation `{record_name}` has no status"))?;
                let updated_at = issue.updated_at.value.ok_or_else(|| "issue updated-at is unknown".to_string())?;
                if claim_at.is_some_and(|claim_at| updated_at <= claim_at) {
                    return Ok(());
                }
                let evidence_at = match leaf.field_path.as_str() {
                    ".state" => issue.state.observed_at,
                    ".updated-at" => issue.updated_at.observed_at,
                    path if path.starts_with(".labels.") => issue.labels.observed_at,
                    _ => {
                        return Err(DeliveryError::permanent(format!(
                            "turn-delivery leaf path `{}` has no firing evidence timestamp",
                            leaf.field_path
                        )))
                    }
                };
                let observation = format!(
                    "- Issue updated at: `{updated_at}`\n- Issue state: {:?}\n- Issue labels: {:?}\n",
                    issue.state.value, issue.labels.value
                );
                let brief = claim_at
                    .map(|claim_at| {
                        compose_subject_turn_brief(
                            &convoy,
                            source,
                            rule,
                            leaf,
                            &observation,
                            claim_at,
                            claim.and_then(|claim| claim.decision_ledger_ref.as_deref()),
                        )
                    })
                    .unwrap_or_default();
                (
                    format!("{}@{}", leaf.address, updated_at.to_rfc3339()),
                    evidence_at,
                    brief,
                    Some(flotilla_resources::MessageReference::Issue {
                        service: service.clone(),
                        scope: scope.clone(),
                        number: *number,
                        revision: updated_at.to_rfc3339(),
                    }),
                )
            }
            LeafAddress::Artifact { convoy: artifact_convoy, producer, kind, subject } => {
                if matches!(
                    &rule.on.subject,
                    flotilla_resources::SubjectVariable::Artifact {
                        about: flotilla_resources::ArtifactSubjectBinding::ChangeRequestHead,
                        ..
                    }
                ) {
                    let checkout_sources =
                        self.inner.backend.including_replicas::<Checkout>(&namespace).list().await.map_err(|error| error.to_string())?;
                    let checkouts = select_convoy_children(&convoy, &checkout_sources.items);
                    let leaves = expected_change_request_leaves(&convoy, &checkouts)?;
                    let mut bound_to_current_head = false;
                    for candidate in leaves {
                        let LeafAddress::ChangeRequest { service, scope, number } = candidate.address else { continue };
                        let name = flotilla_resources::change_request_record_name(&service, &scope, number);
                        let record = self.inner.backend.including_replicas::<ChangeRequest>(&namespace).get(&name).await;
                        if record.ok().and_then(|record| record.object.status.and_then(|status| status.head_sha.value)).as_deref()
                            == Some(subject.as_str())
                        {
                            bound_to_current_head = true;
                            break;
                        }
                    }
                    if !bound_to_current_head {
                        return Ok(());
                    }
                }
                let name = flotilla_resources::artifact_record_name(artifact_convoy, producer, kind, subject);
                let artifact = self
                    .inner
                    .backend
                    .including_replicas::<Artifact>(&namespace)
                    .get(&name)
                    .await
                    .map_err(|error| error.to_string())?
                    .object;
                let bound_leaf = Leaf {
                    address: LeafAddress::Artifact {
                        convoy: artifact_convoy.clone(),
                        producer: producer.clone(),
                        kind: kind.clone(),
                        subject: subject.clone(),
                    },
                    ..leaf.clone()
                };
                if evaluate_leaf(&bound_leaf, Some(&ArtifactLeafSubject(&artifact)), None)?.result != ThreeValue::True {
                    return Ok(());
                }
                let evidence_at = artifact.spec.recorded_at.unwrap_or(artifact.metadata.creation_timestamp);
                let observation = format!(
                    "- Artifact: `{name}`\n- Subject: `{subject}`\n- Digest: `{}`\n- Summary: `{}`\n",
                    artifact.spec.digest,
                    serde_json::to_string(&artifact.spec.summary).map_err(|error| error.to_string())?
                );
                let brief = claim_at
                    .map(|claim_at| {
                        compose_subject_turn_brief(
                            &convoy,
                            source,
                            rule,
                            leaf,
                            &observation,
                            claim_at,
                            claim.and_then(|claim| claim.decision_ledger_ref.as_deref()),
                        )
                    })
                    .unwrap_or_default();
                (
                    format!("{subject}@{}", artifact.spec.digest),
                    evidence_at,
                    brief,
                    Some(flotilla_resources::MessageReference::Artifact {
                        resource: flotilla_protocol::ResourceRef::new("flotilla.work/v1", "Artifact", &namespace, &name),
                        revision: artifact.spec.digest.clone(),
                    }),
                )
            }
            _ => return Err(DeliveryError::permanent("turn-delivery leaf is not externally observed")),
        };
        if status
            .turn_deliveries
            .get(source)
            .is_some_and(|delivery| delivery.episodes.iter().any(|episode| episode.subject_revision == subject_revision))
        {
            return Ok(());
        }
        // A hold remains latched across new observations.
        if status.turn_deliveries.get(source).is_some_and(|delivery| {
            delivery.episodes.iter().any(|episode| matches!(episode.outcome, TurnDeliveryOutcome::Refused { hold_executed: true, .. }))
        }) {
            return Ok(());
        }
        // Active checks/review turns require evidence newer than this work's start;
        // an already-green adopted PR waits for a fresh observation.
        let judged_at = claim_at
            .or_else(|| active_probe.then(|| status.started_at.unwrap_or(convoy.metadata.creation_timestamp)))
            .ok_or_else(|| format!("turn-delivery target {}/{} has no settlement claim", rule.to.vessel, rule.to.role))?;
        if !active_conflict && evidence_at <= judged_at {
            return Ok(());
        }

        let request = CrewTurnIntent::builder()
            .namespace(namespace.clone())
            .convoy(convoy_name.to_string())
            .source(source.to_string())
            .vessel(rule.to.vessel.clone())
            .role(rule.to.role.clone())
            .brief(brief)
            .subject_revision(subject_revision.clone())
            .maybe_subject(match &leaf.address {
                LeafAddress::ChangeRequest { service, scope, number } => Some(flotilla_protocol::Subject {
                    kind: flotilla_protocol::SubjectKind::ChangeRequest,
                    source: flotilla_protocol::IssueSource { service: service.clone(), scope: scope.clone() },
                    id: number.to_string(),
                }),
                LeafAddress::Issue { service, scope, number } => Some(flotilla_protocol::Subject {
                    kind: flotilla_protocol::SubjectKind::Issue,
                    source: flotilla_protocol::IssueSource { service: service.clone(), scope: scope.clone() },
                    id: number.to_string(),
                }),
                _ => None,
            })
            .sender("system:turn-rules".into())
            .references(message_subject.clone().into_iter().collect())
            .maybe_message_subject(message_subject)
            .delivery_condition(leaf.clone())
            .build();
        let prior_episodes = status.turn_deliveries.get(source).map_or(0, |delivery| delivery.episodes.len()) as u32;
        let now = Utc::now();
        let patch = if prior_episodes >= self.inner.episode_limit {
            let reason =
                format!("turn delivery refused after {} consecutive episodes for condition source `{source}`", self.inner.episode_limit);
            let actuator = self.inner.turn_delivery.lock().await.clone();
            actuator.hold(&request, &rule.hold, &reason).await?;
            external_patches::refuse_turn_delivery(
                source.to_string(),
                TurnDeliveryEpisode {
                    subject_revision: subject_revision.clone(),
                    evidence_at,
                    judged_claim_at: judged_at,
                    outcome: TurnDeliveryOutcome::Refused { reason: reason.clone(), refused_at: now, hold_executed: true },
                    sender: flotilla_resources::CrewMessageSender::Unknown,
                },
                ConvoyAttention { source: source.to_string(), reason, raised_at: now },
            )
        } else {
            let actuator = self.inner.turn_delivery.lock().await.clone();
            let sessions = self
                .inner
                .backend
                .including_replicas::<TerminalSession>(&namespace)
                .list_matching_labels(&BTreeMap::from([
                    (CONVOY_LABEL.to_string(), convoy_name.to_string()),
                    (VESSEL_LABEL.to_string(), rule.to.vessel.clone()),
                    (ROLE_LABEL.to_string(), rule.to.role.clone()),
                ]))
                .await
                .map_err(|error| error.to_string())?;
            if sessions.items.iter().any(|session| !matches!(session.object.spec.source, TerminalSessionSource::Agent { .. })) {
                return Err(DeliveryError::permanent("turn-delivery target is not an agent"));
            }
            let admission = actuator.deliver(&request).await?;
            external_patches::record_turn_delivery(
                source.to_string(),
                TurnDeliveryEpisode {
                    subject_revision: subject_revision.clone(),
                    evidence_at,
                    judged_claim_at: judged_at,
                    outcome: TurnDeliveryOutcome::MessageAccepted {
                        new_turn: admission.new_turn,
                        rung: admission.rung,
                        message: admission.message,
                        accepted_at: Utc::now(),
                    },
                    sender: flotilla_resources::CrewMessageSender::Unknown,
                },
                rule.to.vessel.clone(),
                rule.to.role.clone(),
                request.brief.clone(),
            )
        };
        // The actuator may reopen the work before publishing the session message.
        // Apply the delivery record to that newer status rather than restoring
        // the pre-delivery snapshot (which would revoke credentials again).
        let current = convoys.get(convoy_name).await.map_err(|error| error.to_string())?;
        let mut next = current.status.clone().ok_or_else(|| format!("convoy {convoy_name} has no status"))?;
        patch.apply(&mut next);
        convoys.update_status(convoy_name, &current.metadata.resource_version, &next).await.map_err(|error| error.to_string())?;
        if let Some(row) = self.inner.rows.lock().await.get_mut(&subscription_id) {
            row.episode_key.subject_revision = Some(subject_revision);
        }
        Ok(())
    }
}
pub(super) fn compose_change_request_turn_brief(
    convoy: &ResourceObject<Convoy>,
    source: &str,
    rule: &TurnDeliveryRule,
    leaf: &Leaf,
    cr: &flotilla_resources::ChangeRequestStatus,
    claim_at: DateTime<Utc>,
    decision_ledger_ref: Option<&str>,
) -> String {
    let review_link = match (&leaf.address, leaf.field_path.as_str()) {
        (LeafAddress::ChangeRequest { service, scope, number }, ".review.actionable-at-head") if service == "github.com" => {
            format!("- Review feedback: https://github.com/{scope}/pull/{number}\n")
        }
        _ => String::new(),
    };
    let observation = format!(
        "- Head SHA: `{}`\n- Review actionable at head: {:?}\n{}- Checks: {:?}\n- Mergeability: {:?}\n",
        cr.head_sha.value.as_deref().unwrap_or("unknown"),
        cr.review.actionable_at_head.value,
        review_link,
        cr.checks.value,
        cr.mergeable.value,
    );
    compose_subject_turn_brief(convoy, source, rule, leaf, &observation, claim_at, decision_ledger_ref)
}

pub(super) fn compose_subject_turn_brief(
    convoy: &ResourceObject<Convoy>,
    source: &str,
    rule: &TurnDeliveryRule,
    leaf: &Leaf,
    observation: &str,
    claim_at: DateTime<Utc>,
    decision_ledger_ref: Option<&str>,
) -> String {
    let repositories = convoy
        .spec
        .repositories
        .iter()
        .map(|repo| format!("- {}: branch `{}` → `{}`", repo.url, repo.source_ref, repo.target_ref))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "{}\n\n## Turn firing context\n\n- Condition source: `{source}`\n- Fired leaf: `{leaf:?}`\n{}- Claim durability fence: `{}`\n- Decision ledger: {}\n- Durable convoy record: `{}/{}`\n- Target crew: `{}/{}`\n\n## Repositories and branches\n\n{}\n",
        rule.brief.trim(),
        observation,
        claim_at.to_rfc3339(),
        decision_ledger_ref.unwrap_or("MISSING (crew completed without a decision ledger)"),
        convoy.metadata.namespace,
        convoy.metadata.name,
        rule.to.vessel,
        rule.to.role,
        repositories,
    )
}

pub(crate) fn queued_turn_session<'a>(
    sessions: &'a BTreeMap<String, ResourceObject<TerminalSession>>,
    vessel: &str,
    role: &str,
) -> Option<&'a ResourceObject<TerminalSession>> {
    sessions.values().find(|session| {
        session.metadata.labels.get(VESSEL_LABEL).is_some_and(|label| label == vessel)
            && session.metadata.labels.get(ROLE_LABEL).is_some_and(|label| label == role)
    })
}

pub(crate) fn durable_message_evidence(message: &ResourceObject<flotilla_resources::Message>) -> (bool, String) {
    match &message.status {
        Some(status) if status.phase.has_delivery_evidence() => (true, String::new()),
        Some(status) => {
            (false, format!("message {:?}: {}", status.phase, status.reason.as_deref().unwrap_or("waiting for receiver evidence")))
        }
        None => (false, "message Accepted: waiting for receiver resolution".into()),
    }
}

pub(crate) fn queued_turn_evidence(session: Option<&ResourceObject<TerminalSession>>, message_id: &str) -> (bool, String) {
    let Some(session) = session else { return (false, "terminal session unavailable".into()) };
    let status = session.status.as_ref();
    if status.and_then(|status| status.delivered_message_id.as_deref()) == Some(message_id) {
        return (true, String::new());
    }
    if let TerminalSessionSource::Agent { message: Some(message), .. } = &session.spec.source {
        if message.delivered_through(status.and_then(|status| status.delivered_message_id.as_deref()), message_id) {
            return (true, String::new());
        }
        if message.next_after(status.and_then(|status| status.delivered_message_id.as_deref())).is_some_and(|next| next.id != message_id) {
            return (false, "waiting behind an earlier terminal message".into());
        }
    }
    if let Some(condition) = status.and_then(|status| status.degraded.as_ref()) {
        return (false, format!("{}: {}", condition.reason, condition.message));
    }
    match status {
        Some(status) if status.phase == TerminalSessionPhase::Running => match &status.attention {
            Some(attention) => (false, format!("attention {:?}; waiting for turn readiness or submission evidence", attention.state)),
            None => (false, "waiting for startup readiness or submission evidence".into()),
        },
        Some(status) => (false, format!("terminal {:?}; waiting for startup readiness", status.phase)),
        None => (false, "waiting for terminal startup".into()),
    }
}
