use std::collections::{BTreeMap, BTreeSet};

use chrono::{TimeZone, Utc};
use flotilla_protocol::{CanonicalHostId, PlacementDecision, PlacementTargetHost};
use flotilla_resources::{
    controller_patches, external_patches, provisioning_patches, ConvoyPhase, ConvoyProvisioningState, ConvoyStatus, ConvoyStatusPatch,
    CrewSource, CrewSpec, CrewWorkPhase, CrewWorkState, PendingBrief, Selector, StatusPatch, VesselRequirement, WorkCompletionAuthority,
    WorkPhase, WorkState, WorkflowSnapshot,
};

fn ts(seconds: i64) -> chrono::DateTime<Utc> {
    Utc.timestamp_opt(seconds, 0).single().expect("valid timestamp")
}

fn sample_snapshot() -> WorkflowSnapshot {
    WorkflowSnapshot {
        cascade: None,
        stall_nudges: Default::default(),
        supervision: None,
        exit: None,
        turn_delivery: Default::default(),
        vessels: vec![
            VesselRequirement {
                name: "implement".to_string(),
                depends_on: Vec::new(),
                repository_refs: None,
                credential_refs: Default::default(),
                credential_scopes: Default::default(),
                credential_permissions: Default::default(),
                crew: vec![
                    CrewSpec {
                        skills: Default::default(),
                        needs: Default::default(),
                        role: "coder".to_string(),
                        source: CrewSource::Agent {
                            selector: Selector::for_capability("code"),
                            prompt: Some("Implement {{inputs.feature}}".to_string()),
                            brief_template: None,
                        },
                        labels: BTreeMap::new(),
                        completion_conditions: Vec::new(),
                        promises: Vec::new(),
                        promise_exclusions: Vec::new(),
                        deliverer: false,
                    },
                    CrewSpec {
                        skills: Default::default(),
                        needs: Default::default(),
                        role: "build".to_string(),
                        source: CrewSource::Tool { command: "cargo test".to_string() },
                        labels: BTreeMap::new(),
                        completion_conditions: Vec::new(),
                        promises: Vec::new(),
                        promise_exclusions: Vec::new(),
                        deliverer: false,
                    },
                ],
            },
            VesselRequirement {
                name: "review".to_string(),
                depends_on: vec!["implement".to_string()],
                repository_refs: None,
                credential_refs: Default::default(),
                credential_scopes: Default::default(),
                credential_permissions: Default::default(),
                crew: vec![CrewSpec {
                    skills: Default::default(),
                    needs: Default::default(),
                    role: "reviewer".to_string(),
                    source: CrewSource::Agent {
                        selector: Selector::for_capability("code-review"),
                        prompt: Some("Review {{inputs.feature}}".to_string()),
                        brief_template: None,
                    },
                    labels: BTreeMap::new(),
                    completion_conditions: Vec::new(),
                    promises: Vec::new(),
                    promise_exclusions: Vec::new(),
                    deliverer: false,
                }],
            },
        ],
    }
}

fn pending_work() -> WorkState {
    WorkState {
        provisioning_retry: None,
        phase: WorkPhase::Pending,
        completion_authority: WorkCompletionAuthority::CrewRollup,
        ready_at: None,
        started_at: None,
        finished_at: None,
        message: None,
        placement: None,
    }
}

fn crew_work(phase: CrewWorkPhase) -> CrewWorkState {
    CrewWorkState::builder().phase(phase).started_at(ts(10)).build()
}

fn queue_pending_brief(status: &mut ConvoyStatus, role: &str) {
    // Seed a previous-generation record; current writers cannot queue PendingBrief.
    status.turn_deliveries.entry("operator".into()).or_default().pending_brief = Some(
        PendingBrief::builder().vessel("implement".into()).role(role.into()).content("address review".into()).queued_at(ts(15)).build(),
    );
}

#[test]
fn crew_failure_clears_its_pending_brief() {
    let mut status = ConvoyStatus {
        stalled: None,
        nudge_obligations: Vec::new(),
        phase: ConvoyPhase::Active,
        crew_work: BTreeMap::from([("implement".to_string(), BTreeMap::from([("coder".to_string(), crew_work(CrewWorkPhase::Working))]))]),
        ..ConvoyStatus::default()
    };
    queue_pending_brief(&mut status, "coder");

    external_patches::mark_crew_failed("implement".to_string(), "coder".to_string(), ts(20), "session failed".to_string())
        .apply(&mut status);

    assert_eq!(status.crew_work["implement"]["coder"].phase, CrewWorkPhase::Failed);
    assert!(status.pending_brief().is_none());
}

#[test]
fn crew_handoff_clears_the_senders_pending_brief() {
    let mut status = ConvoyStatus {
        phase: ConvoyPhase::Active,
        crew_work: BTreeMap::from([(
            "implement".to_string(),
            BTreeMap::from([
                ("coder".to_string(), crew_work(CrewWorkPhase::Working)),
                ("reviewer".to_string(), crew_work(CrewWorkPhase::Done)),
            ]),
        )]),
        ..ConvoyStatus::default()
    };
    queue_pending_brief(&mut status, "coder");

    external_patches::handoff_crew_work(
        "implement".to_string(),
        "coder".to_string(),
        "reviewer".to_string(),
        ts(20),
        "ready for review".to_string(),
    )
    .apply(&mut status);

    assert_eq!(status.crew_work["implement"]["coder"].phase, CrewWorkPhase::HandedBack);
    assert_eq!(status.crew_work["implement"]["reviewer"].phase, CrewWorkPhase::Working);
    assert!(status.pending_brief().is_none());
}

#[test]
fn kickoff_handoff_preserves_the_working_senders_pending_brief() {
    let mut status = ConvoyStatus {
        phase: ConvoyPhase::Active,
        crew_work: BTreeMap::from([(
            "implement".to_string(),
            BTreeMap::from([
                ("coder".to_string(), crew_work(CrewWorkPhase::Working)),
                ("reviewer".to_string(), crew_work(CrewWorkPhase::Pending)),
            ]),
        )]),
        ..ConvoyStatus::default()
    };
    queue_pending_brief(&mut status, "coder");

    external_patches::handoff_crew_work(
        "implement".to_string(),
        "coder".to_string(),
        "reviewer".to_string(),
        ts(20),
        "please review".to_string(),
    )
    .apply(&mut status);

    assert_eq!(status.crew_work["implement"]["coder"].phase, CrewWorkPhase::Working);
    assert_eq!(status.crew_work["implement"]["reviewer"].phase, CrewWorkPhase::Working);
    assert_eq!(status.pending_brief().map(|brief| brief.role.as_str()), Some("coder"));
}

#[test]
fn terminal_convoy_phase_clears_pending_brief() {
    let mut status = ConvoyStatus { phase: ConvoyPhase::Active, ..ConvoyStatus::default() };
    queue_pending_brief(&mut status, "coder");

    controller_patches::roll_up_phase(ConvoyPhase::Landed, None, Some(ts(20))).apply(&mut status);

    assert_eq!(status.phase, ConvoyPhase::Landed);
    assert!(status.pending_brief().is_none());
}

#[test]
fn placement_decision_is_written_once_without_overwriting_concurrent_status() {
    let first = PlacementDecision {
        minimal_alternatives: Vec::new(),
        escalation_reason: None,
        policy_name: "host-direct-kiwi".to_string(),
        target_host: PlacementTargetHost { reference: CanonicalHostId::resolved("kiwi-id"), display_name: "kiwi".to_string() },
        refused_candidates: Vec::new(),
        viable_not_selected: Vec::new(),
        allocation: None,
    };
    let second = PlacementDecision {
        minimal_alternatives: Vec::new(),
        escalation_reason: None,
        policy_name: "host-direct-feta".to_string(),
        target_host: PlacementTargetHost { reference: CanonicalHostId::resolved("feta-id"), display_name: "feta".to_string() },
        refused_candidates: Vec::new(),
        viable_not_selected: Vec::new(),
        allocation: None,
    };
    let mut status =
        ConvoyStatus { phase: ConvoyPhase::Active, observed_workflow_ref: Some("scratch".to_string()), ..ConvoyStatus::default() };

    ConvoyStatusPatch::SetPlacementDecision { placement_decision: first.clone() }.apply(&mut status);
    ConvoyStatusPatch::SetPlacementDecision { placement_decision: second }.apply(&mut status);

    assert_eq!(status.placement_decision, Some(first));
    assert_eq!(status.phase, ConvoyPhase::Active);
    assert_eq!(status.observed_workflow_ref.as_deref(), Some("scratch"));
}

// Discovery completes only for accepted produced change requests. The generator
// spans every source, relationship, both subject kinds and operator unlink overrides.
#[hegel::test]
fn discovery_completion_respects_subject_kind_relationship_and_unlinks(tc: hegel::TestCase) {
    use flotilla_protocol::{provider_data::IssueSource, Relationship, Subject, SubjectKind};
    use flotilla_resources::SubjectDiscoverySource;
    let source =
        [SubjectDiscoverySource::Branch, SubjectDiscoverySource::Claim, SubjectDiscoverySource::Relay, SubjectDiscoverySource::Operator]
            [tc.draw(hegel::generators::integers::<usize>().min_value(0).max_value(3))];
    let relationship =
        [Relationship::Produces, Relationship::Adopts, Relationship::WorksOn, Relationship::Supersedes, Relationship::References]
            [tc.draw(hegel::generators::integers::<usize>().min_value(0).max_value(4))];
    let kind = if tc.draw(hegel::generators::booleans()) { SubjectKind::ChangeRequest } else { SubjectKind::Issue };
    let unlinked = tc.draw(hegel::generators::booleans());
    let subject = Subject { kind, source: IssueSource { service: "github.com".into(), scope: "team/repo".into() }, id: "42".into() };
    let mut status = ConvoyStatus { branch_subject_scan_error: Some("scan failed".into()), ..Default::default() };
    if unlinked {
        status.unlink_subject(&subject);
    }
    let completes = kind == SubjectKind::ChangeRequest
        && relationship == Relationship::Produces
        && (!unlinked || source == SubjectDiscoverySource::Operator);
    for at in [ts(10), ts(20)] {
        ConvoyStatusPatch::DiscoverSubjects { subjects: vec![(subject.clone(), relationship)], source, at }.apply(&mut status);
        assert_eq!(status.branch_subject_scan_at, completes.then_some(at));
        assert_eq!(status.branch_subject_scan_error.is_none(), completes);
    }
    // Every generated sequence ends with an accepted produced PR so completion
    // is exercised even when the random prefix contains only unrelated evidence.
    let subject = Subject { kind: SubjectKind::ChangeRequest, ..subject };
    ConvoyStatusPatch::DiscoverSubjects {
        subjects: vec![(subject, Relationship::Produces)],
        source: SubjectDiscoverySource::Operator,
        at: ts(30),
    }
    .apply(&mut status);
    assert_eq!(status.branch_subject_scan_at, Some(ts(30)));
    assert!(status.branch_subject_scan_error.is_none());
}

#[test]
fn successful_branch_scan_clears_a_stale_lookup_error() {
    let mut status = ConvoyStatus::default();
    ConvoyStatusPatch::RecordBranchSubjectScan { at: ts(10) }.apply(&mut status);
    ConvoyStatusPatch::RecordBranchSubjectScanFailure { error: "forge temporarily unavailable".into() }.apply(&mut status);
    assert_eq!(status.branch_subject_scan_error.as_deref(), Some("forge temporarily unavailable"));
    ConvoyStatusPatch::RecordBranchSubjectScan { at: ts(20) }.apply(&mut status);
    assert_eq!(status.branch_subject_scan_at, Some(ts(20)));
    assert!(status.branch_subject_scan_error.is_none());
}

#[test]
fn abandon_convoy_stamps_convoy_and_open_work() {
    let mut status = ConvoyStatus {
        promises: Default::default(),
        landing_entry: None,
        landing_settlement: None,
        environment_observations: Default::default(),
        ensure_admission: None,
        unlinked_subjects: Vec::new(),
        subjects: Vec::new(),
        branch_subject_scan_at: None,
        branch_subject_scan_error: None,
        stalled: None,
        nudge_obligations: Vec::new(),
        provisioning: None,
        placement_decision: None,
        phase: ConvoyPhase::Active,
        workflow_snapshot: Some(sample_snapshot()),
        work: BTreeMap::from([
            ("implement".to_string(), WorkState { phase: WorkPhase::Running, ..pending_work() }),
            ("review".to_string(), WorkState { phase: WorkPhase::Complete, finished_at: Some(ts(20)), ..pending_work() }),
        ]),
        crew_work: BTreeMap::new(),
        message: None,
        started_at: Some(ts(1)),
        finished_at: None,
        observed_workflow_ref: Some("review-and-fix".to_string()),
        observed_workflows: Some(BTreeMap::new()),
        disposition: None,
        target_mismatches: Vec::new(),
        turn_deliveries: BTreeMap::new(),
        attention: None,
        lifecycle_mutations: Vec::new(),
    };

    external_patches::mark_convoy_abandoned(
        ConvoyPhase::Active,
        ts(50),
        WorkCompletionAuthority::HumanOverride,
        "superseded by operator".to_string(),
    )
    .apply(&mut status);

    assert_eq!(status.phase, ConvoyPhase::Abandoned);
    assert_eq!(status.finished_at, Some(ts(50)));
    assert_eq!(status.message.as_deref(), Some("abandoned by human override: superseded by operator"));
    assert_eq!(status.work["implement"].phase, WorkPhase::Abandoned);
    assert_eq!(status.work["implement"].completion_authority, WorkCompletionAuthority::HumanOverride);
    assert_eq!(status.work["implement"].message.as_deref(), Some("superseded by operator"));
    assert_eq!(status.work["implement"].finished_at, Some(ts(50)));
    assert_eq!(status.work["review"].phase, WorkPhase::Complete);
}

#[test]
fn abandoned_status_is_immutable_against_stale_and_duplicate_patches() {
    let mut status = ConvoyStatus { phase: ConvoyPhase::Active, ..ConvoyStatus::default() };
    external_patches::mark_convoy_abandoned(
        ConvoyPhase::Active,
        ts(50),
        WorkCompletionAuthority::HumanOverride,
        "first reason".to_string(),
    )
    .apply(&mut status);
    let abandoned = status.clone();

    controller_patches::roll_up_phase(ConvoyPhase::Active, Some(ts(60)), None).apply(&mut status);
    external_patches::mark_convoy_abandoned(
        ConvoyPhase::Active,
        ts(70),
        WorkCompletionAuthority::HumanOverride,
        "replacement reason".to_string(),
    )
    .apply(&mut status);

    assert_eq!(status, abandoned);
}

#[test]
fn terminal_convoy_outcomes_survive_stale_status_patches() {
    for phase in [ConvoyPhase::Landed, ConvoyPhase::Failed, ConvoyPhase::Abandoned] {
        let settled = ConvoyStatus {
            phase,
            finished_at: Some(ts(50)),
            message: Some("settled".to_string()),
            work: BTreeMap::from([("implement".to_string(), pending_work())]),
            ..ConvoyStatus::default()
        };
        let stale = [
            controller_patches::roll_up_phase(ConvoyPhase::Active, Some(ts(60)), None),
            controller_patches::fail_convoy(BTreeMap::from([("implement".to_string(), ts(60))]), ts(60), Some("stale".to_string())),
            ConvoyStatusPatch::Settle {
                evidence: None,
                disposition: "merged".to_string(),
                target_mismatches: Vec::new(),
                finished_at: ts(60),
            },
            external_patches::mark_convoy_abandoned(
                if phase == ConvoyPhase::Failed { ConvoyPhase::Active } else { ConvoyPhase::Failed },
                ts(60),
                WorkCompletionAuthority::HumanOverride,
                "stale".to_string(),
            ),
        ];
        for patch in stale {
            let mut status = settled.clone();
            patch.apply(&mut status);
            assert_eq!(status, settled, "{phase:?} changed after {patch:?}");
        }
    }
}

#[test]
fn operator_can_abandon_the_terminal_phase_it_observed() {
    let mut status = ConvoyStatus { phase: ConvoyPhase::Failed, finished_at: Some(ts(20)), ..ConvoyStatus::default() };

    external_patches::mark_convoy_abandoned(
        ConvoyPhase::Failed,
        ts(30),
        WorkCompletionAuthority::HumanOverride,
        "reclaim override".to_string(),
    )
    .apply(&mut status);

    assert_eq!(status.phase, ConvoyPhase::Abandoned);
    assert_eq!(status.finished_at, Some(ts(30)));
}

#[test]
fn abandon_does_not_apply_when_a_nonterminal_phase_changes() {
    let original = ConvoyStatus { phase: ConvoyPhase::Landing, started_at: Some(ts(10)), ..ConvoyStatus::default() };
    let mut status = original.clone();

    external_patches::mark_convoy_abandoned(
        ConvoyPhase::Active,
        ts(30),
        WorkCompletionAuthority::HumanOverride,
        "stale request".to_string(),
    )
    .apply(&mut status);

    assert_eq!(status, original);
}

#[test]
fn one_shot_work_patches_preserve_existing_terminal_outcomes() {
    for phase in [WorkPhase::Complete, WorkPhase::Failed, WorkPhase::Cancelled, WorkPhase::Abandoned] {
        let original = WorkState { phase, finished_at: Some(ts(20)), message: Some("original outcome".to_string()), ..pending_work() };
        let patches = [
            (WorkPhase::Complete, external_patches::force_work_completed("implement".to_string(), ts(30), Some("stale".to_string()))),
            (
                WorkPhase::Failed,
                ConvoyStatusPatch::MarkWorkFailed { work: "implement".to_string(), finished_at: ts(30), message: "stale".to_string() },
            ),
            (WorkPhase::Cancelled, ConvoyStatusPatch::MarkWorkCancelled { work: "implement".to_string(), finished_at: ts(30) }),
        ];
        for (target, patch) in patches {
            if phase == target {
                continue;
            }
            let mut status = ConvoyStatus {
                phase: ConvoyPhase::Active,
                work: BTreeMap::from([("implement".to_string(), original.clone())]),
                ..ConvoyStatus::default()
            };
            patch.apply(&mut status);
            assert_eq!(status.work["implement"], original, "{phase:?} changed after {patch:?}");
        }

        let mut status = ConvoyStatus {
            phase: ConvoyPhase::Active,
            work: BTreeMap::from([("implement".to_string(), original.clone())]),
            ..ConvoyStatus::default()
        };
        controller_patches::fail_convoy(BTreeMap::from([("implement".to_string(), ts(30))]), ts(30), None).apply(&mut status);
        assert_eq!(status.phase, ConvoyPhase::Failed);
        assert_eq!(status.work["implement"], original, "{phase:?} changed during fail-fast cancellation");
    }
}

#[test]
fn crew_completion_updates_only_the_calling_agent() {
    let mut status = ConvoyStatus {
        promises: Default::default(),
        landing_entry: None,
        landing_settlement: None,
        environment_observations: Default::default(),
        ensure_admission: None,
        unlinked_subjects: Vec::new(),
        subjects: Vec::new(),
        branch_subject_scan_at: None,
        branch_subject_scan_error: None,
        stalled: None,
        nudge_obligations: Vec::new(),
        provisioning: None,
        placement_decision: None,
        phase: ConvoyPhase::Active,
        workflow_snapshot: Some(sample_snapshot()),
        work: BTreeMap::from([(
            "implement".to_string(),
            WorkState {
                provisioning_retry: None,
                phase: WorkPhase::Running,
                completion_authority: WorkCompletionAuthority::CrewRollup,
                ready_at: Some(ts(8)),
                started_at: Some(ts(9)),
                finished_at: None,
                message: None,
                placement: None,
            },
        )]),
        crew_work: BTreeMap::from([(
            "implement".to_string(),
            BTreeMap::from([
                ("coder".to_string(), crew_work(CrewWorkPhase::Working)),
                ("reviewer".to_string(), crew_work(CrewWorkPhase::Working)),
            ]),
        )]),
        message: None,
        started_at: Some(ts(1)),
        finished_at: None,
        observed_workflow_ref: Some("review-and-fix".to_string()),
        observed_workflows: Some(BTreeMap::new()),
        disposition: None,
        target_mismatches: Vec::new(),
        turn_deliveries: BTreeMap::new(),
        attention: None,
        lifecycle_mutations: Vec::new(),
    };

    external_patches::mark_crew_completed(
        "implement".to_string(),
        "coder".to_string(),
        ts(20),
        Some("ready for review".to_string()),
        Some("changes-pushed".to_string()),
        Some("https://example.test/pull/1#comment-2".to_string()),
    )
    .apply(&mut status);
    external_patches::mark_crew_completed(
        "implement".to_string(),
        "coder".to_string(),
        ts(30),
        Some("still ready".to_string()),
        None,
        None,
    )
    .apply(&mut status);

    assert_eq!(status.crew_work["implement"]["coder"].phase, CrewWorkPhase::Done);
    assert_eq!(status.crew_work["implement"]["coder"].finished_at, Some(ts(20)));
    assert_eq!(status.crew_work["implement"]["coder"].message.as_deref(), Some("still ready"));
    assert_eq!(status.crew_work["implement"]["coder"].disposition.as_deref(), Some("changes-pushed"));
    assert_eq!(status.crew_work["implement"]["coder"].decision_ledger_ref.as_deref(), Some("https://example.test/pull/1#comment-2"));
    assert_eq!(status.crew_work["implement"]["reviewer"].phase, CrewWorkPhase::Working);
    assert_eq!(status.work["implement"].phase, WorkPhase::Running);
    assert_eq!(status.phase, ConvoyPhase::Active);
}

#[test]
fn final_crew_completion_claim_enters_landing_idempotently() {
    let mut status = ConvoyStatus {
        promises: Default::default(),
        landing_entry: None,
        landing_settlement: None,
        environment_observations: Default::default(),
        ensure_admission: None,
        unlinked_subjects: Vec::new(),
        subjects: Vec::new(),
        branch_subject_scan_at: None,
        branch_subject_scan_error: None,
        stalled: None,
        nudge_obligations: Vec::new(),
        provisioning: None,
        placement_decision: None,
        phase: ConvoyPhase::Active,
        workflow_snapshot: Some(sample_snapshot()),
        work: BTreeMap::from([(
            "implement".to_string(),
            WorkState {
                provisioning_retry: None,
                phase: WorkPhase::Running,
                completion_authority: WorkCompletionAuthority::CrewRollup,
                ready_at: Some(ts(8)),
                started_at: Some(ts(9)),
                finished_at: None,
                message: None,
                placement: None,
            },
        )]),
        crew_work: BTreeMap::from([("implement".to_string(), BTreeMap::from([("coder".to_string(), crew_work(CrewWorkPhase::Working))]))]),
        message: None,
        started_at: Some(ts(1)),
        finished_at: None,
        observed_workflow_ref: Some("review-and-fix".to_string()),
        observed_workflows: Some(BTreeMap::new()),
        disposition: None,
        target_mismatches: Vec::new(),
        turn_deliveries: BTreeMap::new(),
        attention: None,
        lifecycle_mutations: Vec::new(),
    };

    let patch =
        external_patches::mark_crew_completed("implement".to_string(), "coder".to_string(), ts(20), Some("ready".to_string()), None, None);
    patch.apply(&mut status);
    patch.apply(&mut status);

    assert_eq!(status.phase, ConvoyPhase::Landing);
    assert_eq!(status.finished_at, None);
}

#[test]
fn stall_after_completion_preserves_done_and_landing() {
    let mut status = ConvoyStatus {
        phase: ConvoyPhase::Active,
        work: BTreeMap::from([("implement".to_string(), WorkState::builder().phase(WorkPhase::Running).build())]),
        crew_work: BTreeMap::from([("implement".to_string(), BTreeMap::from([("coder".to_string(), crew_work(CrewWorkPhase::Working))]))]),
        ..Default::default()
    };
    external_patches::mark_crew_completed(
        "implement".to_string(),
        "coder".to_string(),
        ts(20),
        Some("https://github.com/flotilla-org/flotilla/pull/1".to_string()),
        None,
        None,
    )
    .apply(&mut status);
    let completed = status.clone();
    assert_eq!(completed.phase, ConvoyPhase::Landing);

    external_patches::mark_crew_stalled(
        "convoy".to_string(),
        "implement".to_string(),
        "coder".to_string(),
        ts(21),
        flotilla_protocol::StallReason::Scope,
        None,
        "settlement waits for change request merge".to_string(),
    )
    .apply(&mut status);

    assert_eq!(status, completed);
}

#[test]
fn crew_failure_records_terminal_state_and_message() {
    let mut status = ConvoyStatus {
        promises: Default::default(),
        landing_entry: None,
        landing_settlement: None,
        environment_observations: Default::default(),
        ensure_admission: None,
        unlinked_subjects: Vec::new(),
        subjects: Vec::new(),
        branch_subject_scan_at: None,
        branch_subject_scan_error: None,
        stalled: None,
        nudge_obligations: Vec::new(),
        provisioning: None,
        placement_decision: None,
        phase: ConvoyPhase::Active,
        workflow_snapshot: Some(sample_snapshot()),
        work: BTreeMap::new(),
        crew_work: BTreeMap::from([("implement".to_string(), BTreeMap::from([("coder".to_string(), crew_work(CrewWorkPhase::Working))]))]),
        message: None,
        started_at: Some(ts(1)),
        finished_at: None,
        observed_workflow_ref: Some("review-and-fix".to_string()),
        observed_workflows: Some(BTreeMap::new()),
        disposition: None,
        target_mismatches: Vec::new(),
        turn_deliveries: BTreeMap::new(),
        attention: None,
        lifecycle_mutations: Vec::new(),
    };

    external_patches::mark_crew_completed(
        "implement".to_string(),
        "coder".to_string(),
        ts(15),
        Some("initially done".to_string()),
        None,
        None,
    )
    .apply(&mut status);
    external_patches::mark_crew_failed("implement".to_string(), "coder".to_string(), ts(20), "blocked by missing credentials".to_string())
        .apply(&mut status);
    external_patches::mark_crew_failed("implement".to_string(), "coder".to_string(), ts(30), "still blocked".to_string())
        .apply(&mut status);

    assert_eq!(status.crew_work["implement"]["coder"].phase, CrewWorkPhase::Failed);
    assert_eq!(status.crew_work["implement"]["coder"].finished_at, Some(ts(20)));
    assert_eq!(status.crew_work["implement"]["coder"].message.as_deref(), Some("still blocked"));
}

#[test]
fn handoff_to_done_crew_reopens_target_and_marks_sender_handed_back() {
    let mut coder = crew_work(CrewWorkPhase::Done);
    coder.finished_at = Some(ts(15));
    let mut status = ConvoyStatus {
        promises: Default::default(),
        landing_entry: None,
        landing_settlement: None,
        environment_observations: Default::default(),
        ensure_admission: None,
        unlinked_subjects: Vec::new(),
        subjects: Vec::new(),
        branch_subject_scan_at: None,
        branch_subject_scan_error: None,
        stalled: None,
        nudge_obligations: Vec::new(),
        provisioning: None,
        placement_decision: None,
        phase: ConvoyPhase::Landing,
        workflow_snapshot: Some(sample_snapshot()),
        work: BTreeMap::from([(
            "implement".to_string(),
            WorkState {
                provisioning_retry: None,
                phase: WorkPhase::Complete,
                completion_authority: WorkCompletionAuthority::HumanOverride,
                ready_at: Some(ts(8)),
                started_at: Some(ts(9)),
                finished_at: Some(ts(16)),
                message: Some("complete".to_string()),
                placement: None,
            },
        )]),
        crew_work: BTreeMap::from([(
            "implement".to_string(),
            BTreeMap::from([("coder".to_string(), coder), ("reviewer".to_string(), crew_work(CrewWorkPhase::Working))]),
        )]),
        message: None,
        started_at: Some(ts(1)),
        finished_at: None,
        observed_workflow_ref: Some("review-and-fix".to_string()),
        observed_workflows: Some(BTreeMap::new()),
        disposition: None,
        target_mismatches: Vec::new(),
        turn_deliveries: BTreeMap::new(),
        attention: None,
        lifecycle_mutations: Vec::new(),
    };

    external_patches::handoff_crew_work(
        "implement".to_string(),
        "reviewer".to_string(),
        "coder".to_string(),
        ts(20),
        "address review findings".to_string(),
    )
    .apply(&mut status);

    assert_eq!(status.work["implement"].completion_authority, WorkCompletionAuthority::CrewRollup);
    assert_eq!(status.crew_work["implement"]["coder"].phase, CrewWorkPhase::Working);
    assert_eq!(status.crew_work["implement"]["coder"].finished_at, None);
    assert_eq!(status.crew_work["implement"]["reviewer"].phase, CrewWorkPhase::HandedBack);
    assert_eq!(status.crew_work["implement"]["reviewer"].finished_at, Some(ts(20)));
    assert_eq!(status.work["implement"].phase, WorkPhase::Complete);
}

#[test]
fn resume_reopens_completed_crew_without_restarting_its_timeline() {
    let mut coder = crew_work(CrewWorkPhase::Done);
    coder.finished_at = Some(ts(15));
    coder.message = Some("ready".to_string());
    let mut status = ConvoyStatus {
        promises: Default::default(),
        landing_entry: None,
        landing_settlement: None,
        environment_observations: Default::default(),
        ensure_admission: None,
        unlinked_subjects: Vec::new(),
        subjects: Vec::new(),
        branch_subject_scan_at: None,
        branch_subject_scan_error: None,
        stalled: None,
        nudge_obligations: Vec::new(),
        provisioning: None,
        placement_decision: None,
        phase: ConvoyPhase::Landing,
        workflow_snapshot: Some(sample_snapshot()),
        work: BTreeMap::from([(
            "implement".to_string(),
            WorkState {
                provisioning_retry: None,
                phase: WorkPhase::Complete,
                completion_authority: WorkCompletionAuthority::CrewRollup,
                ready_at: Some(ts(8)),
                started_at: Some(ts(9)),
                finished_at: Some(ts(16)),
                message: Some("complete".to_string()),
                placement: None,
            },
        )]),
        crew_work: BTreeMap::from([("implement".to_string(), BTreeMap::from([("coder".to_string(), coder)]))]),
        message: None,
        started_at: Some(ts(1)),
        finished_at: None,
        observed_workflow_ref: Some("single-agent".to_string()),
        observed_workflows: Some(BTreeMap::new()),
        disposition: None,
        target_mismatches: Vec::new(),
        turn_deliveries: BTreeMap::new(),
        attention: None,
        lifecycle_mutations: Vec::new(),
    };

    external_patches::resume_crew_work("implement".to_string(), "coder".to_string(), ts(20), "Rebase onto main".to_string(), None)
        .apply(&mut status);

    assert_eq!(status.phase, ConvoyPhase::Active);
    assert_eq!(status.work["implement"].phase, WorkPhase::Running);
    assert_eq!(status.work["implement"].finished_at, None);
    assert_eq!(status.crew_work["implement"]["coder"].phase, CrewWorkPhase::Working);
    assert_eq!(status.crew_work["implement"]["coder"].started_at, Some(ts(10)));
    assert_eq!(status.crew_work["implement"]["coder"].finished_at, None);
    assert_eq!(status.crew_work["implement"]["coder"].message.as_deref(), Some("Rebase onto main"));
}

#[test]
fn running_vessel_work_starts_pending_agents_without_reopening_done_agents() {
    let mut pending_coder = crew_work(CrewWorkPhase::Pending);
    pending_coder.started_at = None;
    let mut status = ConvoyStatus {
        promises: Default::default(),
        landing_entry: None,
        landing_settlement: None,
        environment_observations: Default::default(),
        ensure_admission: None,
        unlinked_subjects: Vec::new(),
        subjects: Vec::new(),
        branch_subject_scan_at: None,
        branch_subject_scan_error: None,
        stalled: None,
        nudge_obligations: Vec::new(),
        provisioning: None,
        placement_decision: None,
        phase: ConvoyPhase::Active,
        workflow_snapshot: Some(sample_snapshot()),
        work: BTreeMap::from([(
            "implement".to_string(),
            WorkState {
                provisioning_retry: None,
                phase: WorkPhase::Launching,
                completion_authority: WorkCompletionAuthority::CrewRollup,
                ready_at: Some(ts(8)),
                started_at: Some(ts(9)),
                finished_at: None,
                message: None,
                placement: None,
            },
        )]),
        crew_work: BTreeMap::from([(
            "implement".to_string(),
            BTreeMap::from([("coder".to_string(), pending_coder), ("reviewer".to_string(), crew_work(CrewWorkPhase::Done))]),
        )]),
        message: None,
        started_at: Some(ts(1)),
        finished_at: None,
        observed_workflow_ref: Some("review-and-fix".to_string()),
        observed_workflows: Some(BTreeMap::new()),
        disposition: None,
        target_mismatches: Vec::new(),
        turn_deliveries: BTreeMap::new(),
        attention: None,
        lifecycle_mutations: Vec::new(),
    };

    provisioning_patches::work_running("implement".to_string(), ts(12), BTreeSet::from(["coder".to_string()])).apply(&mut status);

    assert_eq!(status.work["implement"].phase, WorkPhase::Running);
    assert_eq!(status.crew_work["implement"]["coder"].phase, CrewWorkPhase::Working);
    assert_eq!(status.crew_work["implement"]["coder"].started_at, Some(ts(12)));
    assert_eq!(status.crew_work["implement"]["reviewer"].phase, CrewWorkPhase::Done);
}

#[test]
fn running_vessel_work_leaves_latent_agents_pending() {
    let mut status = ConvoyStatus {
        promises: Default::default(),
        landing_entry: None,
        landing_settlement: None,
        environment_observations: Default::default(),
        ensure_admission: None,
        unlinked_subjects: Vec::new(),
        subjects: Vec::new(),
        branch_subject_scan_at: None,
        branch_subject_scan_error: None,
        stalled: None,
        nudge_obligations: Vec::new(),
        provisioning: None,
        placement_decision: None,
        phase: ConvoyPhase::Active,
        workflow_snapshot: Some(sample_snapshot()),
        work: BTreeMap::from([(
            "implement".to_string(),
            WorkState {
                provisioning_retry: None,
                phase: WorkPhase::Launching,
                completion_authority: WorkCompletionAuthority::CrewRollup,
                ready_at: Some(ts(8)),
                started_at: Some(ts(9)),
                finished_at: None,
                message: None,
                placement: None,
            },
        )]),
        crew_work: BTreeMap::from([(
            "implement".to_string(),
            // Bootstrapped crew, neither started yet.
            BTreeMap::from([
                ("coder".to_string(), CrewWorkState::builder().phase(CrewWorkPhase::Pending).build()),
                ("reviewer".to_string(), CrewWorkState::builder().phase(CrewWorkPhase::Pending).build()),
            ]),
        )]),
        message: None,
        started_at: Some(ts(1)),
        finished_at: None,
        observed_workflow_ref: Some("review-and-fix".to_string()),
        observed_workflows: Some(BTreeMap::new()),
        disposition: None,
        target_mismatches: Vec::new(),
        turn_deliveries: BTreeMap::new(),
        attention: None,
        lifecycle_mutations: Vec::new(),
    };

    // The vessel launches only the first agent; `reviewer` has no session until
    // someone hands off to it, so reporting it as Working would describe a crew
    // member that does not exist yet.
    provisioning_patches::work_running("implement".to_string(), ts(12), BTreeSet::from(["coder".to_string()])).apply(&mut status);

    assert_eq!(status.crew_work["implement"]["coder"].phase, CrewWorkPhase::Working);
    assert_eq!(status.crew_work["implement"]["coder"].started_at, Some(ts(12)));
    assert_eq!(status.crew_work["implement"]["reviewer"].phase, CrewWorkPhase::Pending);
    assert_eq!(status.crew_work["implement"]["reviewer"].started_at, None, "a latent agent has not started");
}

#[test]
fn bootstrap_sets_snapshot_and_initial_work_map() {
    let mut status = ConvoyStatus::default();
    let mut work = BTreeMap::new();
    work.insert("implement".to_string(), pending_work());
    work.insert("review".to_string(), pending_work());

    let patch = controller_patches::bootstrap(
        sample_snapshot(),
        "review-and-fix".to_string(),
        [("review-and-fix".to_string(), "42".to_string())].into_iter().collect(),
        work.clone(),
        BTreeMap::new(),
        ConvoyPhase::Pending,
        None,
    );

    patch.apply(&mut status);

    assert_eq!(status.phase, ConvoyPhase::Pending);
    assert_eq!(status.workflow_snapshot, Some(sample_snapshot()));
    assert_eq!(status.observed_workflow_ref.as_deref(), Some("review-and-fix"));
    assert_eq!(
        status.observed_workflows.as_ref().expect("observed workflows"),
        &BTreeMap::from([("review-and-fix".to_string(), "42".to_string())])
    );
    assert_eq!(status.work, work);
    assert_eq!(status.provisioning, Some(ConvoyProvisioningState::NotStarted));
}

#[test]
fn advance_work_to_ready_updates_only_selected_vessels() {
    let mut status = ConvoyStatus {
        promises: Default::default(),
        landing_entry: None,
        landing_settlement: None,
        environment_observations: Default::default(),
        ensure_admission: None,
        unlinked_subjects: Vec::new(),
        subjects: Vec::new(),
        branch_subject_scan_at: None,
        branch_subject_scan_error: None,
        stalled: None,
        nudge_obligations: Vec::new(),
        provisioning: None,
        placement_decision: None,
        phase: ConvoyPhase::Pending,
        workflow_snapshot: Some(sample_snapshot()),
        work: BTreeMap::from([
            ("implement".to_string(), pending_work()),
            (
                "review".to_string(),
                WorkState {
                    provisioning_retry: None,
                    phase: WorkPhase::Complete,
                    completion_authority: WorkCompletionAuthority::CrewRollup,
                    ready_at: Some(ts(5)),
                    started_at: Some(ts(6)),
                    finished_at: Some(ts(7)),
                    message: Some("done".to_string()),
                    placement: None,
                },
            ),
        ]),
        crew_work: BTreeMap::new(),
        message: Some("keep".to_string()),
        started_at: None,
        finished_at: None,
        observed_workflow_ref: Some("review-and-fix".to_string()),
        observed_workflows: Some(BTreeMap::from([("review-and-fix".to_string(), "42".to_string())])),
        disposition: None,
        target_mismatches: Vec::new(),
        turn_deliveries: BTreeMap::new(),
        attention: None,
        lifecycle_mutations: Vec::new(),
    };

    let patch = controller_patches::advance_work_to_ready(BTreeMap::from([("implement".to_string(), ts(10))]));
    patch.apply(&mut status);

    assert_eq!(status.work["implement"].phase, WorkPhase::Ready);
    assert_eq!(status.work["implement"].ready_at, Some(ts(10)));
    assert_eq!(status.work["review"].phase, WorkPhase::Complete);
    assert_eq!(status.message.as_deref(), Some("keep"));
    assert_eq!(status.provisioning, Some(ConvoyProvisioningState::Started { started_at: ts(10) }));
}

#[test]
fn init_failure_records_that_provisioning_never_started() {
    let mut status = ConvoyStatus::default();

    ConvoyStatusPatch::FailInit { phase: ConvoyPhase::Failed, message: "invalid workflow".to_string(), finished_at: ts(9) }
        .apply(&mut status);

    assert_eq!(status.provisioning, Some(ConvoyProvisioningState::NotStarted));
}

#[test]
fn fail_convoy_cancels_non_terminal_siblings_and_sets_convoy_failed() {
    let mut status = ConvoyStatus {
        promises: Default::default(),
        landing_entry: None,
        landing_settlement: None,
        environment_observations: Default::default(),
        ensure_admission: None,
        unlinked_subjects: Vec::new(),
        subjects: Vec::new(),
        branch_subject_scan_at: None,
        branch_subject_scan_error: None,
        stalled: None,
        nudge_obligations: Vec::new(),
        provisioning: None,
        placement_decision: None,
        phase: ConvoyPhase::Active,
        workflow_snapshot: Some(sample_snapshot()),
        work: BTreeMap::from([
            (
                "implement".to_string(),
                WorkState {
                    provisioning_retry: None,
                    phase: WorkPhase::Failed,
                    completion_authority: WorkCompletionAuthority::CrewRollup,
                    ready_at: Some(ts(10)),
                    started_at: Some(ts(11)),
                    finished_at: Some(ts(12)),
                    message: Some("boom".to_string()),
                    placement: None,
                },
            ),
            (
                "review".to_string(),
                WorkState {
                    provisioning_retry: None,
                    phase: WorkPhase::Running,
                    completion_authority: WorkCompletionAuthority::CrewRollup,
                    ready_at: Some(ts(20)),
                    started_at: Some(ts(21)),
                    finished_at: None,
                    message: None,
                    placement: None,
                },
            ),
        ]),
        crew_work: BTreeMap::new(),
        message: None,
        started_at: Some(ts(1)),
        finished_at: None,
        observed_workflow_ref: Some("review-and-fix".to_string()),
        observed_workflows: Some(BTreeMap::from([("review-and-fix".to_string(), "42".to_string())])),
        disposition: None,
        target_mismatches: Vec::new(),
        turn_deliveries: BTreeMap::new(),
        attention: None,
        lifecycle_mutations: Vec::new(),
    };

    let patch = controller_patches::fail_convoy(BTreeMap::from([("review".to_string(), ts(30))]), ts(30), Some("work failed".to_string()));
    patch.apply(&mut status);

    assert_eq!(status.phase, ConvoyPhase::Failed);
    assert_eq!(status.finished_at, Some(ts(30)));
    assert_eq!(status.message.as_deref(), Some("work failed"));
    assert_eq!(status.work["implement"].phase, WorkPhase::Failed);
    assert_eq!(status.work["review"].phase, WorkPhase::Cancelled);
    assert_eq!(status.work["review"].finished_at, Some(ts(30)));
}

#[test]
fn roll_up_phase_only_touches_convoy_level_fields() {
    let review = WorkState {
        provisioning_retry: None,
        phase: WorkPhase::Complete,
        completion_authority: WorkCompletionAuthority::CrewRollup,
        ready_at: Some(ts(10)),
        started_at: Some(ts(11)),
        finished_at: Some(ts(12)),
        message: Some("done".to_string()),
        placement: None,
    };
    let mut status = ConvoyStatus {
        promises: Default::default(),
        landing_entry: None,
        landing_settlement: None,
        environment_observations: Default::default(),
        ensure_admission: None,
        unlinked_subjects: Vec::new(),
        subjects: Vec::new(),
        branch_subject_scan_at: None,
        branch_subject_scan_error: None,
        stalled: None,
        nudge_obligations: Vec::new(),
        provisioning: None,
        placement_decision: None,
        phase: ConvoyPhase::Pending,
        workflow_snapshot: Some(sample_snapshot()),
        work: BTreeMap::from([("review".to_string(), review.clone())]),
        crew_work: BTreeMap::new(),
        message: Some("keep".to_string()),
        started_at: None,
        finished_at: None,
        observed_workflow_ref: Some("review-and-fix".to_string()),
        observed_workflows: Some(BTreeMap::from([("review-and-fix".to_string(), "42".to_string())])),
        disposition: None,
        target_mismatches: Vec::new(),
        turn_deliveries: BTreeMap::new(),
        attention: None,
        lifecycle_mutations: Vec::new(),
    };

    let patch = controller_patches::roll_up_phase(ConvoyPhase::Landed, None, Some(ts(40)));
    patch.apply(&mut status);

    assert_eq!(status.phase, ConvoyPhase::Landed);
    assert_eq!(status.finished_at, Some(ts(40)));
    assert_eq!(status.message.as_deref(), Some("keep"));
    assert_eq!(status.work["review"], review);
}

#[test]
fn forced_work_completion_claim_enters_landing() {
    let mut status = ConvoyStatus {
        promises: Default::default(),
        landing_entry: None,
        landing_settlement: None,
        environment_observations: Default::default(),
        ensure_admission: None,
        unlinked_subjects: Vec::new(),
        subjects: Vec::new(),
        branch_subject_scan_at: None,
        branch_subject_scan_error: None,
        stalled: None,
        nudge_obligations: Vec::new(),
        provisioning: None,
        placement_decision: None,
        phase: ConvoyPhase::Active,
        workflow_snapshot: Some(sample_snapshot()),
        work: BTreeMap::from([(
            "review".to_string(),
            WorkState {
                provisioning_retry: None,
                phase: WorkPhase::Running,
                completion_authority: WorkCompletionAuthority::CrewRollup,
                ready_at: Some(ts(10)),
                started_at: Some(ts(11)),
                finished_at: None,
                message: None,
                placement: None,
            },
        )]),
        crew_work: BTreeMap::new(),
        message: None,
        started_at: Some(ts(1)),
        finished_at: None,
        observed_workflow_ref: Some("review-and-fix".to_string()),
        observed_workflows: Some(BTreeMap::from([("review-and-fix".to_string(), "42".to_string())])),
        disposition: None,
        target_mismatches: Vec::new(),
        turn_deliveries: BTreeMap::new(),
        attention: None,
        lifecycle_mutations: Vec::new(),
    };

    let patch =
        ConvoyStatusPatch::ForceWorkCompleted { work: "review".to_string(), finished_at: ts(50), message: Some("done".to_string()) };
    patch.apply(&mut status);

    assert_eq!(status.phase, ConvoyPhase::Landing);
    assert_eq!(status.work["review"].phase, WorkPhase::Complete);
    assert_eq!(status.work["review"].completion_authority, WorkCompletionAuthority::HumanOverride);
    assert_eq!(status.work["review"].finished_at, Some(ts(50)));
    assert_eq!(status.work["review"].message.as_deref(), Some("done"));
}

#[test]
fn forced_work_completion_preserves_agent_owned_state() {
    let mut status = ConvoyStatus {
        promises: Default::default(),
        landing_entry: None,
        landing_settlement: None,
        environment_observations: Default::default(),
        ensure_admission: None,
        unlinked_subjects: Vec::new(),
        subjects: Vec::new(),
        branch_subject_scan_at: None,
        branch_subject_scan_error: None,
        stalled: None,
        nudge_obligations: Vec::new(),
        provisioning: None,
        placement_decision: None,
        phase: ConvoyPhase::Active,
        workflow_snapshot: Some(sample_snapshot()),
        work: BTreeMap::from([(
            "implement".to_string(),
            WorkState {
                provisioning_retry: None,
                phase: WorkPhase::Running,
                completion_authority: WorkCompletionAuthority::CrewRollup,
                ready_at: None,
                started_at: None,
                finished_at: None,
                message: None,
                placement: None,
            },
        )]),
        crew_work: BTreeMap::from([(
            "implement".to_string(),
            BTreeMap::from([
                ("coder".to_string(), crew_work(CrewWorkPhase::Working)),
                ("reviewer".to_string(), crew_work(CrewWorkPhase::HandedBack)),
            ]),
        )]),
        message: None,
        started_at: Some(ts(1)),
        finished_at: None,
        observed_workflow_ref: Some("review-and-fix".to_string()),
        observed_workflows: Some(BTreeMap::new()),
        disposition: None,
        target_mismatches: Vec::new(),
        turn_deliveries: BTreeMap::new(),
        attention: None,
        lifecycle_mutations: Vec::new(),
    };

    external_patches::force_work_completed("implement".to_string(), ts(50), Some("human override".to_string())).apply(&mut status);

    assert_eq!(status.work["implement"].phase, WorkPhase::Complete);
    assert_eq!(status.work["implement"].completion_authority, WorkCompletionAuthority::HumanOverride);
    assert_eq!(status.crew_work["implement"]["coder"].phase, CrewWorkPhase::Working);
    assert_eq!(status.crew_work["implement"]["reviewer"].phase, CrewWorkPhase::HandedBack);
    assert_eq!(status.crew_work["implement"]["reviewer"].finished_at, None);
}

#[test]
fn convoy_lifecycle_timestamps_are_set_once_per_transition() {
    let mut status = ConvoyStatus {
        promises: Default::default(),
        landing_entry: None,
        landing_settlement: None,
        environment_observations: Default::default(),
        ensure_admission: None,
        unlinked_subjects: Vec::new(),
        subjects: Vec::new(),
        branch_subject_scan_at: None,
        branch_subject_scan_error: None,
        stalled: None,
        nudge_obligations: Vec::new(),
        provisioning: None,
        placement_decision: None,
        phase: ConvoyPhase::Pending,
        workflow_snapshot: Some(sample_snapshot()),
        work: BTreeMap::from([("implement".to_string(), pending_work())]),
        crew_work: BTreeMap::new(),
        message: None,
        started_at: None,
        finished_at: None,
        observed_workflow_ref: Some("review-and-fix".to_string()),
        observed_workflows: Some(BTreeMap::new()),
        disposition: None,
        target_mismatches: Vec::new(),
        turn_deliveries: BTreeMap::new(),
        attention: None,
        lifecycle_mutations: Vec::new(),
    };

    ConvoyStatusPatch::AdvanceWorkToReady { ready: BTreeMap::from([("implement".to_string(), ts(10))]) }.apply(&mut status);
    ConvoyStatusPatch::AdvanceWorkToReady { ready: BTreeMap::from([("implement".to_string(), ts(11))]) }.apply(&mut status);
    assert_eq!(status.work["implement"].ready_at, Some(ts(10)));

    ConvoyStatusPatch::WorkLaunching {
        work: "implement".to_string(),
        started_at: ts(20),
        placement: flotilla_resources::PlacementStatus::default(),
    }
    .apply(&mut status);
    ConvoyStatusPatch::WorkLaunching {
        work: "implement".to_string(),
        started_at: ts(21),
        placement: flotilla_resources::PlacementStatus::default(),
    }
    .apply(&mut status);
    assert_eq!(status.work["implement"].started_at, Some(ts(20)));

    ConvoyStatusPatch::ForceWorkCompleted { work: "implement".to_string(), finished_at: ts(30), message: Some("done".to_string()) }
        .apply(&mut status);
    ConvoyStatusPatch::ForceWorkCompleted { work: "implement".to_string(), finished_at: ts(31), message: Some("still done".to_string()) }
        .apply(&mut status);
    assert_eq!(status.work["implement"].finished_at, Some(ts(30)));
    assert_eq!(status.work["implement"].message.as_deref(), Some("still done"));

    ConvoyStatusPatch::RollUpPhase { phase: ConvoyPhase::Active, started_at: Some(ts(40)), finished_at: None }.apply(&mut status);
    ConvoyStatusPatch::RollUpPhase { phase: ConvoyPhase::Active, started_at: Some(ts(41)), finished_at: None }.apply(&mut status);
    assert_eq!(status.started_at, Some(ts(40)));

    ConvoyStatusPatch::RollUpPhase { phase: ConvoyPhase::Landed, started_at: None, finished_at: Some(ts(50)) }.apply(&mut status);
    ConvoyStatusPatch::RollUpPhase { phase: ConvoyPhase::Landed, started_at: None, finished_at: Some(ts(51)) }.apply(&mut status);
    assert_eq!(status.finished_at, Some(ts(50)));
}

#[test]
fn refused_claims_count_only_identical_expectations_and_decode_old_work() {
    let old_work: CrewWorkState = serde_json::from_str(r#"{"phase":"Working"}"#).expect("previous-generation crew work");
    assert!(old_work.completion_refusal.is_none());
    let old_policy: flotilla_resources::StallNudgePolicy =
        serde_json::from_str(r#"{"max_per_episode":2}"#).expect("previous-generation stall nudge policy");
    assert_eq!(old_policy.max_refusals, None);
    let mut status = ConvoyStatus::default();
    status.crew_work.insert("work".into(), BTreeMap::from([("coder".into(), old_work)]));
    let refuse = |expectation: &str| ConvoyStatusPatch::RefuseCrewCompletion {
        vessel: "work".into(),
        role: "coder".into(),
        expectation: expectation.into(),
        causes: Vec::new(),
        message: Some("https://github.com/flotilla-org/flotilla/pull/2200".into()),
    };
    refuse("PR is conflicting").apply(&mut status);
    refuse("PR is conflicting").apply(&mut status);
    assert_eq!(status.crew_work["work"]["coder"].completion_refusal.as_ref().expect("refusal").consecutive_count, 2);
    refuse("PR observation missing").apply(&mut status);
    let refusal = status.crew_work["work"]["coder"].completion_refusal.as_ref().expect("refusal");
    assert_eq!(refusal.consecutive_count, 1);
    assert_eq!(refusal.expectation, "PR observation missing");
}

#[test]
fn nudge_budget_survives_stall_clear_and_resets_only_for_progress_by_that_actor() {
    use flotilla_protocol::{Leaf, LeafAddress, LeafOperator};
    use flotilla_resources::{LeafMaker, NudgeObligation, StallNudge, StallReason};

    let obligation = |role: &str| {
        let leaf = Leaf {
            address: LeafAddress::Work { convoy: "convoy".into(), work: "work".into() },
            field_path: format!(".crew.{role}.phase"),
            operator: LeafOperator::Equal,
            literal: "Done".into(),
        };
        NudgeObligation::builder()
            .maker(LeafMaker::Actor { vessel: "work".into(), role: role.into() })
            .leaves(vec![leaf.clone()])
            .history(vec![StallNudge { at: ts(10), row: leaf }])
            .quiet_since(ts(10))
            .build()
    };
    let mut status = ConvoyStatus {
        phase: ConvoyPhase::Active,
        nudge_obligations: vec![obligation("coder"), obligation("reviewer")],
        ..Default::default()
    };
    ConvoyStatusPatch::SetStalled { condition: None }.apply(&mut status);
    let stored = serde_json::to_string(&status).expect("persist budget");
    status = serde_json::from_str(&stored).expect("restore budget");
    assert_eq!(status.nudge_obligations[0].history.len(), 1, "attention-driven stall clearing preserves accounting");
    ConvoyStatusPatch::RefuseCrewCompletion {
        vessel: "work".into(),
        role: "coder".into(),
        expectation: "checks pass".into(),
        causes: Vec::new(),
        message: None,
    }
    .apply(&mut status);
    assert!(status.nudge_obligations[0].history.is_empty(), "a claim is real progress even when refused");
    assert_eq!(status.nudge_obligations[1].history.len(), 1, "another actor's obligation is independent");
    ConvoyStatusPatch::MarkCrewStalled {
        convoy: "convoy".into(),
        vessel: "work".into(),
        role: "reviewer".into(),
        at: ts(20),
        reason: StallReason::Decision,
        proposed_disposition: None,
        message: "need guidance".into(),
    }
    .apply(&mut status);
    assert!(status.nudge_obligations[1].history.is_empty(), "declaring a stall resets that actor's budget");
    let old_status: ConvoyStatus = serde_json::from_str(r#"{"phase":"Active"}"#).expect("previous-generation convoy");
    assert!(old_status.nudge_obligations.is_empty());
}

// ADR 0047: a previous-generation Convoy with a durable refusal still decodes;
// rewriting it always emits the new causes field, without parsing its explanation.
#[test]
fn previous_generation_convoy_refusal_decodes_and_writes_new_shape() {
    let old = serde_json::json!({
        "phase": "Active",
        "crew_work": {"work": {"coder": {
            "phase": "Working",
            "completion_refusal": {
                "expectation": "could not observe PR 42",
                "consecutive_count": 1,
                "message": "PR URL"
            }
        }}}
    });
    let status: ConvoyStatus = serde_json::from_value(old).expect("previous-generation Convoy status");
    let refusal = status.crew_work["work"]["coder"].completion_refusal.as_ref().expect("refusal retained");
    assert!(refusal.causes.is_empty());
    assert_eq!(refusal.expectation, "could not observe PR 42");
    let rewritten = serde_json::to_value(&status).expect("write current status");
    assert_eq!(rewritten["crew_work"]["work"]["coder"]["completion_refusal"]["causes"], serde_json::json!([]));
}

// #2211: persisted causes retain the complete CR identity through a status patch
// and a stored-data round trip, including combined or repeated remedies.
#[hegel::test]
fn refusal_causes_survive_status_patch_and_storage(tc: hegel::TestCase) {
    use flotilla_resources::CrewCompletionRefusalCause;
    use hegel::generators as gs;

    // Explicitly span absent causes, both variants, duplicates and u64 boundaries.
    let count = tc.draw(gs::integers::<usize>().min_value(0).max_value(4));
    let number = tc.draw(gs::integers::<u64>());
    let causes = (0..count)
        .map(|_| {
            if tc.draw(gs::booleans()) {
                CrewCompletionRefusalCause::ConflictingChangeRequest { service: "github.com".into(), scope: "owner/repo".into(), number }
            } else {
                CrewCompletionRefusalCause::MissingChangeRequestObservation {
                    service: "forge.example".into(),
                    scope: "other/repo".into(),
                    number,
                }
            }
        })
        .collect::<Vec<_>>();
    let mut status = ConvoyStatus::default();
    status
        .crew_work
        .insert("work".into(), BTreeMap::from([("coder".into(), CrewWorkState::builder().phase(CrewWorkPhase::Working).build())]));
    ConvoyStatusPatch::RefuseCrewCompletion {
        vessel: "work".into(),
        role: "coder".into(),
        expectation: "wording is presentation only".into(),
        causes: causes.clone(),
        message: None,
    }
    .apply(&mut status);
    let stored = serde_json::to_string(&status).expect("serialize status");
    let restored: ConvoyStatus = serde_json::from_str(&stored).expect("deserialize status");
    assert_eq!(restored.crew_work["work"]["coder"].completion_refusal.as_ref().expect("refusal").causes, causes);
}

// A failed or suppressed producer restores only its own speculative activation;
// concurrent target changes, other crews and aggregate work remain authoritative.
#[hegel::test]
fn turn_activation_restoration_preserves_concurrent_status(tc: hegel::TestCase) {
    use hegel::generators as gs;
    let phase =
        [CrewWorkPhase::Done, CrewWorkPhase::Stalled, CrewWorkPhase::Pending][tc.draw(gs::integers::<usize>().min_value(0).max_value(2))];
    let target_changed = tc.draw(gs::booleans());
    let other_changed = tc.draw(gs::booleans());
    let work_changed = tc.draw(gs::booleans());
    let previous = ConvoyStatus {
        phase: ConvoyPhase::Landing,
        work: BTreeMap::from([("implement".into(), WorkState::builder().phase(WorkPhase::Complete).build())]),
        crew_work: BTreeMap::from([(
            "implement".into(),
            BTreeMap::from([("coder".into(), crew_work(phase)), ("reviewer".into(), crew_work(CrewWorkPhase::Done))]),
        )]),
        ..Default::default()
    };
    let mut activated = previous.clone();
    external_patches::resume_crew_work("implement".into(), "coder".into(), ts(20), "continue".into(), Some("intent".into()))
        .apply(&mut activated);
    let mut current = activated.clone();
    if target_changed {
        current.crew_work.get_mut("implement").expect("crew").get_mut("coder").expect("target").message = Some("newer action".into());
    }
    if other_changed {
        current.crew_work.get_mut("implement").expect("crew").get_mut("reviewer").expect("other crew").message =
            Some("concurrent review".into());
    }
    if work_changed {
        current.work.get_mut("implement").expect("aggregate work").phase = WorkPhase::Failed;
    }
    let before_restore = current.clone();
    let patch = external_patches::restore_turn_activation("implement".into(), "coder".into(), activated, previous.clone());
    patch.apply(&mut current);
    if target_changed {
        assert_eq!(current, before_restore);
    } else {
        assert_eq!(current.crew_work["implement"]["coder"], previous.crew_work["implement"]["coder"]);
        assert_eq!(current.crew_work["implement"]["reviewer"], before_restore.crew_work["implement"]["reviewer"]);
        if !other_changed && !work_changed {
            assert_eq!(current, previous);
        } else {
            assert_eq!(current.work, before_restore.work);
            assert_eq!(current.phase, ConvoyPhase::Active);
        }
    }
    let restored = current.clone();
    patch.apply(&mut current);
    assert_eq!(current, restored, "restoration is idempotent");
}

// Landing records each contributing claim only once all declared crew claims
// are complete. Later duplicate claims do not rewrite that frozen provenance.
#[hegel::test]
fn landing_claims_wait_for_every_crew_and_remain_frozen(tc: hegel::TestCase) {
    // Cover single/multiple claims, both claim orders, and absent messages.
    let count = tc.draw(hegel::generators::integers::<usize>().min_value(1).max_value(3));
    let reverse = tc.draw(hegel::generators::booleans());
    let with_message = tc.draw(hegel::generators::booleans());
    let roles: Vec<_> = (0..count).map(|index| format!("role-{index}")).collect();
    let mut order = roles.clone();
    if reverse {
        order.reverse();
    }
    let mut status = ConvoyStatus {
        phase: ConvoyPhase::Active,
        work: BTreeMap::from([("work".into(), WorkState::builder().phase(WorkPhase::Running).build())]),
        crew_work: BTreeMap::from([("work".into(), roles.iter().map(|role| (role.clone(), crew_work(CrewWorkPhase::Working))).collect())]),
        ..Default::default()
    };
    for (index, role) in order.iter().enumerate() {
        external_patches::mark_crew_completed_with_context(
            "work".into(),
            role.clone(),
            ts(20 + index as i64),
            with_message.then(|| format!("{role} done")),
            None,
            None,
            Some("digest".into()),
            true,
            None,
        )
        .apply(&mut status);
        assert_eq!(status.phase, if index + 1 == count { ConvoyPhase::Landing } else { ConvoyPhase::Active });
        assert_eq!(status.landing_entry.is_some(), index + 1 == count);
    }
    let entry = status.landing_entry.clone().expect("entry");
    assert_eq!(entry.claims.len(), count);
    let mut prose_entry = entry.clone();
    prose_entry.claims[0].message = Some("See unrelated https://github.com/other/repo/pull/999 for context".into());
    assert!(!prose_entry.reason().contains("(#999)"));
    let mut minimal_entry = serde_json::to_value(&entry).expect("encode entry");
    for claim in minimal_entry["claims"].as_array_mut().expect("claims") {
        let claim = claim.as_object_mut().expect("claim");
        claim.remove("claimed_at");
        claim.remove("message");
        claim.remove("preceding_turn");
    }
    let minimal_entry: flotilla_resources::LandingEntry = serde_json::from_value(minimal_entry).expect("optional claim evidence defaults");
    assert!(minimal_entry
        .claims
        .iter()
        .all(|claim| claim.claimed_at.is_none() && claim.message.is_none() && claim.preceding_turn.is_none()));

    for claim in &entry.claims {
        assert!(roles.contains(&claim.role));
        assert!(claim.preceding_turn.is_none(), "absence of a delivery is explicit");
        assert_eq!(claim.message.is_some(), with_message);
    }
    external_patches::mark_crew_completed_with_context(
        "work".into(),
        roles[0].clone(),
        ts(100),
        Some("duplicate".into()),
        None,
        None,
        Some("digest".into()),
        false,
        None,
    )
    .apply(&mut status);
    assert_eq!(status.landing_entry.as_ref(), Some(&entry));
    let mut legacy = serde_json::to_value(&status).expect("encode");
    legacy.as_object_mut().expect("status object").remove("landing_entry");
    legacy.as_object_mut().expect("status object").remove("landing_settlement");
    for crew in legacy["crew_work"]["work"].as_object_mut().expect("crew object").values_mut() {
        crew.as_object_mut().expect("claim object").remove("completion_turn");
    }
    let decoded: ConvoyStatus = serde_json::from_value(legacy).expect("previous-generation status");
    assert_eq!(decoded.phase, ConvoyPhase::Landing);
    assert!(decoded.landing_entry.is_none() && decoded.landing_settlement.is_none());

    external_patches::resume_crew_work("work".into(), roles[0].clone(), ts(110), "review follow-up".into(), None).apply(&mut status);
    assert_eq!(status.phase, ConvoyPhase::Active);
    assert_eq!(status.landing_entry.as_ref(), Some(&entry), "historical evidence survives reopening");
    external_patches::mark_crew_completed_with_context(
        "work".into(),
        roles[0].clone(),
        ts(120),
        Some("review addressed".into()),
        None,
        None,
        Some("fresh-digest".into()),
        false,
        None,
    )
    .apply(&mut status);
    let fresh = status.landing_entry.as_ref().expect("fresh entry");
    assert_eq!(status.phase, ConvoyPhase::Landing);
    assert_eq!(fresh.entered_at, ts(120));
    assert!(!fresh.event_emitted);
    assert_ne!(fresh, &entry, "re-entry replaces the earlier trigger");
}

// A claim names only a preceding turn for its own role. Future, queued,
// refused, and duplicate/suppressed admissions cannot become its trigger.
#[hegel::test]
fn landing_turn_is_targeted_and_precedes_the_claim(tc: hegel::TestCase) {
    // Cover the time boundary on both sides, every delivery outcome, and
    // missing or unrelated workflow targets.
    let delivered_at = tc.draw(hegel::generators::integers::<i64>().min_value(49).max_value(51));
    let outcome_kind = tc.draw(hegel::generators::integers::<usize>().min_value(0).max_value(4));
    let target_kind = tc.draw(hegel::generators::integers::<usize>().min_value(0).max_value(3));
    let workflow = flotilla_resources::implement_review_workflow_spec();
    let mut snapshot = WorkflowSnapshot {
        cascade: None,
        stall_nudges: Default::default(),
        supervision: None,
        exit: workflow.exit,
        turn_delivery: workflow.turn_delivery,
        vessels: workflow.vessels,
    };
    if target_kind == 1 {
        snapshot.turn_delivery.get_mut("merged-unclaimed").expect("rule").to.role = "reviewer".into();
    } else if target_kind == 2 {
        snapshot.turn_delivery.shift_remove("merged-unclaimed");
    } else if target_kind == 3 {
        snapshot.turn_delivery.get_mut("merged-unclaimed").expect("rule").to.vessel = "other".into();
    }
    let rung = flotilla_resources::TurnDeliveryRung::WarmSession;
    let outcome = match outcome_kind {
        0 => flotilla_resources::TurnDeliveryOutcome::Delivered { rung, delivered_at: ts(delivered_at) },
        1 | 2 => flotilla_resources::TurnDeliveryOutcome::MessageAccepted {
            new_turn: outcome_kind == 1,
            message: flotilla_protocol::ResourceRef::new("flotilla.work/v1", "Message", "flotilla", "turn"),
            rung,
            accepted_at: ts(delivered_at),
        },
        3 => flotilla_resources::TurnDeliveryOutcome::Queued {
            rung,
            queued_at: ts(delivered_at),
            vessel: "work".into(),
            role: "coder".into(),
            message_id: "turn".into(),
            blocking_reason: "active turn".into(),
        },
        _ => flotilla_resources::TurnDeliveryOutcome::Refused {
            reason: "not ready".into(),
            refused_at: ts(delivered_at),
            hold_executed: false,
        },
    };
    let episode = flotilla_resources::TurnDeliveryEpisode::builder()
        .subject_revision("head".into())
        .evidence_at(ts(40))
        .judged_claim_at(ts(20))
        .outcome(outcome)
        .build();
    let mut status = ConvoyStatus {
        phase: ConvoyPhase::Active,
        workflow_snapshot: Some(snapshot),
        work: BTreeMap::from([("work".into(), WorkState::builder().phase(WorkPhase::Running).build())]),
        crew_work: BTreeMap::from([("work".into(), BTreeMap::from([("coder".into(), crew_work(CrewWorkPhase::Working))]))]),
        turn_deliveries: BTreeMap::from([(
            "merged-unclaimed".into(),
            flotilla_resources::TurnDeliveryStatus { episodes: vec![episode.clone()], ..Default::default() },
        )]),
        ..Default::default()
    };
    external_patches::mark_crew_completed_with_context(
        "work".into(),
        "coder".into(),
        ts(50),
        None,
        None,
        None,
        Some("digest".into()),
        false,
        None,
    )
    .apply(&mut status);
    let preceding = &status.landing_entry.expect("entry").claims[0].preceding_turn;
    assert_eq!(preceding.is_some(), target_kind == 0 && outcome_kind <= 1 && delivered_at <= 50);
    if let Some(turn) = preceding {
        assert_eq!(turn.episode, episode);
    }
}
