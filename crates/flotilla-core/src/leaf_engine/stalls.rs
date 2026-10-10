use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write;

use chrono::{DateTime, Utc};
use flotilla_protocol::{arg::shell_quote, Leaf, LeafAddress, LeafOperator};
use flotilla_resources::{
    actor_obligation, select_convoy_children, ChangeRequest, Checkout, Convoy, ConvoyAttention, ConvoyEnsure, ConvoyPhase, ConvoyStatus,
    CrewCompletionRefusal, CrewCompletionRefusalCause, LeafMaker, NudgeObligation, Project, ResourceObject, StallEvidenceSource,
    StallNudge, StallRung, StallSupervisor, StalledCondition, SupervisionTarget, TerminalAttention, TerminalAttentionSource,
    TerminalAttentionState, TerminalSession, TerminalSessionPhase, TerminalSessionSource, TurnDeliveryRule, Vessel, CONVOY_LABEL,
    ROLE_LABEL, VESSEL_LABEL,
};

use super::sources::{change_request_sources, freshest_change_requests};
use super::wake::ReconcilerWake;
use super::{CrewTurnIntent, LeafWatcher, UnableEvidenceKey};
use crate::{
    in_process::convoy_message_address,
    providers::{change_request::observation::ChangeRequestRef, forge::observation_error::ObservationError},
};

pub(crate) fn crew_role_address(project: &str, convoy: &str, vessel: &str, role: &str) -> String {
    format!("{project}/{convoy}/{vessel}/{role}")
}

pub(crate) fn supervision_message_source(convoy: &str, index: usize) -> String {
    format!("supervision-{convoy}-{index}")
}
// Bound silent turns, not their total runtime: long tool-using turns stay able.
pub(super) const TURN_INACTIVITY_BOUND: chrono::Duration = chrono::Duration::minutes(15);

pub(super) fn turn_inactivity_reason(
    status: &flotilla_resources::TerminalSessionStatus,
    obligation: &NudgeObligation,
    now: DateTime<Utc>,
) -> Option<String> {
    let since = obligation.working_since?;
    let hook = status
        .attention
        .as_ref()
        .filter(|attention| attention.source == TerminalAttentionSource::Hook && attention.state != TerminalAttentionState::Unobservable)
        .map(|attention| attention.as_of);
    let last_activity =
        [status.last_tool_activity_at, status.last_output_activity_at, obligation.last_hook_at, hook, obligation.reply_after]
            .into_iter()
            .flatten()
            .fold(since, std::cmp::max);
    let quiet = now.signed_duration_since(last_activity);
    (quiet >= TURN_INACTIVITY_BOUND).then(|| format!(
        "turn inactivity bound exceeded: turn began {since}, last tool {:?}, last hook {:?}, last output {:?}, latest attention {:?}; no tool, hook, or output activity for {} seconds; automatic interrupt deferred to supervisor",
        status.last_tool_activity_at, obligation.last_hook_at.or(hook), status.last_output_activity_at, status.attention.as_ref(), quiet.num_seconds()
    ))
}

// A long first turn may be legitimate; missing hooks are advisory, never a stall.
pub(super) const FIRST_HOOK_GRACE: chrono::Duration = chrono::Duration::minutes(15);

pub(super) fn missing_turn_hook(
    session: &ResourceObject<TerminalSession>,
    obligations: &[NudgeObligation],
    now: DateTime<Utc>,
) -> Option<String> {
    if !matches!(session.spec.source, TerminalSessionSource::Agent { .. }) {
        return None;
    }
    let status = session.status.as_ref()?;
    let crew = status.crew.as_ref()?;
    if status.phase != TerminalSessionPhase::Running || crate::agents::parser_for_harness(&crew.adapter).is_err() {
        return None;
    }
    let started = status.started_at?;
    let vessel = session.metadata.labels.get(VESSEL_LABEL)?;
    let hook_seen = status.last_tool_activity_at.is_some_and(|at| at >= started)
        || status.attention.as_ref().is_some_and(|attention| {
            attention.source == TerminalAttentionSource::Hook
                && attention.state != TerminalAttentionState::Unobservable
                && attention.as_of >= started
        }) || obligations.iter().any(|obligation| {
        matches!(&obligation.maker, LeafMaker::Actor { vessel: actor_vessel, role } if actor_vessel == vessel && role == &session.spec.role)
            && obligation.last_hook_at.is_some_and(|at| at >= started)
    });
    (!hook_seen && now.signed_duration_since(started) >= FIRST_HOOK_GRACE).then(|| {
        format!("{} crew {}@{} has delivered no hook since session {} started at {}; check turn hook wiring (Codex notify / Claude managed settings)",
            crew.adapter, session.spec.role, vessel, session.metadata.name, started)
    })
}

pub(super) fn actor_is_working(
    status: &flotilla_resources::TerminalSessionStatus,
    obligations: &[NudgeObligation],
    vessel: &str,
    role: &str,
    now: DateTime<Utc>,
) -> bool {
    let working = status
        .attention
        .as_ref()
        .is_some_and(|attention| attention.state == TerminalAttentionState::Working && !attention.is_stale_at(now));
    let silent = obligations.iter().any(|obligation| {
        matches!(&obligation.maker, LeafMaker::Actor { vessel: actor_vessel, role: actor_role } if actor_vessel == vessel && actor_role == role)
            && turn_inactivity_reason(status, obligation, now).is_some()
    });
    working && !silent
}

pub(super) fn stalled_source_actor(condition: &StalledCondition) -> Option<(&str, &str)> {
    let leaf = condition.leaves.first()?;
    let LeafAddress::Work { work, .. } = &leaf.address else { return None };
    let role = leaf.field_path.strip_prefix(".crew.")?.strip_suffix(".phase")?;
    Some((work, role))
}

/// Shared by supervisor delivery and the operator backstop, so both identify
/// the same crew and provide commands targeting the exact convoy record and crew.
pub(super) fn stall_supervision_brief(convoy: &ResourceObject<Convoy>, condition: &StalledCondition) -> String {
    let address = convoy_message_address(convoy);
    let actor =
        stalled_source_actor(condition).map(|(vessel, role)| format!("{role}@{vessel}")).unwrap_or_else(|| "unidentified crew".to_string());
    let reason = condition.reason.map_or_else(|| "inferred stall".to_string(), |reason| reason.to_string());
    let proposal = condition.proposed_disposition.map_or(String::new(), |disposition| format!(" Proposed disposition: {disposition}."));
    let mut brief = format!(
        "Supervise stalled crew {actor} in convoy {address} (resource ref: {}). Reason: {reason}. Evidence: {}.{proposal}",
        convoy.metadata.name, condition.evidence,
    );
    if let Some((vessel, role)) = stalled_source_actor(condition) {
        brief.push_str(" Resume it with guidance, convert it to failed, or escalate it.");
        for action in ["resume", "convert-to-failed", "escalate"] {
            write!(
                brief,
                "\n`flotilla crew supervise --convoy {} --vessel {} --role {} {action} --message 'guidance'`",
                shell_quote(&convoy.metadata.name),
                shell_quote(vessel),
                shell_quote(role),
            )
            .expect("writing to a String cannot fail");
        }
    }
    brief
}

// Keep each warning one event line while retaining copyable command text.
pub(super) fn stall_supervision_log_brief(convoy: &ResourceObject<Convoy>, condition: &StalledCondition) -> String {
    stall_supervision_brief(convoy, condition).replace('\n', " ")
}

pub(super) const DEFAULT_REFUSAL_LIMIT: u32 = 2;
pub(super) const DEFAULT_IDLE_GRACE_SECONDS: u32 = 180;

pub(super) fn nudge_policy<'a>(status: &'a ConvoyStatus, vessel: &str, role: &str) -> Option<&'a flotilla_resources::StallNudgePolicy> {
    status.workflow_snapshot.as_ref()?.stall_nudges.get(&format!("{vessel}/{role}"))
}

pub(super) fn idle_grace_seconds(status: &ConvoyStatus, vessel: &str, role: &str) -> u32 {
    nudge_policy(status, vessel, role).and_then(|policy| policy.idle_grace_seconds).unwrap_or(DEFAULT_IDLE_GRACE_SECONDS)
}

pub(super) fn is_conflict_probe(leaf: &Leaf) -> bool {
    leaf.field_path == ".mergeable" && leaf.operator == LeafOperator::Equal && leaf.literal == "conflicting"
}

pub(super) fn is_merged_settlement_probe(leaf: &Leaf) -> bool {
    matches!(leaf.address, LeafAddress::ChangeRequest { .. })
        && leaf.field_path == ".state"
        && leaf.operator == LeafOperator::Equal
        && leaf.literal == "merged"
}

/// Workflow-declared checks, review and merged settlement rules run during active crew work too,
/// including custom rules; conflict probes retain their active delivery behavior.
pub(super) fn is_active_change_request_probe(status: &ConvoyStatus, rule: &TurnDeliveryRule, leaf: &Leaf) -> bool {
    // Promise rejection rows are armed for their owner while work remains active.
    let promise_rejection = leaf.field_path == ".state"
        && leaf.operator == LeafOperator::Equal
        && leaf.literal == "closed"
        && flotilla_resources::promises::owned(status, &rule.to.vessel, &rule.to.role).iter().any(|promise| {
            !promise.state.terminal()
                && promise.submissions.last().is_some_and(|submission| {
                    submission.metadata.get("subject").unwrap_or(&submission.reference) == &leaf.address.to_string()
                })
        });
    let active_field = promise_rejection
        || is_conflict_probe(leaf)
        || is_merged_settlement_probe(leaf)
        || (matches!(leaf.address, LeafAddress::ChangeRequest { .. })
            && matches!(leaf.field_path.as_str(), ".checks" | ".review.actionable-at-head"));
    status.phase == ConvoyPhase::Active
        && active_field
        && status.crew_work.get(&rule.to.vessel).and_then(|crew| crew.get(&rule.to.role)).is_some_and(|work| {
            work.finished_at.is_none()
                && (is_conflict_probe(leaf)
                    || (promise_rejection && work.phase == flotilla_resources::CrewWorkPhase::Stalled)
                    || matches!(work.phase, flotilla_resources::CrewWorkPhase::Working | flotilla_resources::CrewWorkPhase::Interrupted))
        })
}

pub(super) fn refusal_nudge_brief(refusal: &CrewCompletionRefusal) -> String {
    let conflict = refusal.causes.iter().find_map(|cause| match cause {
        CrewCompletionRefusalCause::ConflictingChangeRequest { number, .. } => Some(*number),
        _ => None,
    });
    let missing = refusal.causes.iter().find_map(|cause| match cause {
        CrewCompletionRefusalCause::MissingChangeRequestObservation { number, .. } => Some(*number),
        _ => None,
    });
    // Prefer actionable conflicts over missing observations when several causes coexist.
    let remedy = if let Some(number) = conflict {
        format!("PR #{number} is conflicting: rebase onto the current base branch, rerun the gates, push, then `flotilla crew complete`.")
    } else if let Some(number) = missing {
        format!("PR #{number} has no observation yet: check the PR URL and forge access, then run `flotilla crew complete` again.")
    } else {
        "Resolve the unmet expectation, then run `flotilla crew complete` again.".to_string()
    };
    format!("Your settlement claim was refused: {}. {remedy}", refusal.expectation)
}

pub(super) fn refusal_limit(status: &ConvoyStatus, vessel: &str, role: &str) -> u32 {
    nudge_policy(status, vessel, role).and_then(|policy| policy.max_refusals).unwrap_or(DEFAULT_REFUSAL_LIMIT).max(1)
}

impl ReconcilerWake {
    pub(super) async fn judge_stalls(&self, namespace: &str, convoys: &HashMap<String, ResourceObject<Convoy>>) -> Result<(), String> {
        self.judge_stalls_at(namespace, convoys, Utc::now()).await
    }

    pub(super) async fn judge_stalls_at(
        &self,
        namespace: &str,
        convoys: &HashMap<String, ResourceObject<Convoy>>,
        now: DateTime<Utc>,
    ) -> Result<(), String> {
        let backend = &self.subscriptions.inner.backend;
        let inbox = self
            .subscriptions
            .inner
            .message_inboxes
            .lock()
            .await
            .entry(namespace.to_string())
            .or_insert_with(|| flotilla_resources::MessageInbox::new(backend.clone(), namespace))
            .clone();
        flotilla_resources::reconcile_charter_notifications_with_renderer(
            &inbox,
            self.subscriptions.inner.charter_brief_renderer.as_ref(),
            now,
        )
        .await
        .map_err(|error| error.to_string())?;
        let projects = backend.including_replicas::<Project>(namespace).list().await.map_err(|error| error.to_string())?.items;
        let available_convoys = backend.including_replicas::<Convoy>(namespace).list().await.map_err(|error| error.to_string())?.items;
        let available_ensures =
            backend.including_replicas::<ConvoyEnsure>(namespace).list().await.map_err(|error| error.to_string())?.items;
        let sessions = backend.including_replicas::<TerminalSession>(namespace).list().await.map_err(|error| error.to_string())?.items;
        let observation_list = backend.including_replicas::<ChangeRequest>(namespace).list().await.map_err(|error| error.to_string())?;
        let progress_observations = freshest_change_requests(&change_request_sources(observation_list.clone()));
        let observations = observation_list.items;
        let checkouts = backend.including_replicas::<Checkout>(namespace).list().await.map_err(|error| error.to_string())?.items;
        let vessels = backend.including_replicas::<Vessel>(namespace).list().await.map_err(|error| error.to_string())?.items;
        let messages =
            backend.including_replicas::<flotilla_resources::Message>(namespace).list().await.map_err(|error| error.to_string())?.items;
        let rows = self.subscriptions.rows().await;
        self.subscriptions.inner.supervisor_context.lock().await.retain(|(ns, name), _| ns != namespace || convoys.contains_key(name));
        for convoy in convoys.values() {
            let Some(status) = &convoy.status else { continue };
            let selected_sessions = select_convoy_children(convoy, &sessions);
            self.observe_queued_turns(namespace, convoy, &selected_sessions, now).await?;
            let selected_vessels = select_convoy_children(convoy, &vessels);
            let selected_checkouts = select_convoy_children(convoy, &checkouts);
            let mut obligations = status.nudge_obligations.clone();
            let holding = matches!(status.phase, ConvoyPhase::Active | ConvoyPhase::Landing);
            if holding {
                let mut missing_hooks =
                    selected_sessions.values().filter_map(|session| missing_turn_hook(session, &obligations, now)).collect::<Vec<_>>();
                missing_hooks.sort();
                let reason = (!missing_hooks.is_empty()).then(|| missing_hooks.join("\n"));
                let prior = status.attention.as_ref().filter(|attention| attention.source == ConvoyAttention::MISSING_TURN_HOOK_SOURCE);
                // Skip no-op writes here; the patch repeats the source guard after
                // an optimistic retry so concurrent settlement attention stays intact.
                if status.attention.as_ref().is_none_or(|attention| attention.source == ConvoyAttention::MISSING_TURN_HOOK_SOURCE)
                    && prior.map(|attention| &attention.reason) != reason.as_ref()
                {
                    flotilla_resources::apply_status_patch(
                        &backend.clone().using::<Convoy>(namespace),
                        &convoy.metadata.name,
                        &flotilla_resources::ConvoyStatusPatch::ObserveTurnHookHealth { reason, observed_at: now },
                    )
                    .await
                    .map_err(|error| error.to_string())?;
                }
            }
            if let Some(stalled) =
                status.stalled.as_ref().filter(|stalled| stalled.supervisor.is_some() && stalled.source != StallEvidenceSource::Crew)
            {
                if let Some((vessel, role)) = stalled_source_actor(stalled) {
                    let source_working = selected_sessions.values().any(|session| {
                        session.metadata.labels.get(VESSEL_LABEL).is_some_and(|name| name == vessel)
                            && session.metadata.labels.get(ROLE_LABEL).is_some_and(|name| name == role)
                            && session.status.as_ref().is_some_and(|status| actor_is_working(status, &obligations, vessel, role, now))
                    });
                    if source_working {
                        for obligation in &mut obligations {
                            if matches!(&obligation.maker, LeafMaker::Actor { vessel: actor_vessel, role: actor_role } if actor_vessel == vessel && actor_role == role)
                            {
                                obligation.quiet_since = None;
                            }
                        }
                        if status.nudge_obligations != obligations {
                            flotilla_resources::apply_status_patch(
                                &backend.clone().using::<Convoy>(namespace),
                                &convoy.metadata.name,
                                &flotilla_resources::ConvoyStatusPatch::SetNudgeObligations { obligations },
                            )
                            .await
                            .map_err(|error| error.to_string())?;
                        }
                        flotilla_resources::apply_status_patch(
                            &backend.clone().using::<Convoy>(namespace),
                            &convoy.metadata.name,
                            &flotilla_resources::ConvoyStatusPatch::SetStalled { condition: None },
                        )
                        .await
                        .map_err(|error| error.to_string())?;
                        continue;
                    }
                }
            }
            if !holding && status.stalled.is_none() {
                continue;
            }
            let convoy_rows = rows.iter().filter(|row| {
                row.namespace == namespace
                    && (status.phase != ConvoyPhase::Active || !matches!(row.watcher, LeafWatcher::TurnDelivery { .. }))
                    && matches!(&row.watcher, LeafWatcher::ReconcilerWake { convoy: name } | LeafWatcher::TurnDelivery { convoy: name, .. } if name == &convoy.metadata.name)
            }).collect::<Vec<_>>();
            obligations.retain(|obligation| match &obligation.maker {
                LeafMaker::Actor { vessel, role } => {
                    !status.phase.is_terminal()
                        && !status
                            .crew_work
                            .get(vessel)
                            .and_then(|crew| crew.get(role))
                            .is_some_and(|work| work.phase == flotilla_resources::CrewWorkPhase::Done)
                }
                _ => false,
            });
            let mut progress_changed = false;
            let mut unable = None;
            let mut able = false;
            let mut unknown = false;
            let mut actionable_rows = 0;
            'rows: for row in convoy_rows {
                if row.leaves.iter().any(|leaf| {
                    let LeafAddress::Work { work, .. } = &leaf.address else { return false };
                    let Some(role) = leaf.field_path.strip_prefix(".crew.").and_then(|field| field.strip_suffix(".phase")) else {
                        return false;
                    };
                    status.crew_work.get(work).and_then(|crew| crew.get(role)).is_some_and(|state| {
                        state.phase == flotilla_resources::CrewWorkPhase::Done
                            && leaf.operator == LeafOperator::Equal
                            && leaf.literal == "Done"
                    })
                }) {
                    continue;
                }
                actionable_rows += 1;
                let judgement = match &row.maker {
                    LeafMaker::Actor { vessel, role } => {
                        let session = selected_sessions.values().find(|session| {
                            session.metadata.labels.get(CONVOY_LABEL) == Some(&convoy.metadata.name)
                                && session.metadata.labels.get(VESSEL_LABEL) == Some(vessel)
                                && session.metadata.labels.get(ROLE_LABEL) == Some(role)
                        });
                        let index =
                            obligations.iter().position(|obligation| obligation.maker == row.maker && obligation.leaves == row.leaves);
                        let obligation = if let Some(index) = index {
                            &mut obligations[index]
                        } else {
                            let history = status
                                .stalled
                                .as_ref()
                                .filter(|stall| stall.maker.as_ref() == Some(&row.maker) && stall.leaves == row.leaves)
                                .map_or_else(Vec::new, |stall| stall.nudge_history.clone());
                            obligations.push(
                                NudgeObligation::builder().maker(row.maker.clone()).leaves(row.leaves.clone()).history(history).build(),
                            );
                            obligations.last_mut().expect("inserted obligation")
                        };
                        // Only observed revisions reset a budget. Missing/unknown
                        // records and routine refresh timestamps are not progress.
                        let checkout_refs = selected_vessels
                            .values()
                            .filter(|object| object.spec.vessel_name == *vessel)
                            .flat_map(|object| object.status.iter().flat_map(|status| status.checkout_refs.values()))
                            .collect::<HashSet<_>>();
                        let mut progress = BTreeMap::new();
                        for checkout in selected_checkouts.values() {
                            if checkout_refs.contains(&checkout.metadata.name) {
                                if let Some(commit) = checkout.status.as_ref().and_then(|status| status.integration.head_revision.as_ref())
                                {
                                    progress.insert(format!("checkout/{}", checkout.metadata.name), commit.clone());
                                }
                                if let Some(status) = &checkout.status {
                                    for (reference, observation) in &status.integration.remote_refs {
                                        progress.insert(format!("push/{}/{reference}", checkout.metadata.name), observation.digest.clone());
                                    }
                                }
                            }
                        }
                        for subject in flotilla_resources::active_change_request_subjects(convoy)? {
                            for object in progress_observations.values() {
                                if object.spec.service == subject.source.service
                                    && object.spec.scope == subject.source.scope
                                    && object.spec.number.to_string() == subject.id
                                {
                                    if let Some(head) = object.status.as_ref().and_then(|status| status.head_sha.value.as_ref()) {
                                        progress.insert(format!("cr/{}", object.metadata.name), head.clone());
                                    }
                                }
                            }
                        }
                        let changed = progress.iter().any(|(key, value)| obligation.progress.get(key).is_some_and(|prior| prior != value));
                        if changed {
                            obligation.history.clear();
                            obligation.quiet_since = None;
                            obligation.reply_after = None;
                            progress_changed = true;
                        }
                        obligation.progress.extend(progress);
                        let receiver_address = format!(
                            "{}/{}/{}/{}",
                            convoy.spec.project_ref.as_deref().unwrap_or(namespace),
                            convoy.metadata.name,
                            vessel,
                            role
                        );
                        let current_message = messages
                            .iter()
                            .filter(|message| {
                                message.object.spec.receiver == receiver_address
                                    && message
                                        .object
                                        .status
                                        .as_ref()
                                        .is_none_or(|status| !status.phase.is_terminal() || status.phase.has_delivery_evidence())
                            })
                            .max_by_key(|message| {
                                (
                                    message.object.status.as_ref().and_then(|status| status.accepted_sequence),
                                    message.object.metadata.creation_timestamp,
                                )
                            });
                        let session_status = session.and_then(|session| session.status.as_ref());
                        let message_id = current_message.map(|message| message.object.metadata.name.clone()).or_else(|| {
                            session.and_then(|session| match &session.spec.source {
                                TerminalSessionSource::Agent { message, .. } => message.as_ref().map(|message| message.id.clone()),
                                _ => None,
                            })
                        });
                        let delivered_id = messages
                            .iter()
                            .filter(|message| {
                                message.object.spec.receiver == receiver_address
                                    && message.object.status.as_ref().is_some_and(|status| status.phase.has_delivery_evidence())
                            })
                            .max_by_key(|message| {
                                message
                                    .object
                                    .status
                                    .as_ref()
                                    .and_then(|status| status.resolved_receiver.as_ref())
                                    .map(|receiver| receiver.delivered_at)
                            })
                            .map(|message| message.object.metadata.name.clone())
                            .or_else(|| session_status.and_then(|status| status.delivered_message_id.clone()));
                        if message_id != obligation.message_id || delivered_id != obligation.delivered_message_id {
                            if message_id.is_some() || delivered_id.is_some() {
                                obligation.reply_after = Some(now);
                            }
                            obligation.message_id = message_id.clone();
                            obligation.delivered_message_id = delivered_id;
                        }
                        let tool_activity = session_status.and_then(|status| status.last_tool_activity_at);
                        if tool_activity.is_some_and(|at| obligation.last_tool_activity_at.is_none_or(|previous| at > previous)) {
                            obligation.quiet_since = None;
                            obligation.reply_after = None;
                            obligation.last_tool_activity_at = tool_activity;
                        }
                        let pending_message = messages.iter().any(|message| {
                            message.object.spec.receiver == receiver_address
                                && message.object.status.as_ref().is_none_or(|status| status.phase.is_waiting())
                        }) || session.is_some_and(|session| match &session.spec.source {
                            TerminalSessionSource::Agent { message: Some(message), .. } => !message
                                .delivered_through(session_status.and_then(|status| status.delivered_message_id.as_deref()), &message.id),
                            _ => false,
                        });
                        if let Some(attention) = session_status.and_then(|status| status.attention.as_ref()) {
                            let echo = obligation.reply_after.is_some();
                            if !echo {
                                if obligation
                                    .last_attention_at
                                    .is_some_and(|previous| attention.as_of.signed_duration_since(previous) >= TerminalAttention::FRESH_FOR)
                                {
                                    obligation.quiet_since = None;
                                }
                                if attention.source == TerminalAttentionSource::Hook && obligation.last_hook_at != Some(attention.as_of) {
                                    obligation.quiet_since = None;
                                }
                                if attention.state != TerminalAttentionState::Idle || attention.is_stale_at(now) {
                                    obligation.quiet_since = None;
                                }
                            }
                            obligation.last_attention_at = Some(attention.as_of);
                            if attention.source == TerminalAttentionSource::Hook && attention.state != TerminalAttentionState::Unobservable
                            {
                                obligation.last_hook_at = Some(attention.as_of);
                                if attention.state == TerminalAttentionState::Idle
                                    && obligation.reply_after.is_some_and(|at| attention.as_of > at)
                                    && !pending_message
                                {
                                    // Consume exactly this delivered message's response end.
                                    // It neither starts an episode nor restarts its idle clock.
                                    obligation.reply_after = None;
                                }
                            }
                            if attention.state == TerminalAttentionState::Idle && !attention.is_stale_at(now) && !pending_message {
                                obligation.quiet_since.get_or_insert(now);
                            }
                        } else if obligation.reply_after.is_none() {
                            obligation.quiet_since = None;
                        }
                        match session_status.and_then(|status| status.attention.as_ref()).map(|attention| attention.state) {
                            Some(TerminalAttentionState::Working) => {
                                obligation.working_since.get_or_insert(now);
                            }
                            Some(TerminalAttentionState::Idle | TerminalAttentionState::NeedsInput) => {
                                obligation.working_since = None;
                            }
                            _ => {}
                        }
                        let turn_inactivity = session_status.and_then(|status| turn_inactivity_reason(status, obligation, now));
                        let awaiting_reply_observation = obligation.reply_after.is_some_and(|after| {
                            session_status.and_then(|status| status.attention.as_ref()).is_none_or(|attention| attention.as_of <= after)
                        });
                        let idle_grace = chrono::Duration::seconds(i64::from(idle_grace_seconds(status, vessel, role)));
                        let lost_reason = session
                            .and_then(|session| session.status.as_ref())
                            .filter(|status| status.phase == TerminalSessionPhase::Lost)
                            .and_then(|status| status.message.as_deref());
                        let attention = session
                            .and_then(|session| session.status.as_ref())
                            .filter(|status| status.phase == TerminalSessionPhase::Running)
                            .and_then(|status| status.attention.as_ref());
                        if status
                            .crew_work
                            .get(vessel)
                            .and_then(|crew| crew.get(role))
                            .is_some_and(|work| work.phase == flotilla_resources::CrewWorkPhase::Stalled)
                        {
                            Err((
                                status.crew_work[vessel][role].message.clone().unwrap_or_else(|| "crew stalled".into()),
                                StallEvidenceSource::Crew,
                            ))
                        } else if let Some(reason) = lost_reason {
                            self.subscriptions.inner.unable_since.lock().await.remove(&row.id);
                            Err((format!("session dead: {reason}"), StallEvidenceSource::Session))
                        } else if let Some(reason) = turn_inactivity {
                            Err((reason, StallEvidenceSource::Session))
                        } else {
                            match attention {
                                Some(attention) if attention.state == TerminalAttentionState::Working && !attention.is_stale_at(now) => {
                                    self.subscriptions.inner.unable_since.lock().await.remove(&row.id);
                                    self.subscriptions.inner.stale_attention_reported.lock().await.remove(&row.id);
                                    Ok(())
                                }
                                Some(attention) if attention.is_stale_at(now) => {
                                    self.subscriptions.inner.unable_since.lock().await.remove(&row.id);
                                    self.report_stale_attention(row, attention.source).await;
                                    unknown = true;
                                    continue;
                                }
                                Some(attention) if attention.state == TerminalAttentionState::Unobservable => {
                                    self.subscriptions.inner.unable_since.lock().await.remove(&row.id);
                                    unknown = true;
                                    continue;
                                }
                                Some(attention) => {
                                    self.subscriptions.inner.stale_attention_reported.lock().await.remove(&row.id);
                                    let source = match attention.source {
                                        TerminalAttentionSource::Screen => StallEvidenceSource::Screen,
                                        TerminalAttentionSource::Hook => StallEvidenceSource::Hook,
                                    };
                                    if attention.state == TerminalAttentionState::Idle {
                                        if pending_message
                                            || awaiting_reply_observation
                                            || obligation.quiet_since.is_none_or(|since| now.signed_duration_since(since) < idle_grace)
                                        {
                                            unknown = true;
                                            continue;
                                        }
                                    } else if self
                                        .maker_debouncing(
                                            row.id,
                                            UnableEvidenceKey::Attention { state: attention.state, source: attention.source },
                                            attention.as_of,
                                            if attention.source == TerminalAttentionSource::Hook {
                                                chrono::Duration::zero()
                                            } else {
                                                TerminalAttention::DEBOUNCE_FOR
                                            },
                                            now,
                                        )
                                        .await
                                    {
                                        unknown = true;
                                        continue;
                                    }
                                    let evidence = match attention.state {
                                        TerminalAttentionState::Idle => "idle".into(),
                                        TerminalAttentionState::NeedsInput => "NeedsInput".into(),
                                        TerminalAttentionState::Unobservable => unreachable!("handled as unknown"),
                                        TerminalAttentionState::Working => "working".into(),
                                    };
                                    Err((evidence, source))
                                }
                                None => {
                                    if session.is_some_and(|session| {
                                        session.status.as_ref().is_some_and(|status| status.phase == TerminalSessionPhase::Running)
                                    }) {
                                        self.subscriptions.inner.unable_since.lock().await.remove(&row.id);
                                        unknown = true;
                                        continue;
                                    }
                                    if self
                                        .maker_debouncing(row.id, UnableEvidenceKey::Absent, now, TerminalAttention::DEBOUNCE_FOR, now)
                                        .await
                                    {
                                        unknown = true;
                                        continue;
                                    }
                                    Err(("session dead or absent".into(), StallEvidenceSource::Session))
                                }
                            }
                        }
                    }
                    LeafMaker::Supervisor { convoy: supervisor_convoy, vessel, role } => {
                        let session = sessions.iter().find(|source| {
                            let session = &source.object;
                            session.metadata.labels.get(CONVOY_LABEL) == Some(supervisor_convoy)
                                && session.metadata.labels.get(VESSEL_LABEL) == Some(vessel)
                                && session.metadata.labels.get(ROLE_LABEL) == Some(role)
                        });
                        let attention = session
                            .and_then(|source| source.object.status.as_ref())
                            .filter(|status| status.phase == TerminalSessionPhase::Running)
                            .and_then(|status| status.attention.as_ref());
                        match attention {
                            Some(attention) if attention.state == TerminalAttentionState::Working && !attention.is_stale_at(now) => {
                                self.subscriptions.inner.stale_attention_reported.lock().await.remove(&row.id);
                                Ok(())
                            }
                            Some(attention) if attention.state == TerminalAttentionState::Idle && !attention.is_stale_at(now) => {
                                self.subscriptions.inner.stale_attention_reported.lock().await.remove(&row.id);
                                if self
                                    .maker_debouncing(
                                        row.id,
                                        UnableEvidenceKey::Attention { state: attention.state, source: attention.source },
                                        row.created_at,
                                        TerminalAttention::DEBOUNCE_FOR,
                                        now,
                                    )
                                    .await
                                {
                                    unknown = true;
                                    continue;
                                }
                                Err(("supervisor idle".into(), StallEvidenceSource::Session))
                            }
                            Some(attention) if attention.state == TerminalAttentionState::NeedsInput && !attention.is_stale_at(now) => {
                                self.subscriptions.inner.stale_attention_reported.lock().await.remove(&row.id);
                                Err(("supervisor needs input".into(), StallEvidenceSource::Hook))
                            }
                            Some(attention) if attention.is_stale_at(now) => {
                                self.subscriptions.inner.unable_since.lock().await.remove(&row.id);
                                self.report_stale_attention(row, attention.source).await;
                                unknown = true;
                                continue;
                            }
                            None if session.is_some_and(|source| {
                                source.object.status.as_ref().is_some_and(|status| status.phase == TerminalSessionPhase::Running)
                            }) =>
                            {
                                unknown = true;
                                continue;
                            }
                            Some(_) => {
                                unknown = true;
                                continue;
                            }
                            None => {
                                if self
                                    .maker_debouncing(
                                        row.id,
                                        UnableEvidenceKey::Absent,
                                        row.created_at,
                                        TerminalAttention::DEBOUNCE_FOR,
                                        now,
                                    )
                                    .await
                                {
                                    unknown = true;
                                    continue;
                                }
                                Err(("supervisor session dead or absent".into(), StallEvidenceSource::Session))
                            }
                        }
                    }
                    LeafMaker::Observed { .. } => {
                        let mut reason = None;
                        for leaf in &row.leaves {
                            if let LeafAddress::ChangeRequest { service, scope, number } = &leaf.address {
                                let name = flotilla_resources::change_request_record_name(service, scope, *number);
                                let observation = observations
                                    .iter()
                                    .filter(|source| source.object.metadata.name == name)
                                    .filter_map(|source| source.object.status.as_ref())
                                    .max_by_key(|status| status.state.observed_at);
                                let has_value = observation.is_some_and(|status| match leaf.field_path.as_str() {
                                    ".state" => status.state.value.is_some(),
                                    ".head-sha" => status.head_sha.value.is_some(),
                                    ".checks" => status.checks.value.is_some(),
                                    ".review.actionable-at-head" => status.review.actionable_at_head.value.is_some(),
                                    ".mergeable" => status.mergeable.value.is_some(),
                                    _ => false,
                                });
                                let fresh = has_value
                                    && observation.is_some_and(|status| {
                                        let at = match leaf.field_path.as_str() {
                                            ".head-sha" => status.head_sha.observed_at,
                                            ".checks" => status.checks.observed_at,
                                            ".review.actionable-at-head" => status.review.actionable_at_head.observed_at,
                                            ".mergeable" => status.mergeable.observed_at,
                                            _ => status.state.observed_at,
                                        };
                                        row.freshness_demand.is_none_or(|demand| at >= demand)
                                            && now.signed_duration_since(at)
                                                < chrono::Duration::from_std(self.subscriptions.change_request_stale_after())
                                                    .expect("duration fits")
                                    });
                                if !fresh {
                                    let subject = ChangeRequestRef {
                                        namespace: namespace.into(),
                                        service: service.clone(),
                                        scope: scope.clone(),
                                        number: *number,
                                    };
                                    let refresh_error = self.subscriptions.change_request_observation_error(&subject).await;
                                    if refresh_error.as_ref().and_then(ObservationError::retry_at).is_some_and(|retry_at| retry_at > now) {
                                        // A known forge retry deadline keeps the observed
                                        // maker pending; missing evidence is not a crew stall.
                                        // Deliberately defer every leaf in this row until
                                        // its observed maker can be judged with fresh evidence.
                                        unknown = true;
                                        continue 'rows;
                                    }
                                    reason = Some(match (observation.is_some(), has_value, refresh_error) {
                                        (true, true, Some(error)) => format!("stale; refresh failed: {error}"),
                                        (true, true, None) => "stale".into(),
                                        (_, _, Some(error)) => error.to_string(),
                                        _ => "not refreshed".into(),
                                    });
                                    break;
                                }
                            }
                        }
                        reason.map_or(Ok(()), |reason| Err((reason, StallEvidenceSource::Observation)))
                    }
                    LeafMaker::Controller { retry, ceiling, .. } => {
                        retry.stall_reason(now, *ceiling).map_or(Ok(()), |reason| Err((reason, StallEvidenceSource::LeafEngine)))
                    }
                };
                match judgement {
                    Ok(()) => {
                        able = true;
                        continue;
                    }
                    Err((evidence, source)) => {
                        if matches!(row.maker, LeafMaker::Controller { .. }) {
                            unable = Some((row, evidence, source));
                            able = false;
                            break;
                        }
                        unable.get_or_insert((row, evidence, source))
                    }
                };
            }
            let project_policy = convoy.spec.project_ref.as_ref().and_then(|project| {
                projects
                    .iter()
                    .find(|source| source.object.metadata.name == *project)
                    .and_then(|source| source.object.spec.supervision.clone())
            });
            let mut policy = status
                .workflow_snapshot
                .as_ref()
                .and_then(|workflow| workflow.supervision.clone())
                .or(project_policy)
                .unwrap_or_else(|| {
                    vec![
                        SupervisionTarget::ConvoyCrew { vessel: String::new(), role: "bosun".into() },
                        SupervisionTarget::ProjectCrew { convoy_role: "governor".into(), vessel: String::new(), role: "governor".into() },
                    ]
                });
            if let Some(project) = convoy.spec.project_ref.as_deref() {
                let sender = status
                    .stalled
                    .as_ref()
                    .and_then(stalled_source_actor)
                    .map(|(vessel, role)| crew_role_address(project, &convoy.metadata.name, vessel, role))
                    .unwrap_or_else(|| format!("{project}/{}", convoy.spec.role));
                let path =
                    flotilla_resources::supervision_path(backend, namespace, project, &sender).await.map_err(|error| error.to_string())?;
                if !path.is_empty() {
                    // Explicit convoy supervision remains first; Project and fleet
                    // routing comes from subscriptions, not role-name guesses.
                    policy.retain(|target| matches!(target, SupervisionTarget::ConvoyCrew { .. }));
                    policy.extend(path.into_iter().map(|contact| SupervisionTarget::Address { address: contact.address }));
                }
            }
            // An exhausted cursor on a crew target is a legacy failed lookup,
            // not a consumed operator policy. Retry that target after one roll.
            let retry_exhausted = status.stalled.as_ref().is_some_and(|stalled| {
                stalled.supervision_exhausted
                    && stalled.supervisor.is_none()
                    && stalled
                        .supervision_index
                        .is_some_and(|index| policy.get(index).is_some_and(|target| !matches!(target, SupervisionTarget::Operator)))
            });
            // Previous-generation fallback records were marked exhausted with no
            // consumed rung. Retry those too; only a consumed ladder stays exhausted.
            if status.stalled.as_ref().is_some_and(|stalled| stalled.supervision_exhausted && stalled.supervision_index.is_some())
                && !retry_exhausted
                && !able
                && !progress_changed
                && status.phase == ConvoyPhase::Active
                && actionable_rows > 0
            {
                continue;
            }
            let next = if holding && !able && !unknown && (status.phase != ConvoyPhase::Active || actionable_rows > 0) {
                let (leaves, maker, evidence, source) = if let Some((row, evidence, source)) = unable.as_ref() {
                    (row.leaves.clone(), Some(row.maker.clone()), evidence.clone(), source.clone())
                } else {
                    (Vec::new(), None, "no armed row with an able maker".into(), StallEvidenceSource::LeafEngine)
                };
                let prior = status.stalled.as_ref().filter(|prior| prior.leaves == leaves && prior.maker == maker);
                let nudge_history = obligations
                    .iter()
                    .find(|obligation| Some(&obligation.maker) == maker.as_ref() && obligation.leaves == leaves)
                    .map_or_else(Vec::new, |obligation| obligation.history.clone());
                let mut condition = StalledCondition {
                    leaves,
                    maker,
                    evidence,
                    source,
                    cause: None,
                    began_at: prior.map_or(now, |stalled| stalled.began_at),
                    rung: StallRung::Operator,
                    supervisor: None,
                    supervision_message: None,
                    supervision_index: None,
                    supervision_exhausted: false,
                    reason: None,
                    proposed_disposition: None,
                    nudge_history,
                };
                let declared = status
                    .stalled
                    .as_ref()
                    .filter(|stalled| stalled.source == StallEvidenceSource::Crew && stalled.leaves == condition.leaves);
                if let Some(declared) = declared {
                    // Keep the crew's evidence, rather than accumulating prior delivery diagnostics.
                    condition.evidence = stalled_source_actor(declared)
                        .and_then(|(vessel, role)| status.crew_work.get(vessel)?.get(role)?.message.clone())
                        .unwrap_or_else(|| declared.evidence.clone());
                    condition.source = StallEvidenceSource::Crew;
                    condition.reason = declared.reason;
                    condition.proposed_disposition = declared.proposed_disposition;
                    condition.began_at = declared.began_at;
                }
                if matches!(condition.maker, Some(LeafMaker::Supervisor { .. })) {
                    if let Some(prior) = prior {
                        condition.evidence = prior.evidence.clone();
                        condition.source = prior.source.clone();
                        condition.reason = prior.reason;
                        condition.proposed_disposition = prior.proposed_disposition;
                        condition.began_at = prior.began_at;
                    }
                }
                let mut awaiting_resumed_turn = false;
                if let (Some(LeafMaker::Actor { vessel, role }), Some(row)) = (&condition.maker, unable.as_ref().map(|(row, _, _)| *row)) {
                    let refusal =
                        status.crew_work.get(vessel).and_then(|crew| crew.get(role)).and_then(|work| work.completion_refusal.as_ref());
                    let refusal_escalated = refusal.is_some_and(|refusal| refusal.consecutive_count >= refusal_limit(status, vessel, role));
                    let session = selected_sessions.values().find(|session| {
                        session.metadata.labels.get(VESSEL_LABEL) == Some(vessel) && session.metadata.labels.get(ROLE_LABEL) == Some(role)
                    });
                    let idle_at = session
                        .filter(|session| matches!(session.spec.source, TerminalSessionSource::Agent { .. }))
                        .and_then(|session| session.status.as_ref())
                        .and_then(|status| status.attention.as_ref())
                        .filter(|attention| attention.state == TerminalAttentionState::Idle && !attention.is_stale_at(now))
                        .map(|attention| attention.as_of);
                    let resume_grace = status
                        .crew_work
                        .get(vessel)
                        .and_then(|crew| crew.get(role))
                        .filter(|work| work.phase == flotilla_resources::CrewWorkPhase::Working)
                        .and_then(|work| work.resumed_at.zip(work.resume_brief_id.as_deref()))
                        .is_some_and(|(resumed_at, brief_id)| {
                            // A missing or recreated session must not suppress
                            // supervision indefinitely while its brief is unconfirmed.
                            if now.signed_duration_since(resumed_at) >= chrono::Duration::minutes(2) {
                                return false;
                            }
                            let operator_brief_delivered = messages.iter().any(|message| {
                                message.object.metadata.name == brief_id
                                    && message.object.status.as_ref().is_some_and(|status| status.phase.has_delivery_evidence())
                            }) || session.is_some_and(|session| {
                                let delivered_id = session.status.as_ref().and_then(|status| status.delivered_message_id.as_deref());
                                matches!(&session.spec.source, TerminalSessionSource::Agent { message: Some(message), .. }
                                    if message.delivered_through(delivered_id, brief_id))
                            });
                            !operator_brief_delivered || idle_at.is_none_or(|idle_at| idle_at <= resumed_at)
                        });
                    awaiting_resumed_turn = resume_grace;
                    if refusal_escalated {
                        condition.rung = StallRung::Operator;
                        condition.evidence = format!(
                            "settlement claim refused {} times: {}",
                            refusal.map_or(0, |refusal| refusal.consecutive_count),
                            refusal.map_or("", |refusal| refusal.expectation.as_str())
                        );
                    } else if idle_at.is_some() && declared.is_none() && !resume_grace {
                        let limit = nudge_policy(status, vessel, role).map_or(2, |policy| policy.max_per_episode) as usize;
                        let grace_seconds = idle_grace_seconds(status, vessel, role);
                        // Initial grace is 1x; intervals after delivered nudges are 1x, 2x, 4x, ...
                        let backoff = chrono::Duration::seconds(
                            i64::from(grace_seconds.max(1))
                                .saturating_mul(1_i64 << condition.nudge_history.len().saturating_sub(1).min(20)),
                        );
                        let due = condition.nudge_history.last().is_none_or(|nudge| now.signed_duration_since(nudge.at) >= backoff);
                        if prior.is_some_and(|stalled| {
                            stalled.rung == StallRung::Operator && stalled.evidence.starts_with("nudge delivery failed:")
                        }) {
                            condition.rung = StallRung::Operator;
                            condition.evidence = prior.expect("checked above").evidence.clone();
                        } else if condition.nudge_history.len() < limit {
                            condition.rung = StallRung::Nudge;
                            if due {
                                let leaf = row.leaves.first().ok_or_else(|| "actor row has no leaf".to_string())?;
                                let declared_stall = status
                                    .crew_work
                                    .get(vessel)
                                    .and_then(|crew| crew.get(role))
                                    .is_some_and(|crew| crew.phase == flotilla_resources::CrewWorkPhase::Stalled);
                                let obligation = if declared_stall {
                                    "Your stall is recorded. What changed since your report? If the blocker persists, update the stall with new evidence; if it has cleared, ask your supervisor to resume you.".into()
                                } else if let Some(refusal) = refusal {
                                    refusal_nudge_brief(refusal)
                                } else {
                                    actor_obligation(leaf)?
                                };
                                let brief = format!(
                                    "For {role}@{vessel} in {} (resource ref: {}):\n{obligation}",
                                    convoy_message_address(convoy),
                                    convoy.metadata.name
                                );
                                let request = CrewTurnIntent::builder()
                                    .namespace(namespace.to_string())
                                    .convoy(convoy.metadata.name.clone())
                                    .source(format!("stall-nudge-{}", condition.nudge_history.len() + 1))
                                    .vessel(vessel.clone())
                                    .role(role.clone())
                                    .brief(brief)
                                    .subject_revision(now.timestamp_micros().to_string())
                                    .sender("system:nudge".into())
                                    .expectation(flotilla_resources::MessageExpectation::Outcome { condition: leaf.clone() })
                                    .references(vec![flotilla_resources::MessageReference::ControlRecord {
                                        resource: flotilla_protocol::ResourceRef::new(
                                            "flotilla.work/v1",
                                            "Convoy",
                                            namespace,
                                            &convoy.metadata.name,
                                        ),
                                        revision: convoy.metadata.resource_version.clone(),
                                    }])
                                    .build();
                                match self.subscriptions.inner.turn_delivery.lock().await.clone().deliver(&request).await {
                                    Ok(_) => {
                                        condition.nudge_history.push(StallNudge { at: now, row: leaf.clone() });
                                        if let Some(obligation) = obligations
                                            .iter_mut()
                                            .find(|obligation| obligation.maker == row.maker && obligation.leaves == row.leaves)
                                        {
                                            obligation.history = condition.nudge_history.clone();
                                            obligation.reply_after = Some(now);
                                        }
                                    }
                                    Err(error) => {
                                        tracing::warn!(
                                            convoy = %convoy.metadata.name,
                                            target = %role,
                                            %vessel,
                                            reason = %error,
                                            brief = %stall_supervision_log_brief(convoy, &condition),
                                            "stall nudge fell back to operator"
                                        );
                                        condition.rung = StallRung::Operator;
                                        condition.evidence = format!("nudge delivery failed: {error}");
                                    }
                                }
                            }
                        } else if !due {
                            condition.rung = StallRung::Nudge;
                        }
                    }
                }
                let needs_supervisor = matches!(condition.maker, Some(LeafMaker::Supervisor { .. }))
                    || condition.source == StallEvidenceSource::Crew
                    || condition.source == StallEvidenceSource::Session
                    || matches!(&condition.maker, Some(LeafMaker::Actor { vessel, role }) if status.crew_work.get(vessel)
                        .and_then(|crew| crew.get(role)).and_then(|work| work.completion_refusal.as_ref())
                        .is_some_and(|refusal| refusal.consecutive_count >= refusal_limit(status, vessel, role)))
                    || condition.evidence == "NeedsInput"
                    || (condition.rung == StallRung::Operator && !condition.nudge_history.is_empty())
                    || (condition.rung == StallRung::Operator
                        && matches!(condition.maker, Some(LeafMaker::Actor { .. }))
                        && condition.evidence == "idle"
                        && condition.nudge_history.is_empty());
                if needs_supervisor && !awaiting_resumed_turn && !condition.evidence.starts_with("nudge delivery failed:") {
                    let start = prior
                        .and_then(|stalled| stalled.supervision_index.map(|index| if retry_exhausted { index } else { index + 1 }))
                        .unwrap_or(0);
                    let keep_current = prior.is_some_and(|stalled| stalled.supervisor.is_some())
                        && !matches!(condition.maker, Some(LeafMaker::Supervisor { .. }));
                    let context = format!(
                        "{:?}",
                        (
                            &policy,
                            status.crew_work.iter().map(|(vessel, crew)| (vessel, crew.keys().collect::<Vec<_>>())).collect::<Vec<_>>(),
                            available_convoys
                                .iter()
                                .filter(|candidate| candidate.object.metadata.name != convoy.metadata.name
                                    && candidate.object.spec.project_ref == convoy.spec.project_ref)
                                .map(|candidate| {
                                    let candidate = &candidate.object;
                                    (
                                        &candidate.metadata.name,
                                        (
                                            &candidate.spec.role,
                                            candidate.spec.generation,
                                            candidate.status.as_ref().map(|status| {
                                                (
                                                    &status.phase,
                                                    status
                                                        .crew_work
                                                        .iter()
                                                        .map(|(vessel, crew)| (vessel, crew.keys().collect::<Vec<_>>()))
                                                        .collect::<Vec<_>>(),
                                                )
                                            }),
                                        ),
                                    )
                                })
                                .collect::<BTreeMap<_, _>>(),
                            available_ensures
                                .iter()
                                .map(|ensure| (&ensure.object.metadata.name, &ensure.object.metadata.resource_version))
                                .collect::<BTreeMap<_, _>>(),
                            projects
                                .iter()
                                .map(|project| (&project.object.metadata.name, &project.object.metadata.resource_version))
                                .collect::<BTreeMap<_, _>>()
                        )
                    );
                    let context_key = (namespace.to_string(), convoy.metadata.name.clone());
                    let unchanged_context = self.subscriptions.inner.supervisor_context.lock().await.get(&context_key) == Some(&context);
                    let cached_lookup = unchanged_context
                        && prior.is_some_and(|prior| {
                            prior.rung == StallRung::Operator && prior.supervisor.is_none() && !prior.supervision_exhausted
                        });
                    if keep_current || cached_lookup {
                        condition = prior.expect("checked above").clone();
                    } else {
                        self.subscriptions.inner.supervisor_context.lock().await.remove(&context_key);
                        condition.rung = StallRung::Operator;
                        condition.supervisor = None;
                        // The cursor records consumed rungs, not failed delivery attempts.
                        // Retain it so retrying a higher rung cannot route back to a lower one.
                        // Persist the last consumed rung, not the next attempt.
                        // Retrying legacy rung i stores i - 1 (None at zero), so
                        // failed lookup/delivery retries i without revisiting lower rungs.
                        condition.supervision_index = prior.and_then(|stalled| stalled.supervision_index).and_then(|index| {
                            if retry_exhausted {
                                index.checked_sub(1)
                            } else {
                                Some(index)
                            }
                        });
                        condition.maker = condition.leaves.first().and_then(|leaf| {
                            let LeafAddress::Work { work, .. } = &leaf.address else { return None };
                            let role = leaf.field_path.strip_prefix(".crew.")?.strip_suffix(".phase")?;
                            Some(LeafMaker::Actor { vessel: work.clone(), role: role.to_string() })
                        });
                        condition.supervision_exhausted = true;
                        let mut unavailable_target = None;
                        let mut delivery_failed = false;
                        for (index, target) in policy.iter().enumerate().skip(start) {
                            let candidate = match target {
                                SupervisionTarget::ConvoyCrew { vessel, role } => {
                                    let found = status
                                        .crew_work
                                        .iter()
                                        .find(|(name, crew)| (vessel.is_empty() || *name == vessel) && crew.contains_key(role));
                                    found.map(|(name, _)| (convoy.metadata.name.clone(), name.clone(), role.clone(), StallRung::Bosun))
                                }
                                SupervisionTarget::ProjectCrew { convoy_role, vessel, role } => {
                                    let owned = available_ensures
                                        .iter()
                                        .map(|source| &source.object)
                                        .find(|ensure| {
                                            Some(ensure.spec.project_ref.as_str()) == convoy.spec.project_ref.as_deref()
                                                && ensure.spec.role == *convoy_role
                                        })
                                        .and_then(|ensure| ensure.status.as_ref())
                                        .and_then(|status| status.convoy_ref.as_deref());
                                    let candidate = available_convoys
                                        .iter()
                                        .map(|source| &source.object)
                                        .filter(|candidate| {
                                            convoy.spec.project_ref.is_some()
                                                && candidate.spec.project_ref == convoy.spec.project_ref
                                                && candidate.spec.role == *convoy_role
                                                && candidate.metadata.name != convoy.metadata.name
                                                && candidate.status.as_ref().is_some_and(|status| !status.phase.is_terminal())
                                        })
                                        .max_by_key(|candidate| {
                                            (
                                                Some(candidate.metadata.name.as_str()) == owned,
                                                candidate.spec.generation,
                                                &candidate.metadata.name,
                                            )
                                        });
                                    let supervisor = candidate.and_then(|candidate| {
                                        candidate.status.as_ref().and_then(|status| {
                                            status
                                                .crew_work
                                                .iter()
                                                .find(|(name, crew)| (vessel.is_empty() || *name == vessel) && crew.contains_key(role))
                                                .map(|(name, _)| {
                                                    (candidate.metadata.name.clone(), name.clone(), role.clone(), StallRung::Governor)
                                                })
                                        })
                                    });
                                    if supervisor.is_none() {
                                        if let Some(project) = convoy.spec.project_ref.as_deref() {
                                            let missing = if candidate.is_some() { " crew" } else { "" };
                                            let detail = if candidate.is_some() {
                                                format!("required crew {vessel}/{role} is absent")
                                            } else {
                                                let matching = available_convoys
                                                    .iter()
                                                    .filter(|source| {
                                                        source.object.spec.project_ref.as_deref() == Some(project)
                                                            && source.object.spec.role == *convoy_role
                                                    })
                                                    .count();
                                                if matching == 0 {
                                                    "no matching convoy in replica view".to_string()
                                                } else {
                                                    "matching convoys are terminal or the stalled convoy itself".to_string()
                                                }
                                            };
                                            condition
                                                .evidence
                                                .push_str(&format!("; no live {convoy_role}{missing} for project {project} ({detail})"));
                                        } else {
                                            condition.evidence.push_str(&format!("; cannot find {convoy_role}: convoy has no project_ref"));
                                        }
                                    }
                                    supervisor
                                }
                                SupervisionTarget::Address { address } => {
                                    flotilla_resources::resolve_message_receiver(backend, namespace, address)
                                        .await
                                        .map_err(|error| error.to_string())?
                                        .map(|holder| {
                                            let labels = &holder.object.metadata.labels;
                                            (
                                                labels
                                                    .get(flotilla_resources::CONVOY_LABEL)
                                                    .cloned()
                                                    .unwrap_or_else(|| convoy.metadata.name.clone()),
                                                labels.get(flotilla_resources::VESSEL_LABEL).cloned().unwrap_or_default(),
                                                labels
                                                    .get(flotilla_resources::ROLE_LABEL)
                                                    .cloned()
                                                    .unwrap_or_else(|| holder.object.spec.role.clone()),
                                                StallRung::Governor,
                                            )
                                        })
                                }
                                SupervisionTarget::Operator => None,
                            };
                            if let Some((target_convoy, target_vessel, target_role, rung)) = candidate {
                                if target_convoy == convoy.metadata.name
                                    && stalled_source_actor(&condition)
                                        .is_some_and(|(vessel, role)| vessel == target_vessel && role == target_role)
                                {
                                    unavailable_target = Some(target);
                                    continue;
                                }
                                let from = stalled_source_actor(&condition)
                                    .map(|(vessel, role)| format!("{role}@{vessel} in {}", convoy_message_address(convoy)))
                                    .unwrap_or_else(|| convoy_message_address(convoy));
                                let brief = format!("Escalated from {from}:\n\n{}", stall_supervision_brief(convoy, &condition));
                                let delivery = CrewTurnIntent::builder()
                                    .maybe_receiver(match target {
                                        SupervisionTarget::Address { address } => Some(address.clone()),
                                        _ => None,
                                    })
                                    .namespace(namespace.to_string())
                                    .convoy(target_convoy.clone())
                                    .source(supervision_message_source(&convoy.metadata.name, index))
                                    .vessel(target_vessel.clone())
                                    .role(target_role.clone())
                                    .brief(brief)
                                    .subject_revision(condition.began_at.timestamp_micros().to_string())
                                    .sender(
                                        stalled_source_actor(&condition)
                                            .map(|(vessel, role)| {
                                                crew_role_address(
                                                    convoy.spec.project_ref.as_deref().unwrap_or(namespace),
                                                    &convoy.metadata.name,
                                                    vessel,
                                                    role,
                                                )
                                            })
                                            .unwrap_or_else(|| "system:stall-judge".into()),
                                    )
                                    .relation(flotilla_resources::MessageRelation::Supervisor)
                                    .expectation(flotilla_resources::MessageExpectation::Reply)
                                    .references(vec![flotilla_resources::MessageReference::ControlRecord {
                                        resource: flotilla_protocol::ResourceRef::new(
                                            "flotilla.work/v1",
                                            "Convoy",
                                            namespace,
                                            &convoy.metadata.name,
                                        ),
                                        revision: convoy.metadata.resource_version.clone(),
                                    }])
                                    .build();
                                let admission = match self.subscriptions.inner.turn_delivery.lock().await.clone().deliver(&delivery).await {
                                    Ok(admission) => admission,
                                    Err(error) => {
                                        if prior.is_none_or(|prior| {
                                            prior.rung != StallRung::Operator
                                                || !prior.evidence.ends_with(&format!("supervisor delivery failed: {error}"))
                                        }) {
                                            tracing::warn!(
                                                convoy = %convoy.metadata.name,
                                                target = %target_convoy,
                                                %target_vessel,
                                                %target_role,
                                                reason = %error,
                                                brief = %stall_supervision_log_brief(convoy, &condition),
                                                "stall escalation fell back to operator"
                                            );
                                        }
                                        condition.evidence.push_str(&format!("; supervisor delivery failed: {error}"));
                                        condition.supervision_exhausted = false;
                                        delivery_failed = true;
                                        break;
                                    }
                                };
                                condition.supervision_message = Some(admission.message);
                                condition.rung = rung;
                                condition.supervision_exhausted = false;
                                condition.supervision_index = Some(index);
                                condition.supervisor = Some(StallSupervisor {
                                    convoy: target_convoy.clone(),
                                    vessel: target_vessel.clone(),
                                    role: target_role.clone(),
                                });
                                condition.maker =
                                    Some(LeafMaker::Supervisor { convoy: target_convoy, vessel: target_vessel, role: target_role });
                                break;
                            }
                            if matches!(target, SupervisionTarget::Operator) {
                                if unavailable_target.is_none() {
                                    condition.supervision_index = Some(index);
                                }
                                break;
                            }
                            unavailable_target = Some(target);
                        }
                        if condition.supervisor.is_none() && !delivery_failed {
                            let target = unavailable_target.unwrap_or(&SupervisionTarget::Operator);
                            if prior.is_none_or(|prior| {
                                prior.rung != condition.rung
                                    || prior.supervisor != condition.supervisor
                                    || prior.evidence != condition.evidence
                            }) {
                                tracing::warn!(
                                    convoy = %convoy.metadata.name,
                                    ?target,
                                    reason = if unavailable_target.is_some() { "supervisor_lookup_failed" }
                                        else if start >= policy.len() { "supervision_policy_exhausted" }
                                        else { "operator_rung_selected" },
                                    supervision_start = start,
                                    supervision_policy_len = policy.len(),
                                    evidence = %condition.evidence,
                                    brief = %stall_supervision_log_brief(convoy, &condition),
                                    "stall escalation fell back to operator"
                                );
                            }
                            if unavailable_target.is_some() {
                                self.subscriptions.inner.supervisor_context.lock().await.insert(context_key, context);
                                // Operator attention is a backstop while supervisors reconnect,
                                // not a consumed ladder. Try the same unconsumed rungs next pass.
                                condition.supervision_exhausted = false;
                            } else {
                                // An empty or fully consumed policy ends at the operator.
                                condition.supervision_index = Some(policy.len());
                            }
                        }
                    }
                }
                Some(condition)
            } else {
                status
                    .stalled
                    .as_ref()
                    .filter(|stalled| {
                        status.phase == ConvoyPhase::Active
                            && actionable_rows > 0
                            && (stalled.supervisor.is_some()
                                || (stalled.rung == StallRung::Operator && stalled.source == StallEvidenceSource::Crew))
                    })
                    .cloned()
            };
            if status.nudge_obligations != obligations {
                flotilla_resources::apply_status_patch(
                    &backend.clone().using::<Convoy>(namespace),
                    &convoy.metadata.name,
                    &flotilla_resources::ConvoyStatusPatch::SetNudgeObligations { obligations },
                )
                .await
                .map_err(|error| error.to_string())?;
            }
            if status.stalled != next {
                if let Err(error) = flotilla_resources::apply_status_patch(
                    &backend.clone().using::<Convoy>(namespace),
                    &convoy.metadata.name,
                    &flotilla_resources::ConvoyStatusPatch::SetStalled { condition: next },
                )
                .await
                {
                    tracing::warn!(namespace, convoy = %convoy.metadata.name, %error, "write stalled condition failed");
                }
            }
        }
        Ok(())
    }
}
