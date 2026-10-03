use std::collections::BTreeMap;

use flotilla_protocol::{
    result_set::{AwarenessCounts, AwarenessEntry, AwarenessKind, AwarenessLink, AwarenessNode, AwarenessState, CrewMemberSummary},
    HostName, IssueRef, IssueSource, RepoKey, RepositoryKey, ResourceRef,
};
use flotilla_resources::{
    ChangeRequestReviewObservation, ChangeRequestStatus, Observation, ObservedChangeRequestState, ObservedChecks, ObservedMergeability,
    ObservedReviewDecision,
};

use super::*;
use crate::{
    entity,
    keys::{
        KEY_CHANGE_REQUEST_NUMBER, KEY_CHECKOUT_BRANCH, KEY_CHECKOUT_PATH, KEY_CONVOY, KEY_CONVOY_NAME, KEY_COUNT_CHECKOUTS,
        KEY_COUNT_ISSUES, KEY_COUNT_TOTAL, KEY_DISPLAY_LABEL, KEY_DISPLAY_LABEL_MEDIUM, KEY_DISPLAY_LABEL_SHORT, KEY_ENTITY_ID,
        KEY_ENTITY_KIND, KEY_PRIMARY_ACTION_RECIPE, KEY_PRIMARY_ACTION_TARGET, KEY_ROLE, KEY_ROLE_HOLD, KEY_ROLE_NAME, KEY_SOURCE,
        KEY_STATUS_ATTENTION, KEY_STATUS_STATE, KEY_SUMMARY_TEXT, KEY_VESSEL, KEY_WORKSPACE_PRIMARY_STATE, KEY_WORKSPACE_PRIMARY_TARGET,
        SEGMENT_PROJECT, SEGMENT_REPO,
    },
    recipe::FlotillaRecipes,
};

fn mint() -> FlotillaRecipes {
    FlotillaRecipes::new("flotilla")
}

fn convoy_ref(namespace: &str, name: &str) -> ResourceRef {
    ResourceRef::new("flotilla/v1", "Convoy", namespace, name).on_host(HostName::new("kiwi"))
}

#[bon::builder]
fn vessel(convoy: &ResourceRef, name: &str, phase: WorkPhase, materialize: Option<&str>) -> VesselRow {
    VesselRow::builder()
        .resource(convoy.subresource(format!("vessels/{name}")))
        .name(name)
        .phase(phase)
        .host(HostName::new("feta"))
        .maybe_materialize(materialize.map(str::to_owned))
        .build()
}

fn find_entity<'a>(patches: &'a [MetadataPatch], entity: &EntityRef) -> &'a MetadataPatch {
    patches.iter().find(|patch| patch.target == MetadataTarget::Entity(entity.clone())).unwrap_or_else(|| panic!("no patch for {entity:?}"))
}

fn text(patch: &MetadataPatch, key: &str) -> String {
    match &patch.set.get(key).unwrap_or_else(|| panic!("no {key} on {:?}", patch.target)).value {
        MetadataValue::Text(value) => value.clone(),
        other => panic!("{key} is not text: {other:?}"),
    }
}

fn catalog_input(convoys: &[ConvoyRow]) -> CatalogInput<'_> {
    CatalogInput { subjects: None, awareness: None, convoys, independents: &[], standing_roles: &[], project_repositories: &[] }
}

#[derive(bon::Builder)]
struct ReadinessCase {
    name: &'static str,
    state: Option<ObservedChangeRequestState>,
    checks: Option<ObservedChecks>,
    mergeable: Option<ObservedMergeability>,
    review_decision: Option<ObservedReviewDecision>,
    actionable: Option<bool>,
    landed: bool,
    expected: ChangeRequestReadiness,
}

#[test]
fn change_request_readiness_precedence_and_unknown_evidence() {
    use ChangeRequestReadiness as Readiness;
    use ObservedChangeRequestState as State;
    use ObservedChecks as Checks;
    use ObservedMergeability as Mergeability;
    use ObservedReviewDecision as Review;

    let cases = [
        ReadinessCase::builder()
            .name("closed beats failures")
            .state(State::Closed)
            .checks(Checks::Fail)
            .mergeable(Mergeability::Conflicting)
            .review_decision(Review::ChangesRequested)
            .actionable(true)
            .landed(false)
            .expected(Readiness::Closed)
            .build(),
        ReadinessCase::builder()
            .name("merged awaiting landing")
            .state(State::Merged)
            .checks(Checks::Fail)
            .mergeable(Mergeability::Conflicting)
            .review_decision(Review::ChangesRequested)
            .actionable(true)
            .landed(false)
            .expected(Readiness::MergedNotLanded)
            .build(),
        ReadinessCase::builder()
            .name("merged and landed")
            .state(State::Merged)
            .checks(Checks::Pass)
            .mergeable(Mergeability::Mergeable)
            .review_decision(Review::Approved)
            .actionable(false)
            .landed(true)
            .expected(Readiness::Closed)
            .build(),
        ReadinessCase::builder()
            .name("draft beats conflict")
            .state(State::Draft)
            .checks(Checks::Fail)
            .mergeable(Mergeability::Conflicting)
            .review_decision(Review::ChangesRequested)
            .actionable(true)
            .landed(false)
            .expected(Readiness::Draft)
            .build(),
        ReadinessCase::builder()
            .name("conflict beats failing CI")
            .state(State::Open)
            .checks(Checks::Fail)
            .mergeable(Mergeability::Conflicting)
            .review_decision(Review::ChangesRequested)
            .actionable(true)
            .landed(false)
            .expected(Readiness::Conflicting)
            .build(),
        ReadinessCase::builder()
            .name("failing CI beats review")
            .state(State::Open)
            .checks(Checks::Fail)
            .mergeable(Mergeability::Mergeable)
            .review_decision(Review::ChangesRequested)
            .actionable(true)
            .landed(false)
            .expected(Readiness::CiFailing)
            .build(),
        ReadinessCase::builder()
            .name("actionable feedback")
            .state(State::Open)
            .checks(Checks::Pass)
            .mergeable(Mergeability::Mergeable)
            .review_decision(Review::Approved)
            .actionable(true)
            .landed(false)
            .expected(Readiness::AwaitingReviewResponse)
            .build(),
        ReadinessCase::builder()
            .name("changes requested")
            .state(State::Open)
            .checks(Checks::Pass)
            .mergeable(Mergeability::Mergeable)
            .review_decision(Review::ChangesRequested)
            .actionable(false)
            .landed(false)
            .expected(Readiness::AwaitingReviewResponse)
            .build(),
        ReadinessCase::builder()
            .name("review required")
            .state(State::Open)
            .checks(Checks::Pass)
            .mergeable(Mergeability::Mergeable)
            .review_decision(Review::Required)
            .actionable(false)
            .landed(false)
            .expected(Readiness::AwaitingReviewResponse)
            .build(),
        ReadinessCase::builder()
            .name("approved")
            .state(State::Open)
            .checks(Checks::Pass)
            .mergeable(Mergeability::Mergeable)
            .review_decision(Review::Approved)
            .actionable(false)
            .landed(false)
            .expected(Readiness::ReadyToMerge)
            .build(),
        ReadinessCase::builder()
            .name("no review required")
            .state(State::Open)
            .checks(Checks::Pass)
            .mergeable(Mergeability::Mergeable)
            .review_decision(Review::None)
            .actionable(false)
            .landed(false)
            .expected(Readiness::ReadyToMerge)
            .build(),
        ReadinessCase::builder()
            .name("pending checks")
            .state(State::Open)
            .checks(Checks::Pending)
            .mergeable(Mergeability::Mergeable)
            .review_decision(Review::Approved)
            .actionable(false)
            .landed(false)
            .expected(Readiness::AwaitingReviewResponse)
            .build(),
        ReadinessCase::builder()
            .name("unknown checks")
            .state(State::Open)
            .maybe_checks(None)
            .mergeable(Mergeability::Mergeable)
            .review_decision(Review::Approved)
            .actionable(false)
            .landed(false)
            .expected(Readiness::AwaitingReviewResponse)
            .build(),
        ReadinessCase::builder()
            .name("unknown mergeability")
            .state(State::Open)
            .checks(Checks::Pass)
            .maybe_mergeable(None)
            .review_decision(Review::Approved)
            .actionable(false)
            .landed(false)
            .expected(Readiness::AwaitingReviewResponse)
            .build(),
        ReadinessCase::builder()
            .name("unknown review decision")
            .state(State::Open)
            .checks(Checks::Pass)
            .mergeable(Mergeability::Mergeable)
            .maybe_review_decision(None)
            .actionable(false)
            .landed(false)
            .expected(Readiness::AwaitingReviewResponse)
            .build(),
        ReadinessCase::builder()
            .name("unknown feedback")
            .state(State::Open)
            .checks(Checks::Pass)
            .mergeable(Mergeability::Mergeable)
            .review_decision(Review::Approved)
            .maybe_actionable(None)
            .landed(false)
            .expected(Readiness::AwaitingReviewResponse)
            .build(),
        ReadinessCase::builder()
            .name("unknown state")
            .maybe_state(None)
            .checks(Checks::Pass)
            .mergeable(Mergeability::Mergeable)
            .review_decision(Review::Approved)
            .actionable(false)
            .landed(false)
            .expected(Readiness::AwaitingReviewResponse)
            .build(),
    ];
    for case in cases {
        let status = ChangeRequestStatus {
            title: Observation::default(),
            author: Observation::default(),
            review_decision: Observation { value: case.review_decision, ..Observation::default() },
            review_requested_from_owner: Observation::default(),
            state: Observation { value: case.state, ..Observation::default() },
            head_sha: Observation::default(),
            checks: Observation { value: case.checks, ..Observation::default() },
            review: ChangeRequestReviewObservation { actionable_at_head: Observation { value: case.actionable, ..Observation::default() } },
            mergeable: Observation { value: case.mergeable, ..Observation::default() },
        };
        assert_eq!(change_request_readiness(&status, case.landed), case.expected, "{}", case.name);
    }
}

#[test]
fn change_request_readiness_uses_adr_wire_values() {
    use ChangeRequestReadiness as Readiness;

    for (readiness, expected) in [
        (Readiness::ReadyToMerge, "ready_to_merge"),
        (Readiness::AwaitingReviewResponse, "awaiting_review_response"),
        (Readiness::CiFailing, "ci_failing"),
        (Readiness::Conflicting, "conflicting"),
        (Readiness::Draft, "draft"),
        (Readiness::MergedNotLanded, "merged_not_landed"),
        (Readiness::Closed, "closed"),
    ] {
        assert_eq!(readiness.as_str(), expected);
    }
}

#[test]
fn direct_cleat_recipe_contract_covers_remote_local_fallback_and_withdrawal() {
    let reference = convoy_ref("dev", "remote");
    let endpoint =
        CleatEndpoint { runtime_root: "/var/lib/flotilla/cleat".to_owned(), daemon: "work@3".to_owned(), session: "session-42".to_owned() };
    let mut terminal = vessel().convoy(&reference).name("coder").phase(WorkPhase::Running).materialize("terminal-remote-coder").call();
    terminal.cleat_endpoint = Some(endpoint.clone());
    let convoy = ConvoyRow::builder()
        .resource(reference)
        .name("remote")
        .workflow_ref("implement")
        .phase(ConvoyPhase::Active)
        .message("waiting for a reviewer")
        .vessels(vec![terminal.clone()])
        .build();
    let remote_mint = mint().with_host_routes(HostName::new("kiwi"), BTreeMap::from([("feta".to_owned(), "crew-alias".to_owned())]));
    let remote = project_catalog(&catalog_input(std::slice::from_ref(&convoy)), &remote_mint);
    let vessel_entity = entity::vessel("dev", "remote", "coder", "feta");
    let patch = find_entity(&remote.reassert_patches(), &vessel_entity).clone();
    assert_eq!(text(&patch, KEY_PRIMARY_ACTION_RECIPE), "'flotilla' attach --host 'feta' 'terminal-remote-coder'");
    assert_eq!(text(&patch, KEY_PRIMARY_DIRECT_TRANSPORT), "ssh");
    assert_eq!(text(&patch, KEY_PRIMARY_DIRECT_HOST), "crew-alias");
    assert_eq!(text(&patch, KEY_PRIMARY_DIRECT_RUNTIME_ROOT), endpoint.runtime_root);
    assert_eq!(text(&patch, KEY_PRIMARY_DIRECT_DAEMON), endpoint.daemon);
    assert_eq!(text(&patch, KEY_PRIMARY_DIRECT_SESSION), endpoint.session);
    assert_eq!(
        text(find_entity(&remote.reassert_patches(), &entity::convoy("dev", "remote", "kiwi")), KEY_SUMMARY_TEXT),
        "waiting for a reviewer"
    );

    let local_mint = mint().with_host_routes(HostName::new("feta"), BTreeMap::new());
    let local = project_catalog(&catalog_input(std::slice::from_ref(&convoy)), &local_mint);
    let local_patch = find_entity(&local.reassert_patches(), &vessel_entity).clone();
    assert_eq!(text(&local_patch, KEY_PRIMARY_DIRECT_TRANSPORT), "local");

    let unreachable = project_catalog(&catalog_input(std::slice::from_ref(&convoy)), &mint());
    let fallback = find_entity(&unreachable.reassert_patches(), &vessel_entity).clone();
    assert!(fallback.set.contains_key(KEY_PRIMARY_ACTION_RECIPE));
    assert!(text(&fallback, KEY_PRIMARY_DIRECT_REASON).contains("SSH reachability"));
    assert!(!fallback.set.contains_key(KEY_PRIMARY_DIRECT_DAEMON));

    let mut endpoint_lost = convoy.clone();
    endpoint_lost.vessels[0].cleat_endpoint = None;
    let command_only = project_catalog(&catalog_input(&[endpoint_lost]), &remote_mint);
    let lost = find_entity(&command_only.diff_patches(&remote), &vessel_entity).clone();
    assert!(lost.unset.contains(&KEY_PRIMARY_DIRECT_DAEMON.to_owned()));
    assert!(lost.set.contains_key(KEY_PRIMARY_DIRECT_REASON));
    assert!(!lost.unset.contains(&KEY_PRIMARY_ACTION_RECIPE.to_owned()));

    let mut withdrawn_convoy = convoy;
    withdrawn_convoy.vessels[0].materialize = None;
    withdrawn_convoy.vessels[0].cleat_endpoint = None;
    let withdrawn = project_catalog(&catalog_input(&[withdrawn_convoy]), &remote_mint);
    let diff = withdrawn.diff_patches(&remote);
    let withdrawal = find_entity(&diff, &vessel_entity);
    assert!(withdrawal.unset.contains(&KEY_PRIMARY_DIRECT_DAEMON.to_owned()));
    assert!(withdrawal.unset.contains(&KEY_PRIMARY_ACTION_RECIPE.to_owned()));
}

#[test]
fn raw_catalog_is_entities_only_with_canonical_flat_facts() {
    let reference = convoy_ref("dev", "cutover");
    let convoy = ConvoyRow::builder()
        .resource(reference.clone())
        .name("cutover")
        .workflow_ref("implement")
        .phase(ConvoyPhase::Active)
        .project_ref("project/dev/platform")
        .repo(RepoKey("github.com:flotilla-org/flotilla".to_owned()))
        .subjects(vec![flotilla_protocol::result_set::ConvoySubjectRow {
            subject: flotilla_protocol::Subject {
                kind: flotilla_protocol::SubjectKind::ChangeRequest,
                source: flotilla_protocol::IssueSource { service: "github.com".into(), scope: "flotilla-org/flotilla".into() },
                id: "1044".into(),
            },
            relationship: flotilla_protocol::Relationship::Produces,
            declared: false,
            short: "flotilla!1044".into(),
            url: Some("https://github.com/flotilla-org/flotilla/pull/1044".into()),
            repository_key: Some(RepositoryKey("repo-flotilla".to_owned())),
        }])
        .vessels(vec![vessel().convoy(&reference).name("coder").phase(WorkPhase::Running).materialize("terminal-cutover-coder").call()])
        .build();

    let patches = project_catalog(
        &CatalogInput {
            subjects: None,
            awareness: None,
            convoys: &[convoy],
            independents: &[],
            standing_roles: &[],
            project_repositories: &[],
        },
        &mint(),
    )
    .reassert_patches();

    assert!(
        patches.iter().all(|patch| matches!(patch.target, MetadataTarget::Entity(_))),
        "the connector emits no pre-built group targets"
    );
    let convoy_entity = entity::convoy("dev", "cutover", "kiwi");
    let vessel_entity = entity::vessel("dev", "cutover", "coder", "feta");
    let convoy_patch = find_entity(&patches, &convoy_entity);
    let vessel_patch = find_entity(&patches, &vessel_entity);

    assert_eq!(text(convoy_patch, KEY_ENTITY_KIND), "convoy");
    assert_eq!(text(convoy_patch, KEY_ENTITY_ID), convoy_entity.id);
    assert_eq!(text(convoy_patch, SEGMENT_PROJECT), "dev/platform@kiwi");
    assert_eq!(text(convoy_patch, SEGMENT_REPO), "github.com:flotilla-org/flotilla");
    assert_eq!(text(convoy_patch, KEY_CONVOY), "dev/cutover@kiwi");
    assert!(!convoy_patch.set.contains_key(KEY_CHANGE_REQUEST_NUMBER));
    assert_eq!(text(vessel_patch, KEY_VESSEL), "dev/cutover/coder@feta");
    assert_eq!(
        text(convoy_patch, KEY_PRIMARY_ACTION_TARGET),
        vessel_entity.action_target(),
        "the one-vessel convoy and vessel point at the same live target"
    );
    assert_eq!(text(vessel_patch, KEY_PRIMARY_ACTION_TARGET), vessel_entity.action_target());
    assert_eq!(text(vessel_patch, KEY_PRIMARY_ACTION_RECIPE), "'flotilla' attach --host 'feta' 'terminal-cutover-coder'");
    assert!(patches.iter().all(|patch| text(patch, KEY_SOURCE) == "flotilla"), "every entity carries producer provenance");
}

#[test]
fn long_entity_labels_publish_stable_semantic_tiers() {
    let reference = convoy_ref("dev", "convoy-0123456789abcdef");
    let convoy = ConvoyRow::builder()
        .resource(reference.clone())
        .name("grouping-live-session")
        .workflow_ref("implement")
        .phase(ConvoyPhase::Active)
        .project_ref("project/dev/platform-observability-tools")
        .vessels(vec![vessel().convoy(&reference).name("publish-release-notes").phase(WorkPhase::Running).call()])
        .build();
    let independent = IndependentRow::builder()
        .resource(ResourceRef::new("flotilla/v1", "TerminalSession", "dev", "governor").on_host(HostName::new("feta")))
        .name("governor")
        .host(HostName::new("feta"))
        .phase(SessionPhase::Running)
        .build();

    let catalog = project_catalog(
        &CatalogInput {
            subjects: None,
            awareness: None,
            convoys: &[convoy],
            independents: &[independent],
            standing_roles: &[],
            project_repositories: &[],
        },
        &mint(),
    );
    let first = catalog.reassert_patches();
    let second = catalog.reassert_patches();
    assert_eq!(first, second, "re-assertion must reuse the same label facts");

    let cases = [
        (entity::project("dev", "platform-observability-tools", "kiwi"), "platform-observability-tools", "po-tools", "pot"),
        (entity::convoy("dev", &reference.name, "kiwi"), "grouping-live-session", "gl-session", "gls"),
        (entity::vessel("dev", &reference.name, "publish-release-notes", "feta"), "publish-release-notes", "pr-notes", "prn"),
        (entity::session("feta/dev/governor"), "governor", "governor", "g"),
    ];
    for (entity, full, medium, short) in cases {
        let patch = find_entity(&first, &entity);
        assert_eq!(text(patch, KEY_DISPLAY_LABEL), full);
        assert_eq!(text(patch, KEY_DISPLAY_LABEL_MEDIUM), medium);
        assert_eq!(text(patch, KEY_DISPLAY_LABEL_SHORT), short);
    }
}

#[test]
fn awareness_issues_are_recipe_less_entities_with_source_plus_id_identity() {
    let issue_ref = IssueRef {
        source: IssueSource { service: "https://github.com".to_owned(), scope: "flotilla-org/flotilla".to_owned() },
        id: "982".to_owned(),
    };
    let issue = AwarenessEntry::builder()
        .id("issue/flotilla-org/flotilla/982".to_owned())
        .kind(AwarenessKind::Issue)
        .label("#982 entities-only cutover".to_owned())
        .state(AwarenessState::Waiting)
        .as_of(flotilla_protocol::result_set::Timestamp::UNIX_EPOCH)
        .issue_refs(vec![issue_ref.clone()])
        .build();
    let node = AwarenessNode::builder()
        .id("project/dev/platform".to_owned())
        .kind(AwarenessKind::Project)
        .label("platform".to_owned())
        .scope(flotilla_protocol::QueryScope::new("dev", "platform"))
        .state(AwarenessState::Waiting)
        .as_of(flotilla_protocol::result_set::Timestamp::UNIX_EPOCH)
        .counts(AwarenessCounts::builder().total(1).issues(1).build())
        .entries(vec![issue])
        .build();

    let patches = project_catalog(
        &CatalogInput {
            subjects: None,
            awareness: Some(&[node]),
            convoys: &[],
            independents: &[],
            standing_roles: &[],
            project_repositories: &[],
        },
        &mint(),
    )
    .reassert_patches();
    let issue_patch = find_entity(&patches, &entity::issue(&issue_ref));
    let project_patch = find_entity(&patches, &entity::project("dev", "platform", "fleet"));
    assert_eq!(text(issue_patch, KEY_ENTITY_KIND), "issue");
    assert_eq!(
        project_patch.set[KEY_COUNT_ISSUES].value,
        MetadataValue::Integer(1),
        "counts stay on the project entity, not copied onto the issue"
    );
    assert!(!issue_patch.set.contains_key(KEY_COUNT_ISSUES));
    assert!(!issue_patch.set.contains_key(KEY_PRIMARY_ACTION_RECIPE));
    assert!(!issue_patch.set.contains_key(KEY_DISPLAY_LABEL_MEDIUM));
    assert!(!issue_patch.set.contains_key(KEY_DISPLAY_LABEL_SHORT));
}

#[test]
fn awareness_convoy_labels_ignore_legacy_subject_annotations() {
    let convoy = AwarenessEntry::builder()
        .id("convoy/dev/landing".to_owned())
        .kind(AwarenessKind::Convoy)
        .label("landing · PR #1044".to_owned())
        .state(AwarenessState::Active)
        .as_of(flotilla_protocol::result_set::Timestamp::UNIX_EPOCH)
        .annotations(std::collections::HashMap::from([
            (KEY_CONVOY_NAME.to_owned(), "landing".to_owned()),
            (KEY_CHANGE_REQUEST_NUMBER.to_owned(), "1044".to_owned()),
        ]))
        .build();
    let checkout = AwarenessEntry::builder()
        .id("checkout/kiwi//work/flotilla".to_owned())
        .kind(AwarenessKind::Checkout)
        .label("main · /work/flotilla".to_owned())
        .state(AwarenessState::Active)
        .as_of(flotilla_protocol::result_set::Timestamp::UNIX_EPOCH)
        .annotations(std::collections::HashMap::from([
            (KEY_CHECKOUT_BRANCH.to_owned(), "main".to_owned()),
            (KEY_CHECKOUT_PATH.to_owned(), "/work/flotilla".to_owned()),
        ]))
        .build();
    let node = AwarenessNode::builder()
        .id("project/dev/platform".to_owned())
        .kind(AwarenessKind::Project)
        .label("platform".to_owned())
        .scope(flotilla_protocol::QueryScope::new("dev", "platform"))
        .state(AwarenessState::Active)
        .as_of(flotilla_protocol::result_set::Timestamp::UNIX_EPOCH)
        .counts(AwarenessCounts::builder().total(2).convoys(1).checkouts(1).build())
        .entries(vec![convoy, checkout])
        .build();

    let patches = project_catalog(
        &CatalogInput {
            subjects: None,
            awareness: Some(&[node]),
            convoys: &[],
            independents: &[],
            standing_roles: &[],
            project_repositories: &[],
        },
        &mint(),
    )
    .reassert_patches();
    let convoy = find_entity(&patches, &entity::convoy("dev", "landing", "fleet"));
    assert_eq!(text(convoy, KEY_DISPLAY_LABEL), "landing");
    assert_eq!(text(convoy, KEY_DISPLAY_LABEL_MEDIUM), "landing");
    assert_eq!(text(convoy, KEY_DISPLAY_LABEL_SHORT), "l");
    assert_eq!(text(convoy, KEY_SUMMARY_TEXT), "landing · PR #1044");
    assert_eq!(text(convoy, KEY_CONVOY_NAME), "landing");
    assert!(!convoy.set.contains_key(KEY_CHANGE_REQUEST_NUMBER));

    let checkout = find_entity(&patches, &entity::checkout("checkout/kiwi//work/flotilla"));
    assert_eq!(text(checkout, KEY_DISPLAY_LABEL), "main · /work/flotilla");
    assert_eq!(text(checkout, KEY_SUMMARY_TEXT), "main · /work/flotilla");
    assert_eq!(text(checkout, KEY_CHECKOUT_BRANCH), "main");
    assert_eq!(text(checkout, KEY_CHECKOUT_PATH), "/work/flotilla");

    let project = find_entity(&patches, &entity::project("dev", "platform", "fleet"));
    assert_eq!(text(project, KEY_SUMMARY_TEXT), "2 entries · 0 issues · 0 vessels · 1 checkouts");
    assert_eq!(project.set[KEY_COUNT_TOTAL].value, MetadataValue::Integer(2));
    assert_eq!(project.set[KEY_COUNT_CHECKOUTS].value, MetadataValue::Integer(1));
}

#[test]
fn truncated_awareness_summary_reports_exact_omitted_count() {
    let node = AwarenessNode::builder()
        .id("project/dev/platform".to_owned())
        .kind(AwarenessKind::Project)
        .label("platform".to_owned())
        .scope(flotilla_protocol::QueryScope::new("dev", "platform"))
        .state(AwarenessState::Active)
        .as_of(flotilla_protocol::result_set::Timestamp::UNIX_EPOCH)
        .counts(AwarenessCounts::builder().total(5).convoys(5).build())
        .entries(vec![
            AwarenessEntry::builder()
                .id("convoy/dev/one".to_owned())
                .kind(AwarenessKind::Convoy)
                .label("one".to_owned())
                .state(AwarenessState::Active)
                .as_of(flotilla_protocol::result_set::Timestamp::UNIX_EPOCH)
                .build(),
            AwarenessEntry::builder()
                .id("convoy/dev/two".to_owned())
                .kind(AwarenessKind::Convoy)
                .label("two".to_owned())
                .state(AwarenessState::Done)
                .as_of(flotilla_protocol::result_set::Timestamp::UNIX_EPOCH)
                .build(),
        ])
        .build();

    let patches = project_catalog(
        &CatalogInput {
            subjects: None,
            awareness: Some(&[node]),
            convoys: &[],
            independents: &[],
            standing_roles: &[],
            project_repositories: &[],
        },
        &mint(),
    )
    .reassert_patches();
    let project = find_entity(&patches, &entity::project("dev", "platform", "fleet"));
    assert!(text(project, KEY_SUMMARY_TEXT).ends_with("+3 more"));
}

#[test]
fn same_role_remote_governors_project_as_distinct_catalog_entities() {
    let governor = |resource_name: &str, project: &str| {
        ConvoyRow::builder()
            .resource(convoy_ref("flotilla", resource_name).on_host(HostName::new("udder")))
            .name("governor")
            .workflow_ref("standing-governor")
            .phase(ConvoyPhase::Active)
            .project_ref(project)
            .build()
    };
    let awareness = |resource_name: &str, project: &str| {
        AwarenessNode::builder()
            .id(format!("project/flotilla/{project}"))
            .kind(AwarenessKind::Project)
            .label(project.to_owned())
            .scope(flotilla_protocol::QueryScope::new("flotilla", project))
            .state(AwarenessState::Active)
            .as_of(flotilla_protocol::result_set::Timestamp::UNIX_EPOCH)
            .counts(AwarenessCounts::builder().total(1).convoys(1).build())
            .entries(vec![AwarenessEntry::builder()
                .id(format!("convoy/flotilla/{resource_name}"))
                .kind(AwarenessKind::Convoy)
                .label("governor".to_owned())
                .state(AwarenessState::Active)
                .as_of(flotilla_protocol::result_set::Timestamp::UNIX_EPOCH)
                .annotations(std::collections::HashMap::from([(KEY_CONVOY_NAME.to_owned(), "governor".to_owned())]))
                .build()])
            .build()
    };
    let convoys = [governor("governor-andamento-01234567", "andamento"), governor("governor-wheelhouse-89abcdef", "wheelhouse")];
    let awareness = [awareness("governor-andamento-01234567", "andamento"), awareness("governor-wheelhouse-89abcdef", "wheelhouse")];

    let patches = project_catalog(
        &CatalogInput {
            subjects: None,
            awareness: Some(&awareness),
            convoys: &convoys,
            independents: &[],
            standing_roles: &[],
            project_repositories: &[],
        },
        &mint(),
    )
    .reassert_patches();
    for (resource_name, project) in [("governor-andamento-01234567", "andamento"), ("governor-wheelhouse-89abcdef", "wheelhouse")] {
        let governor = find_entity(&patches, &entity::convoy("flotilla", resource_name, "udder"));
        assert_eq!(text(governor, SEGMENT_PROJECT), format!("flotilla/{project}@fleet"));
        assert_eq!(text(governor, KEY_CONVOY), format!("flotilla/{resource_name}@udder"));
        assert_eq!(text(governor, KEY_CONVOY_NAME), "governor");
    }
}

#[test]
fn standing_checkout_mints_a_transient_terminal_action_but_convoy_checkout_does_not() {
    let checkout = |id: &str, path: &str, links: Vec<AwarenessLink>| {
        AwarenessEntry::builder()
            .id(id.to_owned())
            .kind(AwarenessKind::Checkout)
            .label(format!("main · {path}"))
            .state(AwarenessState::Active)
            .as_of(flotilla_protocol::result_set::Timestamp::UNIX_EPOCH)
            .refs(vec![ResourceRef::new("flotilla.work/v1", "Checkout", "dev", id).on_host(HostName::new("kiwi"))])
            .links(links)
            .annotations(std::collections::HashMap::from([
                (KEY_CHECKOUT_BRANCH.to_owned(), "main".to_owned()),
                (KEY_CHECKOUT_PATH.to_owned(), path.to_owned()),
            ]))
            .build()
    };
    let standing = checkout("standing", "/work/standing", vec![]);
    let convoy_owned =
        checkout("convoy-owned", "/work/convoy", vec![AwarenessLink { rel: "for-convoy".to_owned(), target: "dev/ship-it".to_owned() }]);
    let node = AwarenessNode::builder()
        .id("project/dev/platform".to_owned())
        .kind(AwarenessKind::Project)
        .label("platform".to_owned())
        .scope(flotilla_protocol::QueryScope::new("dev", "platform"))
        .state(AwarenessState::Active)
        .as_of(flotilla_protocol::result_set::Timestamp::UNIX_EPOCH)
        .counts(AwarenessCounts::builder().total(2).checkouts(2).build())
        .entries(vec![standing, convoy_owned])
        .build();

    let patches = project_catalog(
        &CatalogInput {
            subjects: None,
            awareness: Some(&[node]),
            convoys: &[],
            independents: &[],
            standing_roles: &[],
            project_repositories: &[],
        },
        &mint(),
    )
    .reassert_patches();
    let standing = find_entity(&patches, &entity::checkout("standing"));
    assert_eq!(text(standing, KEY_PRIMARY_ACTION_RECIPE), "'flotilla' attach --transient --host 'kiwi' '/work/standing'");
    assert_eq!(text(standing, KEY_PRIMARY_ACTION_TARGET), entity::checkout("standing").action_target());

    let convoy_owned = find_entity(&patches, &entity::checkout("convoy-owned"));
    assert!(!convoy_owned.set.contains_key(KEY_PRIMARY_ACTION_RECIPE));
    assert!(!convoy_owned.set.contains_key(KEY_PRIMARY_ACTION_TARGET));
}

#[test]
fn empty_project_is_an_idle_zero_count_latent_that_opens_its_scoped_view() {
    let node = AwarenessNode::builder()
        .id("project/dev/empty".to_owned())
        .kind(AwarenessKind::Project)
        .label("empty".to_owned())
        .scope(flotilla_protocol::QueryScope::new("dev", "empty"))
        .state(AwarenessState::Idle)
        .as_of(flotilla_protocol::result_set::Timestamp::UNIX_EPOCH)
        .build();

    let patches = project_catalog(
        &CatalogInput {
            subjects: None,
            awareness: Some(&[node]),
            convoys: &[],
            independents: &[],
            standing_roles: &[],
            project_repositories: &[],
        },
        &mint(),
    )
    .reassert_patches();
    let project = find_entity(&patches, &entity::project("dev", "empty", "fleet"));

    assert_eq!(text(project, KEY_STATUS_STATE), "idle");
    assert_eq!(project.set[KEY_COUNT_TOTAL].value, MetadataValue::Integer(0));
    assert_eq!(text(project, KEY_PRIMARY_ACTION_RECIPE), "'flotilla' view 'project/dev/empty'");
}

#[test]
fn awareness_children_use_their_convoys_canonical_origin() {
    let issue_ref = IssueRef {
        source: IssueSource { service: "https://github.com".to_owned(), scope: "flotilla-org/flotilla".to_owned() },
        id: "982".to_owned(),
    };
    let issue = AwarenessEntry::builder()
        .id("issue/flotilla-org/flotilla/982".to_owned())
        .kind(AwarenessKind::Issue)
        .label("#982 entities-only cutover".to_owned())
        .state(AwarenessState::Waiting)
        .as_of(flotilla_protocol::result_set::Timestamp::UNIX_EPOCH)
        .issue_refs(vec![issue_ref.clone()])
        .build();
    let node = AwarenessNode::builder()
        .id("convoy/dev/cutover".to_owned())
        .kind(AwarenessKind::Convoy)
        .label("cutover".to_owned())
        .state(AwarenessState::Waiting)
        .as_of(flotilla_protocol::result_set::Timestamp::UNIX_EPOCH)
        .counts(AwarenessCounts::builder().total(1).issues(1).build())
        .entries(vec![issue])
        .build();
    let reference = convoy_ref("dev", "cutover");
    let convoy = ConvoyRow::builder().resource(reference).name("cutover").workflow_ref("implement").phase(ConvoyPhase::Active).build();

    let patches = project_catalog(
        &CatalogInput {
            subjects: None,
            awareness: Some(&[node]),
            convoys: &[convoy],
            independents: &[],
            standing_roles: &[],
            project_repositories: &[],
        },
        &mint(),
    )
    .reassert_patches();
    let issue_patch = find_entity(&patches, &entity::issue(&issue_ref));

    assert_eq!(text(issue_patch, KEY_CONVOY), "dev/cutover@kiwi");
}

#[test]
fn awareness_repository_group_does_not_masquerade_as_project() {
    let independent = AwarenessEntry::builder()
        .id("independent/dev/governor".to_owned())
        .kind(AwarenessKind::Independent)
        .label("governor".to_owned())
        .state(AwarenessState::Active)
        .as_of(flotilla_protocol::result_set::Timestamp::UNIX_EPOCH)
        .annotations(std::collections::HashMap::from([(SEGMENT_REPO.to_owned(), "flotilla-org/flotilla".to_owned())]))
        .build();
    let node = AwarenessNode::builder()
        .id("repo/opaque-repository-key".to_owned())
        .kind(AwarenessKind::Project)
        .label("github.com/flotilla-org/flotilla".to_owned())
        .state(AwarenessState::Active)
        .as_of(flotilla_protocol::result_set::Timestamp::UNIX_EPOCH)
        .entries(vec![independent])
        .build();

    let patches = project_catalog(
        &CatalogInput {
            subjects: None,
            awareness: Some(&[node]),
            convoys: &[],
            independents: &[],
            standing_roles: &[],
            project_repositories: &[],
        },
        &mint(),
    )
    .reassert_patches();

    find_entity(&patches, &entity::repo("flotilla-org/flotilla"));
    let independent = find_entity(&patches, &entity::session("independent/dev/governor"));
    assert_eq!(text(independent, KEY_DISPLAY_LABEL), "governor");
    assert_eq!(text(independent, KEY_DISPLAY_LABEL_MEDIUM), "governor");
    assert_eq!(text(independent, KEY_DISPLAY_LABEL_SHORT), "g");
    assert!(
        patches.iter().all(|patch| !matches!(&patch.target, MetadataTarget::Entity(entity) if entity.kind == "project")),
        "repository-only awareness must not mint a project entity"
    );
}

#[test]
fn independent_session_uses_the_canonical_session_ref() {
    let row = IndependentRow::builder()
        .resource(ResourceRef::new("flotilla/v1", "TerminalSession", "dev", "scratch").on_host(HostName::new("feta")))
        .name("scratch")
        .host(HostName::new("feta"))
        .attach("scratch")
        .phase(SessionPhase::Running)
        .build();
    let patches = project_catalog(
        &CatalogInput {
            subjects: None,
            awareness: None,
            convoys: &[],
            independents: &[row],
            standing_roles: &[],
            project_repositories: &[],
        },
        &mint(),
    )
    .reassert_patches();
    let session = entity::session("feta/dev/scratch");
    let patch = find_entity(&patches, &session);
    assert_eq!(text(patch, KEY_SESSION), session.id);
    assert_eq!(text(patch, KEY_PRIMARY_ACTION_TARGET), session.action_target());
}

#[test]
fn catalog_diff_unsets_removed_entity_facts() {
    let reference = convoy_ref("dev", "cutover");
    let with_message = ConvoyRow::builder()
        .resource(reference.clone())
        .name("cutover")
        .workflow_ref("implement")
        .phase(ConvoyPhase::Failed)
        .message("boom")
        .build();
    let without_message =
        ConvoyRow::builder().resource(reference).name("cutover").workflow_ref("implement").phase(ConvoyPhase::Active).build();
    let previous = project_catalog(
        &CatalogInput {
            subjects: None,
            awareness: None,
            convoys: &[with_message],
            independents: &[],
            standing_roles: &[],
            project_repositories: &[],
        },
        &mint(),
    );
    let current = project_catalog(
        &CatalogInput {
            subjects: None,
            awareness: None,
            convoys: &[without_message],
            independents: &[],
            standing_roles: &[],
            project_repositories: &[],
        },
        &mint(),
    );
    let diff = current.diff_patches(&previous);
    let patch = find_entity(&diff, &entity::convoy("dev", "cutover", "kiwi"));
    assert!(patch.unset.contains(&KEY_CONVOY_MESSAGE.to_owned()));
}

#[test]
fn badges_preserve_normalized_status_and_attention() {
    assert_eq!(convoy_badge(ConvoyPhase::Pending, false), Badge { state: BadgeState::Waiting, attention: false });
    assert_eq!(convoy_badge(ConvoyPhase::Active, false), Badge { state: BadgeState::Active, attention: false });
    assert_eq!(convoy_badge(ConvoyPhase::Failed, false), Badge { state: BadgeState::Failed, attention: true });
    assert_eq!(work_badge(WorkPhase::Ready), Badge { state: BadgeState::Waiting, attention: true });
    assert_eq!(session_badge(SessionPhase::Running), Badge { state: BadgeState::Active, attention: false });
}

#[test]
fn convoy_summary_surfaces_status_message_ahead_of_progress() {
    let reference = convoy_ref("dev", "waiting");
    let convoy = ConvoyRow::builder()
        .resource(reference.clone())
        .name("waiting")
        .workflow_ref("implement")
        .phase(ConvoyPhase::Pending)
        .message("2 in pool, all leased")
        .vessels(vec![vessel().convoy(&reference).name("coder").phase(WorkPhase::Pending).call()])
        .build();
    let patches = project_catalog(
        &CatalogInput {
            subjects: None,
            awareness: None,
            convoys: &[convoy],
            independents: &[],
            standing_roles: &[],
            project_repositories: &[],
        },
        &mint(),
    )
    .reassert_patches();
    let patch = find_entity(&patches, &entity::convoy("dev", "waiting", "kiwi"));

    assert_eq!(text(patch, KEY_STATUS_STATE), "waiting");
    assert_eq!(text(patch, KEY_SUMMARY_TEXT), "2 in pool, all leased");
}

#[test]
fn crew_roles_remain_a_flat_fact() {
    let reference = convoy_ref("dev", "cutover");
    let mut coder = vessel().convoy(&reference).name("coder").phase(WorkPhase::Running).call();
    coder.crew = vec![CrewMemberSummary {
        role: "coder".to_owned(),
        command_preview: "codex".to_owned(),
        requested_stance: None,
        effective_stance: None,
    }];
    let convoy = ConvoyRow::builder()
        .resource(reference)
        .name("cutover")
        .workflow_ref("implement")
        .phase(ConvoyPhase::Active)
        .vessels(vec![coder])
        .build();
    let patches = project_catalog(
        &CatalogInput {
            subjects: None,
            awareness: None,
            convoys: &[convoy],
            independents: &[],
            standing_roles: &[],
            project_repositories: &[],
        },
        &mint(),
    )
    .reassert_patches();
    let patch = find_entity(&patches, &entity::vessel("dev", "cutover", "coder", "feta"));
    assert_eq!(patch.set[KEY_CREW_ROLES].value, MetadataValue::StringList(vec!["coder".to_owned()]));
}

#[test]
fn awareness_retains_exact_convoy_phase_for_visibility_controls() {
    for phase in [ConvoyPhase::Active, ConvoyPhase::Landed, ConvoyPhase::Cancelled, ConvoyPhase::Abandoned, ConvoyPhase::Failed] {
        let reference = convoy_ref("dev", "governor-old");
        let convoy = ConvoyRow::builder()
            .resource(reference.clone())
            .name("governor".to_owned())
            .workflow_ref("standing-governor")
            .phase(phase)
            .build();
        let entry = AwarenessEntry::builder()
            .id("convoy/dev/governor-old".to_owned())
            .kind(AwarenessKind::Convoy)
            .label("governor".to_owned())
            .state(AwarenessState::Idle)
            .phase(AwarenessPhase::Convoy(phase))
            .as_of(flotilla_protocol::result_set::Timestamp::UNIX_EPOCH)
            .build();
        let vessel = AwarenessEntry::builder()
            .id("vessel/dev/governor-old/govern".to_owned())
            .kind(AwarenessKind::Vessel)
            .label("govern".to_owned())
            .state(AwarenessState::Idle)
            .as_of(flotilla_protocol::result_set::Timestamp::UNIX_EPOCH)
            .build();
        let rows = [convoy];
        for subject in [&entry, &vessel] {
            let (_, facts) = awareness_entry_entity(subject, &rows).expect("published entity facts");
            assert!(facts.contains(&(KEY_CONVOY_PHASE, MetadataValue::text(phase.as_str()))));
        }
        let (_, facts) = awareness_entry_entity(&entry, &[]).expect("published entity facts");
        assert!(facts.contains(&(KEY_CONVOY_PHASE, MetadataValue::text(phase.as_str()))));
    }
}

#[test]
fn only_older_terminal_role_generations_are_superseded() {
    let rows = [
        ("old", "p", "governor", 1, ConvoyPhase::Failed),
        ("latest", "p", "governor", 2, ConvoyPhase::Failed),
        ("live-old", "p", "governor", 1, ConvoyPhase::Active),
        ("other-project", "q", "governor", 1, ConvoyPhase::Failed),
        ("other-role", "p", "worker", 1, ConvoyPhase::Failed),
    ]
    .into_iter()
    .map(|(name, project, role, generation, phase)| {
        ConvoyRow::builder()
            .resource(convoy_ref("dev", name))
            .name(role)
            .project_ref(project)
            .address_role(role)
            .generation(generation)
            .phase(phase)
            .workflow_ref("standing")
            .build()
    })
    .collect::<Vec<_>>();
    let mut catalog = project_catalog(
        &CatalogInput {
            subjects: None,
            awareness: None,
            convoys: &rows,
            independents: &[],
            standing_roles: &[],
            project_repositories: &[],
        },
        &mint(),
    );
    // A detached vessel in Attention must receive the same visibility fact.
    catalog.assert_entity(
        entity::vessel("dev", "old", "govern", "kiwi"),
        vec![(KEY_CONVOY, MetadataValue::text(entity::convoy("dev", "old", "kiwi").id))],
        None,
    );
    mark_superseded_convoys(&mut catalog, &rows);
    for row in &rows {
        let patches = catalog.reassert_patches();
        let facts = find_entity(&patches, &entity::convoy("dev", &row.resource.name, "kiwi"));
        assert_eq!(facts.set["flotilla.convoy.superseded"].value, MetadataValue::Bool(row.resource.name == "old"));
    }
    let patches = catalog.reassert_patches();
    assert_eq!(
        find_entity(&patches, &entity::vessel("dev", "old", "govern", "kiwi")).set["flotilla.convoy.superseded"].value,
        MetadataValue::Bool(true)
    );
}

#[test]
fn raw_role_generations_keep_vessel_identity_and_activation_targets_distinct() {
    let rows = [("convoy-old", 1, ConvoyPhase::Failed), ("convoy-new", 2, ConvoyPhase::Active)]
        .into_iter()
        .map(|(name, generation, phase)| {
            let reference = convoy_ref("dev", name);
            ConvoyRow::builder()
                .resource(reference.clone())
                .name("governor")
                .project_ref("project/dev/p")
                .address_role("governor")
                .generation(generation)
                .phase(phase)
                .workflow_ref("standing")
                .vessels(vec![vessel()
                    .convoy(&reference)
                    .name("govern")
                    .phase(WorkPhase::Running)
                    .materialize(&format!("terminal-{name}"))
                    .call()])
                .build()
        })
        .collect::<Vec<_>>();
    let patches = project_catalog(
        &CatalogInput {
            subjects: None,
            awareness: None,
            convoys: &rows,
            independents: &[],
            standing_roles: &[],
            project_repositories: &[],
        },
        &mint(),
    )
    .reassert_patches();
    for row in &rows {
        let convoy = entity::convoy("dev", &row.resource.name, "kiwi");
        let vessel = entity::vessel("dev", &row.resource.name, "govern", "feta");
        let convoy_patch = find_entity(&patches, &convoy);
        let vessel_patch = find_entity(&patches, &vessel);
        assert_eq!(text(convoy_patch, KEY_DISPLAY_LABEL), "governor");
        assert_eq!(text(vessel_patch, KEY_CONVOY), convoy.id);
        assert_eq!(text(convoy_patch, KEY_PRIMARY_ACTION_TARGET), vessel.action_target());
        assert_eq!(text(vessel_patch, KEY_PRIMARY_ACTION_TARGET), vessel.action_target());
        for patch in [convoy_patch, vessel_patch] {
            assert_eq!(patch.set["flotilla.convoy.superseded"].value, MetadataValue::Bool(row.generation == 1));
        }
    }
}

fn standing_role(project: &str, role: &str) -> StandingRoleRow {
    StandingRoleRow::builder()
        .resource(ResourceRef::new("flotilla.work/v1", "ConvoyEnsure", "dev", format!("ensure-{project}-{role}")))
        .project_ref(project)
        .role(role)
        .build()
}

#[bon::builder]
fn attempt(name: &str, role: &str, generation: u64, phase: ConvoyPhase, ensured_from: Option<&str>, vessels: Option<usize>) -> ConvoyRow {
    let reference = convoy_ref("dev", name);
    let vessels = (0..vessels.unwrap_or(1))
        .map(|index| {
            vessel()
                .convoy(&reference)
                .name(&format!("govern{}", if index == 0 { String::new() } else { index.to_string() }))
                .phase(WorkPhase::Running)
                .materialize(&format!("terminal-{name}-{index}"))
                .call()
        })
        .collect();
    ConvoyRow::builder()
        .resource(reference)
        .name(role)
        .project_ref("p")
        .address_role(role)
        .maybe_ensured_from(ensured_from.map(str::to_owned))
        .generation(generation)
        .phase(phase)
        .workflow_ref("standing")
        .vessels(vessels)
        .build()
}

fn role_catalog(roles: &[StandingRoleRow], convoys: &[ConvoyRow]) -> Catalog {
    project_catalog(
        &CatalogInput { subjects: None, awareness: None, convoys, independents: &[], standing_roles: roles, project_repositories: &[] },
        &mint(),
    )
}

fn role_entity_for(project: &str, role: &str) -> EntityRef {
    entity::role("dev", project, role, "fleet")
}

#[test]
fn live_standing_role_resolves_its_current_vessel_behind_a_stable_intent() {
    let role = standing_role("p", "governor");
    let convoys =
        [attempt().name("convoy-a").role("governor").generation(1).phase(ConvoyPhase::Active).ensured_from("ensure-p-governor").call()];
    let patches = role_catalog(&[role], &convoys).reassert_patches();

    let entity = role_entity_for("p", "governor");
    let facts = find_entity(&patches, &entity);
    let vessel = entity::vessel("dev", "convoy-a", "govern", "feta");
    assert_eq!(text(facts, KEY_ENTITY_KIND), "role");
    assert_eq!(text(facts, SEGMENT_PROJECT), entity::project("dev", "p", "fleet").id);
    assert_eq!(text(facts, KEY_ROLE_NAME), "governor");
    assert_eq!(text(facts, KEY_PRIMARY_ACTION_TARGET), entity.action_target());
    assert_eq!(text(facts, KEY_WORKSPACE_PRIMARY_STATE), "ready");
    assert_eq!(text(facts, KEY_WORKSPACE_PRIMARY_TARGET), vessel.action_target());
    assert!(text(facts, KEY_PRIMARY_ACTION_RECIPE).contains("terminal-convoy-a-0"));
    assert_eq!(text(facts, KEY_STATUS_STATE), "active");
    // The role must not create grouping segments of its backing attempt.
    assert!(!facts.set.contains_key(KEY_CONVOY) && !facts.set.contains_key(KEY_VESSEL));

    for attempt in [entity::convoy("dev", "convoy-a", "kiwi"), vessel] {
        let facts = find_entity(&patches, &attempt);
        assert_eq!(text(facts, KEY_ROLE), entity.id);
        assert_eq!(facts.set[KEY_CONVOY_STANDING].value, MetadataValue::Bool(true));
    }
}

#[test]
fn replacement_generation_changes_the_resolved_target_but_not_the_intent() {
    let role = standing_role("p", "governor");
    let old = attempt().name("convoy-a").role("governor").generation(1).phase(ConvoyPhase::Failed).ensured_from("ensure-p-governor").call();
    let new = attempt().name("convoy-b").role("governor").generation(2).phase(ConvoyPhase::Active).ensured_from("ensure-p-governor").call();
    let live_old =
        attempt().name("convoy-a").role("governor").generation(1).phase(ConvoyPhase::Active).ensured_from("ensure-p-governor").call();
    let before = role_catalog(std::slice::from_ref(&role), &[live_old]);
    let after = role_catalog(std::slice::from_ref(&role), &[old, new]);

    let entity = role_entity_for("p", "governor");
    let patches = after.reassert_patches();
    let facts = find_entity(&patches, &entity);
    assert_eq!(text(facts, KEY_WORKSPACE_PRIMARY_STATE), "ready");
    assert_eq!(text(facts, KEY_WORKSPACE_PRIMARY_TARGET), entity::vessel("dev", "convoy-b", "govern", "feta").action_target());
    assert_eq!(text(facts, KEY_STATUS_STATE), "active", "a superseded failure is not the role's state");

    let diff = after.diff_patches(&before);
    let role_diff = find_entity(&diff, &entity);
    assert!(role_diff.set.contains_key(KEY_WORKSPACE_PRIMARY_TARGET));
    assert!(!role_diff.set.contains_key(KEY_PRIMARY_ACTION_TARGET), "the stable intent is unchanged");
}

#[test]
fn standing_role_between_generations_is_held_without_attention() {
    let role = standing_role("p", "governor");
    let failed =
        attempt().name("convoy-a").role("governor").generation(1).phase(ConvoyPhase::Failed).ensured_from("ensure-p-governor").call();
    let patches = role_catalog(&[role], &[failed]).reassert_patches();
    let facts = find_entity(&patches, &role_entity_for("p", "governor"));
    assert_eq!(text(facts, KEY_WORKSPACE_PRIMARY_STATE), "held");
    assert_eq!(text(facts, KEY_STATUS_STATE), "waiting");
    assert!(!facts.set.contains_key(KEY_STATUS_ATTENTION));
    assert!(!facts.set.contains_key(KEY_WORKSPACE_PRIMARY_TARGET));
    assert!(!facts.set.contains_key(KEY_PRIMARY_ACTION_RECIPE));
}

#[test]
fn held_standing_role_without_an_attempt_remains_visible_and_asks_for_attention() {
    let mut role = standing_role("p", "governor");
    role.hold = Some(StandingRoleHold::RestartLimit);
    role.last_failure = Some("crew exited".to_owned());
    let patches = role_catalog(&[role], &[]).reassert_patches();
    let facts = find_entity(&patches, &role_entity_for("p", "governor"));
    assert_eq!(text(facts, KEY_WORKSPACE_PRIMARY_STATE), "held");
    assert_eq!(text(facts, KEY_ROLE_HOLD), "restart_limit");
    assert_eq!(text(facts, KEY_STATUS_STATE), "failed");
    assert_eq!(facts.set[KEY_STATUS_ATTENTION].value, MetadataValue::Bool(true));
    assert_eq!(text(facts, KEY_SUMMARY_TEXT), "crew exited");
}

#[test]
fn live_attempt_without_a_single_attachable_vessel_stays_held() {
    let role = standing_role("p", "governor");
    let convoys = [attempt()
        .name("convoy-a")
        .role("governor")
        .generation(1)
        .phase(ConvoyPhase::Active)
        .ensured_from("ensure-p-governor")
        .vessels(2)
        .call()];
    let patches = role_catalog(&[role], &convoys).reassert_patches();
    let facts = find_entity(&patches, &role_entity_for("p", "governor"));
    assert_eq!(text(facts, KEY_WORKSPACE_PRIMARY_STATE), "held");
    assert_eq!(text(facts, KEY_STATUS_STATE), "active");
}

#[test]
fn two_standing_roles_on_one_project_are_distinct_entities() {
    let roles = [standing_role("p", "governor"), standing_role("p", "quartermaster")];
    let convoys = [
        attempt().name("convoy-g").role("governor").generation(1).phase(ConvoyPhase::Active).ensured_from("ensure-p-governor").call(),
        attempt()
            .name("convoy-q")
            .role("quartermaster")
            .generation(1)
            .phase(ConvoyPhase::Active)
            .ensured_from("ensure-p-quartermaster")
            .call(),
    ];
    let patches = role_catalog(&roles, &convoys).reassert_patches();
    for (role, convoy) in [("governor", "convoy-g"), ("quartermaster", "convoy-q")] {
        let facts = find_entity(&patches, &role_entity_for("p", role));
        assert_eq!(text(facts, KEY_WORKSPACE_PRIMARY_TARGET), entity::vessel("dev", convoy, "govern", "feta").action_target());
    }
}

#[test]
fn task_convoy_sharing_a_role_name_is_not_standing() {
    let role = standing_role("p", "governor");
    let task = attempt().name("convoy-task").role("governor").generation(1).phase(ConvoyPhase::Active).call();
    let patches = role_catalog(&[role], std::slice::from_ref(&task)).reassert_patches();
    let facts = find_entity(&patches, &entity::convoy("dev", "convoy-task", "kiwi"));
    assert!(!facts.set.contains_key(KEY_ROLE));
    assert!(!facts.set.contains_key(KEY_CONVOY_STANDING));
    let role = find_entity(&patches, &role_entity_for("p", "governor"));
    assert_eq!(text(role, KEY_WORKSPACE_PRIMARY_STATE), "held", "a task convoy never backs the role");
}

#[test]
fn removed_declaration_retracts_the_role_but_keeps_its_attempts_standing() {
    let role = standing_role("p", "governor");
    let convoys =
        [attempt().name("convoy-a").role("governor").generation(1).phase(ConvoyPhase::Active).ensured_from("ensure-p-governor").call()];
    let declared = role_catalog(&[role], &convoys);
    let removed = role_catalog(&[], &convoys);

    let diff = removed.diff_patches(&declared);
    let retracted = find_entity(&diff, &role_entity_for("p", "governor"));
    assert!(retracted.set.keys().all(|key| key == KEY_SOURCE));
    assert!(retracted.unset.iter().any(|key| key == KEY_WORKSPACE_PRIMARY_STATE));

    let patches = removed.reassert_patches();
    let convoy = find_entity(&patches, &entity::convoy("dev", "convoy-a", "kiwi"));
    assert_eq!(convoy.set[KEY_CONVOY_STANDING].value, MetadataValue::Bool(true));
    assert!(!convoy.set.contains_key(KEY_ROLE), "an unknown declaration is not confirmed");
}

fn membership_project(name: &str, members: &[(&str, Option<&str>, Option<&str>)]) -> ProjectRepositoriesRow {
    ProjectRepositoriesRow {
        resource: ResourceRef::new("flotilla.work/v1", "Project", "dev", name),
        display_name: format!("{name} display"),
        repositories: members
            .iter()
            .map(|(key, slug, subpath)| flotilla_protocol::ProjectRepositoryMembership {
                key: RepositoryKey((*key).to_owned()),
                slug: slug.map(str::to_owned),
                subpath: subpath.map(str::to_owned),
            })
            .collect(),
    }
}

#[test]
fn definitions_publish_multiple_shared_and_subpath_memberships_without_work() {
    let projects = [
        membership_project("alpha", &[("repo-shared", Some("github.com:org/shared"), None), ("repo-docs", None, Some("docs"))]),
        membership_project("beta", &[("repo-shared", Some("github.com:org/shared"), Some("src"))]),
        membership_project("empty", &[]),
    ];
    let catalog = project_catalog(
        &CatalogInput {
            subjects: None,
            awareness: None,
            convoys: &[],
            independents: &[],
            standing_roles: &[],
            project_repositories: &projects,
        },
        &mint(),
    );
    let patches = catalog.reassert_patches();
    assert_eq!(
        patches
            .iter()
            .filter(|patch| matches!(&patch.target, MetadataTarget::Entity(entity) if entity.kind == "project_repository"))
            .count(),
        3
    );
    let shared_alpha = entity::project_repository("dev", "alpha", "repo-shared", None);
    let shared_beta = entity::project_repository("dev", "beta", "repo-shared", Some("src"));
    assert_ne!(shared_alpha, shared_beta);
    assert_eq!(text(find_entity(&patches, &shared_alpha), KEY_MEMBERSHIP_REPOSITORY_KEY), "repo-shared");
    assert_eq!(text(find_entity(&patches, &shared_beta), KEY_MEMBERSHIP_SUBPATH), "src");
    assert_eq!(
        text(find_entity(&patches, &entity::project_repository("dev", "alpha", "repo-docs", Some("docs"))), KEY_MEMBERSHIP_SUBPATH),
        "docs"
    );
    assert!(matches!(
        find_entity(&patches, &entity::project("dev", "empty", "fleet")).set[KEY_PROJECT_REPOSITORY_COUNT].value,
        MetadataValue::Integer(0)
    ));
    assert!(patches.iter().all(
        |patch| !matches!(&patch.target, MetadataTarget::Entity(entity) if entity.kind == "repo" && patch.set.contains_key(SEGMENT_PROJECT))
    ));
}

#[test]
fn membership_removal_retracts_relation_while_project_remains() {
    let before = [membership_project("alpha", &[("repo-a", Some("github.com:org/a"), None)])];
    let after = [membership_project("alpha", &[])];
    let workspace = ConvoyRow::builder()
        .resource(convoy_ref("dev", "open-workspace"))
        .name("open-workspace")
        .workflow_ref("implement")
        .phase(ConvoyPhase::Active)
        .project_ref("project/dev/alpha")
        .repo(RepoKey("github.com:org/a".to_owned()))
        .build();
    let first = project_catalog(
        &CatalogInput {
            subjects: None,
            awareness: None,
            convoys: std::slice::from_ref(&workspace),
            independents: &[],
            standing_roles: &[],
            project_repositories: &before,
        },
        &mint(),
    );
    let second = project_catalog(
        &CatalogInput {
            subjects: None,
            awareness: None,
            convoys: std::slice::from_ref(&workspace),
            independents: &[],
            standing_roles: &[],
            project_repositories: &after,
        },
        &mint(),
    );
    let relation = entity::project_repository("dev", "alpha", "repo-a", None);
    let patch = find_entity(&second.diff_patches(&first), &relation).clone();
    assert!(patch.unset.contains(&KEY_MEMBERSHIP_REPOSITORY_KEY.to_owned()));
    assert!(second
        .reassert_patches()
        .iter()
        .any(|patch| patch.target == MetadataTarget::Entity(entity::convoy("dev", "open-workspace", "kiwi"))));
    assert_eq!(text(find_entity(&second.reassert_patches(), &entity::project("dev", "alpha", "fleet")), KEY_PROJECT_NAME), "alpha display");
}

#[test]
fn shared_repository_entity_is_independent_of_convoy_generation_order() {
    let convoys = ["first", "second"].map(|name| {
        ConvoyRow::builder()
            .resource(convoy_ref("dev", name))
            .name(name)
            .workflow_ref("implement")
            .phase(ConvoyPhase::Active)
            .project_ref(if name == "first" { "project/dev/alpha" } else { "project/dev/beta" })
            .repo(RepoKey("github.com:org/shared".to_owned()))
            .build()
    });
    let input = |rows: &[ConvoyRow]| {
        project_catalog(
            &CatalogInput {
                subjects: None,
                awareness: None,
                convoys: rows,
                independents: &[],
                standing_roles: &[],
                project_repositories: &[],
            },
            &mint(),
        )
    };
    let forward = input(&convoys);
    let backward = input(&[convoys[1].clone(), convoys[0].clone()]);
    let repo = entity::repo("github.com:org/shared");
    let forward_patch = find_entity(&forward.reassert_patches(), &repo).clone();
    let backward_patch = find_entity(&backward.reassert_patches(), &repo).clone();
    assert_eq!(forward_patch.set, backward_patch.set);
    assert!(!forward_patch.set.contains_key(SEGMENT_PROJECT));
}

#[test]
fn live_role_carries_its_attempts_vessel_attention() {
    let role = standing_role("p", "governor");
    let mut live =
        attempt().name("convoy-a").role("governor").generation(1).phase(ConvoyPhase::Active).ensured_from("ensure-p-governor").call();
    let quiet = role_catalog(std::slice::from_ref(&role), std::slice::from_ref(&live)).reassert_patches();
    assert!(!find_entity(&quiet, &role_entity_for("p", "governor")).set.contains_key(KEY_STATUS_ATTENTION));

    live.phase = ConvoyPhase::Interrupted;
    let interrupted = role_catalog(std::slice::from_ref(&role), std::slice::from_ref(&live)).reassert_patches();
    let interrupted_role = find_entity(&interrupted, &role_entity_for("p", "governor"));
    let interrupted_convoy = find_entity(&interrupted, &entity::convoy("dev", "convoy-a", "kiwi"));
    assert!(!interrupted_role.set.contains_key(KEY_STATUS_ATTENTION));
    assert!(!interrupted_convoy.set.contains_key(KEY_STATUS_ATTENTION));
    assert_eq!(text(interrupted_role, KEY_STATUS_STATE), "waiting");
    live.phase = ConvoyPhase::Active;

    live.vessels[0].surface_state =
        flotilla_protocol::result_set::SurfaceState::StalledHandled { rung: flotilla_protocol::result_set::HandledRung::Nudge };
    live.surface_state = live.vessels[0].surface_state;
    let handled = role_catalog(std::slice::from_ref(&role), std::slice::from_ref(&live)).reassert_patches();
    let handled_facts = find_entity(&handled, &role_entity_for("p", "governor"));
    assert!(!handled_facts.set.contains_key(KEY_STATUS_ATTENTION));
    assert_eq!(text(handled_facts, KEY_SURFACE_STATE), "stalled_handled");
    assert_eq!(text(handled_facts, KEY_SURFACE_RUNG), "nudge");

    live.vessels[0].surface_state = flotilla_protocol::result_set::SurfaceState::NeedsYou;
    live.surface_state = live.vessels[0].surface_state;
    let waiting = role_catalog(&[role], &[live]).reassert_patches();
    let facts = find_entity(&waiting, &role_entity_for("p", "governor"));
    assert_eq!(facts.set[KEY_STATUS_ATTENTION].value, MetadataValue::Bool(true));
    assert_eq!(text(facts, KEY_STATUS_STATE), "active", "attention does not replace the attempt's state");
}

#[test]
fn replicated_subjects_publish_entities_edges_and_reference_labels() {
    use flotilla_protocol::{Relationship, RepositoryAlias, Subject, SubjectKind};
    use flotilla_resources::{ChangeRequest, ChangeRequestSpec, Forge, ForgeKind, ForgeSpec, InMemoryBackend, InputMeta, ResourceBackend};
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let runtime = tokio::runtime::Builder::new_current_thread().build().expect("runtime");
    let record = runtime.block_on(async {
        backend
            .using::<ChangeRequest>("flotilla")
            .create(
                &InputMeta::builder().name("cr-42".into()).build(),
                &ChangeRequestSpec::builder()
                    .service("github".into())
                    .scope("org/flotilla".into())
                    .number(42)
                    .observing_authority("kiwi".into())
                    .build(),
            )
            .await
            .expect("record")
    });
    let forges = runtime.block_on(async {
        let mut forges = Vec::new();
        // Both claim github.com. The exact Forge ID must win, irrespective of input order.
        for id in ["aaa", "github"] {
            let object = backend
                .using::<Forge>("flotilla")
                .create(
                    &InputMeta::builder().name(id.into()).build(),
                    &ForgeSpec::builder()
                        .forge_id(id.into())
                        .kind(ForgeKind::Github)
                        .hosts(["github.com".into(), "github".into()].into_iter().collect())
                        .https_url("https://github.com".into())
                        .git_ssh_host("github.com".into())
                        .build(),
                )
                .await
                .expect("forge");
            forges.push(object);
        }
        forges
    });
    let source = IssueSource { service: "github".into(), scope: "org/flotilla".into() };
    let subject = Subject { kind: SubjectKind::ChangeRequest, source: source.clone(), id: "42".into() };
    let mut convoy = ConvoyRow::builder()
        .resource(convoy_ref("flotilla", "ship-it"))
        .name("ship-it")
        .workflow_ref("workflow/dev")
        .phase(ConvoyPhase::Active)
        .build();
    convoy.subjects.push(flotilla_protocol::result_set::ConvoySubjectRow {
        subject: subject.clone(),
        relationship: Relationship::Produces,
        declared: false,
        short: "flotilla!42".into(),
        url: None,
        repository_key: None,
    });
    let observations = SubjectCatalogInput {
        change_requests: vec![record],
        forges,
        references: flotilla_protocol::ReferenceContext {
            repositories: vec![RepositoryAlias {
                project: Some("flotilla".into()),
                alias: "f".into(),
                source,
                web_base: "https://github.com".into(),
                forge_alias: None,
            }],
        },
        ..Default::default()
    };
    let convoys = [convoy];
    let mut input = catalog_input(&convoys);
    input.awareness = Some(&[]);
    input.subjects = Some(&observations);
    let patches = project_catalog(&input, &mint()).reassert_patches();
    let cr = EntityRef::new("change_request", "github/org/flotilla!42");
    let patch = find_entity(&patches, &cr);
    assert_eq!(patch.set["flotilla.forge"].value, MetadataValue::EntityRefs(vec![entity::forge("github")]));
    assert!(!patch.set.contains_key("flotilla.change_request.state"));
    assert_eq!(text(patch, KEY_DISPLAY_LABEL), "f!42");
    assert_eq!(text(patch, KEY_DISPLAY_LABEL_SHORT), "f!42");
    assert_eq!(text(patch, "flotilla.change_request.readiness"), "awaiting_review_response");
    let convoy = find_entity(&patches, &entity::convoy("flotilla", "ship-it", "kiwi"));
    assert_eq!(text(convoy, KEY_DISPLAY_LABEL), "ship-it");
    assert_eq!(convoy.set["flotilla.subject.produces"].value, MetadataValue::EntityRefs(vec![cr]));
}

#[test]
fn subject_window_and_successive_role_attempts_exclude_unlinked_records() {
    use flotilla_protocol::{Relationship, Subject, SubjectKind};
    use flotilla_resources::{ChangeRequest, ChangeRequestSpec, InMemoryBackend, InputMeta, ResourceBackend};
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let runtime = tokio::runtime::Builder::new_current_thread().build().expect("runtime");
    let mut record = runtime
        .block_on(
            backend.using::<ChangeRequest>("flotilla").create(
                &InputMeta::builder().name("cr-42".into()).build(),
                &ChangeRequestSpec::builder()
                    .service("github.com".into())
                    .scope("org/flotilla".into())
                    .number(42)
                    .observing_authority("kiwi".into())
                    .build(),
            ),
        )
        .expect("record");
    let now = "2026-10-02T12:00:00Z".parse().expect("time");
    record.status = Some(ChangeRequestStatus {
        title: Observation::known("ship it".into(), now),
        author: Observation::default(),
        state: Observation::known(ObservedChangeRequestState::Open, now),
        checks: Observation::default(),
        mergeable: Observation::default(),
        head_sha: Observation::default(),
        review: ChangeRequestReviewObservation { actionable_at_head: Observation::default() },
        review_decision: Observation::default(),
        review_requested_from_owner: Observation::default(),
    });
    let subject = Subject {
        kind: SubjectKind::ChangeRequest,
        source: IssueSource { service: "github.com".into(), scope: "org/flotilla".into() },
        id: "42".into(),
    };
    let mut old = ConvoyRow::builder()
        .resource(convoy_ref("flotilla", "old"))
        .name("old")
        .workflow_ref("workflow/dev")
        .phase(ConvoyPhase::Landed)
        .finished_at("2026-10-01T12:00:00Z".parse().expect("time"))
        .ensured_from("governor")
        .generation(1)
        .build();
    old.subjects.push(flotilla_protocol::result_set::ConvoySubjectRow {
        subject,
        relationship: Relationship::Produces,
        declared: false,
        short: "flotilla!42".into(),
        url: None,
        repository_key: None,
    });
    let observations = SubjectCatalogInput { change_requests: vec![record.clone()], now: Some(now), ..Default::default() };
    let mut input = catalog_input(&[]);
    input.subjects = Some(&observations);
    let unlinked = project_catalog(&input, &mint()).reassert_patches();
    let cr = EntityRef::new("change_request", "github.com/org/flotilla!42");
    assert!(
        !unlinked.iter().any(|patch| patch.target == MetadataTarget::Entity(cr.clone())),
        "unlinked records are outside this projection"
    );
    let rows = [old.clone()];
    input.convoys = &rows;
    let expired = project_catalog(&input, &mint()).reassert_patches();
    assert!(!expired.iter().any(|patch| patch.target == MetadataTarget::Entity(cr.clone())));
    let recent = SubjectCatalogInput {
        now: Some("2026-10-02T11:59:59Z".parse().expect("within window")),
        change_requests: vec![record.clone()],
        ..Default::default()
    };
    input.subjects = Some(&recent);
    let within_window = project_catalog(&input, &mint());
    assert!(within_window.reassert_patches().iter().any(|patch| patch.target == MetadataTarget::Entity(cr.clone())));
    input.subjects = Some(&observations);
    let withdrawal = project_catalog(&input, &mint()).diff_patches(&within_window);
    assert!(find_entity(&withdrawal, &cr).unset.contains(&"flotilla.subject_of".into()));
    let mut current = old;
    current.resource = convoy_ref("flotilla", "new");
    current.name = "new".into();
    current.phase = ConvoyPhase::Active;
    current.generation = 2;
    let rows = [rows[0].clone(), current];
    let roles = [StandingRoleRow::builder()
        .resource(ResourceRef::new("flotilla.work/v1", "ConvoyEnsure", "flotilla", "governor"))
        .project_ref("flotilla/platform")
        .role("governor")
        .strikes(2)
        .build()];
    input.convoys = &rows;
    input.standing_roles = &roles;
    let patches = project_catalog(&input, &mint()).reassert_patches();
    let role = find_entity(&patches, &entity::role("flotilla", "platform", "governor", "fleet"));
    assert_eq!(role.set["flotilla.role.current_attempt"].value, MetadataValue::EntityRefs(vec![entity::convoy("flotilla", "new", "kiwi")]));
    assert_eq!(role.set["flotilla.subject.produces"].value, MetadataValue::EntityRefs(vec![cr.clone()]));
    assert_eq!(
        find_entity(&patches, &cr).set["flotilla.subject_of"].value,
        MetadataValue::EntityRefs(vec![
            entity::convoy("flotilla", "new", "kiwi"),
            entity::role("flotilla", "platform", "governor", "fleet")
        ])
    );
}

#[test]
fn standing_attempt_edges_and_crew_sessions_survive_re_admission() {
    let mut role = standing_role("p", "governor");
    role.strikes = 2;
    role.next_attempt = Some("2026-10-02T12:00:00Z".parse().expect("backoff time"));
    let live_old = attempt().name("old").role("governor").generation(1).phase(ConvoyPhase::Active).ensured_from("ensure-p-governor").call();
    let before = role_catalog(std::slice::from_ref(&role), std::slice::from_ref(&live_old));
    let patches = before.reassert_patches();
    let facts = find_entity(&patches, &role_entity_for("p", "governor"));
    assert_eq!(facts.set["flotilla.role.current_attempt"].value, MetadataValue::EntityRefs(vec![entity::convoy("dev", "old", "kiwi")]));
    assert_eq!(facts.set["flotilla.role.crew_sessions"].value, MetadataValue::EntityRefs(vec![entity::session("feta/dev/terminal-old-0")]));
    assert_eq!(facts.set["flotilla.role.restart_count"].value, MetadataValue::Integer(2));
    assert_eq!(text(facts, "flotilla.role.desired_state"), "running");
    let mut old = live_old;
    old.phase = ConvoyPhase::Failed;
    let new = attempt().name("new").role("governor").generation(2).phase(ConvoyPhase::Active).ensured_from("ensure-p-governor").call();
    let after = role_catalog(std::slice::from_ref(&role), &[new, old.clone()]);
    let diff = after.diff_patches(&before);
    let facts = find_entity(&diff, &role_entity_for("p", "governor"));
    assert_eq!(facts.set["flotilla.role.current_attempt"].value, MetadataValue::EntityRefs(vec![entity::convoy("dev", "new", "kiwi")]));
    assert_eq!(
        facts.set["flotilla.role.attempts"].value,
        MetadataValue::EntityRefs(vec![entity::convoy("dev", "old", "kiwi"), entity::convoy("dev", "new", "kiwi")])
    );
    let between = role_catalog(&[role], &[old]);
    let diff = between.diff_patches(&after);
    let facts = find_entity(&diff, &role_entity_for("p", "governor"));
    assert!(facts.unset.contains(&"flotilla.role.current_attempt".into()));
    assert!(facts.unset.contains(&"flotilla.role.crew_sessions".into()));
}

#[tokio::test]
async fn replicated_declared_and_discovered_subjects_publish_the_same_multi_repo_graph() {
    use flotilla_protocol::{NodeId, ReferenceContext, Relationship, RepositoryAlias, Subject, SubjectKind};
    use flotilla_resources::{
        convoy_subject_rows, ChangeRequest, ChangeRequestSpec, Convoy, ConvoySpec, ConvoyStatus, DeclaredSubject, Forge, ForgeKind,
        ForgeSpec, InMemoryBackend, InputMeta, Issue, IssueSpec, IssueStatus, ObservedIssueState, Resource, ResourceBackend,
        ResourceObject, SubjectDiscoverySource, WatchEvent,
    };
    async fn create<T: Resource>(backend: &ResourceBackend, name: &str, spec: &T::Spec) -> ResourceObject<T> {
        backend.using::<T>("flotilla").create(&InputMeta::builder().name(name.into()).build(), spec).await.expect("create fixture")
    }
    let author = ResourceBackend::InMemory(InMemoryBackend::default());
    let replica = ResourceBackend::InMemory(InMemoryBackend::default());
    let now = "2026-10-02T12:00:00Z".parse().expect("time");
    let gh = create::<Forge>(
        &author,
        "github",
        &ForgeSpec::builder()
            .forge_id("github".into())
            .kind(ForgeKind::Github)
            .hosts(["github.com".into()].into_iter().collect())
            .https_url("https://github.com".into())
            .git_ssh_host("github.com".into())
            .build(),
    )
    .await;
    let lab = create::<Forge>(
        &author,
        "lab",
        &ForgeSpec::builder()
            .forge_id("lab".into())
            .kind(ForgeKind::Forgejo)
            .hosts(["forge.example".into()].into_iter().collect())
            .https_url("https://forge.example/git/".into())
            .git_ssh_host("forge.example".into())
            .build(),
    )
    .await;
    let first_source = IssueSource { service: "github.com".into(), scope: "org/flotilla".into() };
    let second_source = IssueSource { service: "lab".into(), scope: "team/cleat".into() };
    let first = Subject { kind: SubjectKind::ChangeRequest, source: first_source.clone(), id: "42".into() };
    let second = Subject { kind: SubjectKind::ChangeRequest, source: second_source.clone(), id: "281".into() };
    let issue = Subject { kind: SubjectKind::Issue, source: second_source.clone(), id: "7".into() };
    let references = ReferenceContext {
        repositories: vec![
            RepositoryAlias {
                project: Some("flotilla".into()),
                alias: "c".into(),
                source: first_source,
                web_base: "https://github.com".into(),
                forge_alias: None,
            },
            RepositoryAlias {
                project: Some("wheelhouse".into()),
                alias: "c".into(),
                source: second_source,
                web_base: "https://forge.example/git".into(),
                forge_alias: Some("lab".into()),
            },
        ],
    };
    let mut requests = Vec::new();
    for (name, subject) in [("first", &first), ("second", &second)] {
        let record = create::<ChangeRequest>(
            &author,
            name,
            &ChangeRequestSpec::builder()
                .service(subject.source.service.clone())
                .scope(subject.source.scope.clone())
                .number(subject.id.parse().expect("number"))
                .observing_authority("kiwi".into())
                .build(),
        )
        .await;
        let status = ChangeRequestStatus {
            title: Observation::known(format!("Ship {name}"), now),
            author: Observation::known("alice".into(), now),
            state: Observation::known(ObservedChangeRequestState::Open, now),
            checks: Observation::known(ObservedChecks::Pass, now),
            mergeable: Observation::known(ObservedMergeability::Mergeable, now),
            head_sha: Observation::known("abc123".into(), now),
            review: ChangeRequestReviewObservation { actionable_at_head: Observation::known(false, now) },
            review_decision: Observation::known(ObservedReviewDecision::Approved, now),
            review_requested_from_owner: Observation::known(true, now),
        };
        let record = author
            .using::<ChangeRequest>("flotilla")
            .update_status(name, &record.metadata.resource_version, &status)
            .await
            .expect("observe request");
        replica
            .replica_writer::<ChangeRequest>(NodeId::new("kiwi"), "flotilla")
            .apply(WatchEvent::Added(record.clone()), now)
            .await
            .expect("replicate request");
        requests.push(record);
    }
    let issue_record = create::<Issue>(
        &author,
        "issue",
        &IssueSpec::builder().service("lab".into()).scope("team/cleat".into()).number(7).observing_authority("kiwi".into()).build(),
    )
    .await;
    let issue_record = author
        .using::<Issue>("flotilla")
        .update_status("issue", &issue_record.metadata.resource_version, &IssueStatus {
            title: Observation::known("Make it work".into(), now),
            assignees: Observation::known(vec!["alice".into()], now),
            state: Observation::known(ObservedIssueState::Open, now),
            labels: Observation::known(vec!["ready".into()], now),
            updated_at: Observation::known(now, now),
        })
        .await
        .expect("observe issue");
    replica
        .replica_writer::<Issue>(NodeId::new("kiwi"), "flotilla")
        .apply(WatchEvent::Added(issue_record.clone()), now)
        .await
        .expect("replicate issue");
    for forge in [&gh, &lab] {
        replica
            .replica_writer::<Forge>(NodeId::new("kiwi"), "flotilla")
            .apply(WatchEvent::Added(forge.clone()), now)
            .await
            .expect("replicate forge");
    }
    let convoy = create::<Convoy>(
        &author,
        "ship-it",
        &ConvoySpec::builder()
            .workflow_ref("dev".into())
            .subjects(vec![
                DeclaredSubject { subject: first.clone(), relationship: Relationship::Produces, issue: None, change_request: None },
                DeclaredSubject { subject: issue.clone(), relationship: Relationship::WorksOn, issue: None, change_request: None },
            ])
            .build(),
    )
    .await;
    let mut status = ConvoyStatus { phase: flotilla_resources::ConvoyPhase::Active, ..Default::default() };
    status.discover_subject(first, Relationship::Produces, SubjectDiscoverySource::Claim, now);
    status.discover_subject(second, Relationship::Produces, SubjectDiscoverySource::Branch, now);
    let convoy = author
        .using::<Convoy>("flotilla")
        .update_status("ship-it", &convoy.metadata.resource_version, &status)
        .await
        .expect("discover subjects");
    replica
        .replica_writer::<Convoy>(NodeId::new("kiwi"), "flotilla")
        .apply(WatchEvent::Added(convoy.clone()), now)
        .await
        .expect("replicate convoy");
    let project = |convoy: &ResourceObject<Convoy>, observations: &SubjectCatalogInput| {
        let rows = [ConvoyRow::builder()
            .resource(convoy_ref("flotilla", "ship-it"))
            .name("ship-it")
            .workflow_ref("dev")
            .phase(ConvoyPhase::Active)
            .project_ref("project/dev/platform")
            .subjects(convoy_subject_rows(convoy, &references))
            .build()];
        let mut input = catalog_input(&rows);
        input.subjects = Some(observations);
        project_catalog(&input, &mint())
    };
    let local_observations = SubjectCatalogInput {
        change_requests: requests,
        issues: vec![issue_record],
        forges: vec![gh, lab],
        references: references.clone(),
        now: Some(now),
    };
    let remote_observations = SubjectCatalogInput {
        change_requests: replica
            .including_replicas::<ChangeRequest>("flotilla")
            .list()
            .await
            .expect("replicated requests")
            .items
            .into_iter()
            .map(|item| item.object)
            .collect(),
        issues: replica
            .including_replicas::<Issue>("flotilla")
            .list()
            .await
            .expect("replicated issues")
            .items
            .into_iter()
            .map(|item| item.object)
            .collect(),
        forges: replica
            .including_replicas::<Forge>("flotilla")
            .list()
            .await
            .expect("replicated forges")
            .items
            .into_iter()
            .map(|item| item.object)
            .collect(),
        references: references.clone(),
        now: Some(now),
    };
    let remote_convoy = replica.including_replicas::<Convoy>("flotilla").get("ship-it").await.expect("replicated convoy").object;
    let local = project(&convoy, &local_observations);
    assert_eq!(local, project(&remote_convoy, &remote_observations));
    let patches = local.reassert_patches();
    let convoy = find_entity(&patches, &entity::convoy("flotilla", "ship-it", "kiwi"));
    assert_eq!(
        convoy.set["flotilla.subject.produces"].value,
        MetadataValue::EntityRefs(vec![
            entity::change_request("github.com", "org/flotilla", "42"),
            entity::change_request("lab", "team/cleat", "281")
        ])
    );
    let first = find_entity(&patches, &entity::change_request("github.com", "org/flotilla", "42"));
    // Replicated subjects join the same shared project on every host.
    assert_eq!(text(first, SEGMENT_PROJECT), "dev/platform@fleet");
    assert_eq!(text(first, KEY_DISPLAY_LABEL), "flotilla/c!42");
    assert_eq!(text(first, "flotilla.change_request.title"), "Ship first");
    assert_eq!(text(first, "flotilla.change_request.readiness"), "ready_to_merge");
    assert_eq!(text(first, "flotilla.change_request.checks"), "pass");
    assert_eq!(text(first, "flotilla.change_request.author.observed_at"), "2026-10-02T12:00:00+00:00");
    assert_eq!(first.set["flotilla.forge"].value, MetadataValue::EntityRefs(vec![entity::forge("github")]));
    let issue = find_entity(
        &patches,
        &entity::issue(&IssueRef { source: IssueSource { service: "lab".into(), scope: "team/cleat".into() }, id: "7".into() }),
    );
    assert_eq!(text(issue, SEGMENT_PROJECT), "dev/platform@fleet");
    assert_eq!(text(issue, KEY_DISPLAY_LABEL_SHORT), "wheelhouse/c#7");
    assert_eq!(issue.set["flotilla.issue.labels"].value, MetadataValue::StringList(vec!["ready".into()]));
    assert_eq!(issue.set["flotilla.issue.assignees"].value, MetadataValue::StringList(vec!["alice".into()]));
    assert_eq!(
        issue.set["flotilla.subject_of.works_on"].value,
        MetadataValue::EntityRefs(vec![entity::convoy("flotilla", "ship-it", "kiwi")])
    );
    let forge = find_entity(&patches, &entity::forge("lab"));
    assert_eq!(text(forge, "flotilla.forge.web_url"), "https://forge.example/git");
    assert_eq!(text(forge, "flotilla.forge.change_request_url_template"), "{web_url}/{scope}/pulls/{number}");
}

// Behaviour (#2454): both subject kinds publish the unique linking project,
// independently of awareness and host; zero or multiple projects publish none.
#[hegel::test]
fn subject_project_is_unambiguous(tc: hegel::TestCase) {
    use flotilla_protocol::{result_set::ConvoySubjectRow, Relationship, Subject, SubjectKind};
    use flotilla_resources::{ChangeRequest, ChangeRequestSpec, InMemoryBackend, InputMeta, Issue, IssueSpec, ResourceBackend};
    use hegel::generators as gs;

    // Empty through repeated and conflicting projects, missing project refs,
    // all relationship kinds, remote/local origins, and role disagreement.
    // Convoy project 3 means no reference; roles always name a project in 0..=2.
    let count = tc.draw(gs::integers::<usize>().min_value(0).max_value(6));
    let with_role = tc.draw(gs::booleans());
    let source = IssueSource { service: "github".into(), scope: "org/repo".into() };
    let subjects = [Subject { kind: SubjectKind::ChangeRequest, source: source.clone(), id: "42".into() }, Subject {
        kind: SubjectKind::Issue,
        source: source.clone(),
        id: "42".into(),
    }];
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let runtime = tokio::runtime::Builder::new_current_thread().build().expect("runtime");
    let observations = runtime.block_on(async {
        SubjectCatalogInput {
            change_requests: vec![backend
                .using::<ChangeRequest>("dev")
                .create(
                    &InputMeta::builder().name("cr".into()).build(),
                    &ChangeRequestSpec::builder()
                        .service("github".into())
                        .scope("org/repo".into())
                        .number(42)
                        .observing_authority("kiwi".into())
                        .build(),
                )
                .await
                .expect("request")],
            issues: vec![backend
                .using::<Issue>("dev")
                .create(
                    &InputMeta::builder().name("issue".into()).build(),
                    &IssueSpec::builder()
                        .service("github".into())
                        .scope("org/repo".into())
                        .number(42)
                        .observing_authority("kiwi".into())
                        .build(),
                )
                .await
                .expect("issue")],
            ..Default::default()
        }
    });
    let mut expected = std::collections::BTreeSet::new();
    let mut convoys = Vec::new();
    for index in 0..count {
        let project = tc.draw(gs::integers::<usize>().min_value(0).max_value(3));
        let relationship =
            [Relationship::Produces, Relationship::Adopts, Relationship::WorksOn, Relationship::Supersedes, Relationship::References]
                [tc.draw(gs::integers::<usize>().min_value(0).max_value(4))];
        let mut resource = convoy_ref("dev", &format!("convoy-{index}"));
        if tc.draw(gs::booleans()) {
            resource.host = None;
        }
        let mut convoy =
            ConvoyRow::builder().resource(resource).name(format!("convoy-{index}")).workflow_ref("dev").phase(ConvoyPhase::Active).build();
        if project < 3 {
            convoy.project_ref = Some(format!("project/dev/p{project}"));
            expected.insert(format!("dev/p{project}@fleet"));
        }
        convoy.subjects = subjects
            .iter()
            .map(|subject| ConvoySubjectRow {
                subject: subject.clone(),
                relationship,
                declared: false,
                short: "42".into(),
                url: None,
                repository_key: None,
            })
            .collect();
        if with_role && index == 0 {
            convoy.ensured_from = Some("ensure".into());
        }
        // Every generated linker has a twin on the other host origin. Their
        // identical project must deduplicate rather than become ambiguity.
        let mut twin = convoy.clone();
        twin.resource.name = format!("peer-{index}");
        twin.name = format!("peer-{index}");
        twin.resource.host = if convoy.resource.host.is_some() { None } else { Some(HostName::new("kiwi")) };
        twin.ensured_from = None;
        convoys.extend([convoy, twin]);
    }
    let roles = if with_role && count > 0 {
        let project = tc.draw(gs::integers::<usize>().min_value(0).max_value(2));
        expected.insert(format!("dev/p{project}@fleet"));
        vec![StandingRoleRow::builder()
            .resource(ResourceRef::new("flotilla.work/v1", "ConvoyEnsure", "dev", "ensure"))
            .project_ref(format!("project/dev/p{project}"))
            .role("governor")
            .strikes(0)
            .build()]
    } else {
        vec![]
    };
    let awareness = [AwarenessNode::builder()
        .id("project/dev/p0".to_owned())
        .kind(AwarenessKind::Project)
        .label("p0".to_owned())
        .state(AwarenessState::Waiting)
        .as_of(Timestamp::UNIX_EPOCH)
        .counts(AwarenessCounts::builder().total(1).issues(1).build())
        .entries(vec![AwarenessEntry::builder()
            .id("issue/org/repo/42".to_owned())
            .kind(AwarenessKind::Issue)
            .label("issue".to_owned())
            .state(AwarenessState::Waiting)
            .as_of(Timestamp::UNIX_EPOCH)
            .issue_refs(vec![IssueRef { source, id: "42".into() }])
            .build()])
        .build()];
    for nodes in [None, Some(&[][..]), Some(&awareness[..])] {
        let mut input = catalog_input(&convoys);
        input.subjects = Some(&observations);
        input.standing_roles = &roles;
        input.awareness = nodes;
        let patches = project_catalog(&input, &mint()).reassert_patches();
        // Unlinked records are outside subject projection; awareness may
        // independently publish the issue, but must not publish a request.
        if count == 0 {
            assert!(!patches
                .iter()
                .any(|patch| patch.target == MetadataTarget::Entity(entity::change_request("github", "org/repo", "42"))));
            continue;
        }
        for subject in &subjects {
            let target = match subject.kind {
                SubjectKind::ChangeRequest => entity::change_request("github", "org/repo", "42"),
                SubjectKind::Issue => entity::issue(&IssueRef { source: subject.source.clone(), id: "42".into() }),
            };
            let actual = find_entity(&patches, &target).set.get(SEGMENT_PROJECT).map(|value| &value.value);
            let wanted = (expected.len() == 1).then(|| MetadataValue::text(expected.first().expect("unique project")));
            assert_eq!(actual, wanted.as_ref());
        }
    }
}

// Behaviour (#2471): duplicate subject names select one complete observation,
// independently of arrival order, and preserve the union of convoy history.
#[hegel::test]
fn duplicate_request_subjects_have_one_observation(tc: hegel::TestCase) {
    use flotilla_protocol::Relationship;
    use flotilla_resources::{
        select_change_requests, ChangeRequest, ChangeRequestSpec, ChangeRequestSubjectHistory, InMemoryBackend, InputMeta, ResourceBackend,
    };
    use hegel::generators as gs;

    // Cover empty/single/many records, equal and unequal timestamps, multiple
    // authorities, known/unknown fields, unrelated subject numbers, and rotations.
    let count = tc.draw(gs::integers::<usize>().min_value(0).max_value(6));
    let offset = tc.draw(gs::integers::<usize>().min_value(0).max_value(6));
    let now: Timestamp = "2026-10-02T12:00:00Z".parse().expect("time");
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let runtime = tokio::runtime::Builder::new_current_thread().build().expect("runtime");
    let mut records = Vec::new();
    let mut expected = BTreeMap::new();
    for index in 0..count {
        let stamp = tc.draw(gs::integers::<i64>().min_value(0).max_value(2));
        let authority = tc.draw(gs::integers::<usize>().min_value(0).max_value(2)).to_string();
        let number = tc.draw(gs::integers::<u64>().min_value(42).max_value(43));
        let known = tc.draw(gs::booleans());
        let canonical = index == 0 && tc.draw(gs::booleans());
        let name = if canonical {
            flotilla_resources::change_request_record_name("github", "org/repo", number)
        } else {
            format!("duplicate-{index}")
        };
        let mut record = runtime
            .block_on(
                backend.using::<ChangeRequest>("dev").create(
                    &InputMeta::builder().name(name.clone()).build(),
                    &ChangeRequestSpec::builder()
                        .service("github".into())
                        .scope("org/repo".into())
                        .number(number)
                        .observing_authority(authority.clone())
                        .subject_of(vec![ChangeRequestSubjectHistory::builder()
                            .namespace("dev".into())
                            .convoy(format!("convoy-{index}"))
                            .origin("kiwi".into())
                            .relationship(Relationship::Produces)
                            .role("coder".into())
                            .last_seen(now)
                            .build()])
                        .build(),
                ),
            )
            .expect("record");
        let at = now + std::time::Duration::from_secs(stamp as u64);
        record.status = Some(ChangeRequestStatus {
            title: if known { Observation::known(name.clone(), at) } else { Observation::unknown(at) },
            author: Observation::default(),
            state: Observation::known(ObservedChangeRequestState::Open, at),
            checks: Observation::unknown(at),
            mergeable: Observation::unknown(at),
            head_sha: Observation::unknown(at),
            review: ChangeRequestReviewObservation { actionable_at_head: Observation::unknown(at) },
            review_decision: Observation::unknown(at),
            review_requested_from_owner: Observation::unknown(at),
        });
        record.spec.subject_of.push(
            ChangeRequestSubjectHistory::builder()
                .namespace("dev".into())
                .convoy("shared-producer".into())
                .origin("kiwi".into())
                .relationship(Relationship::Produces)
                .role("coder".into())
                .last_seen(at)
                .build(),
        );
        let rank = (stamp, authority, canonical, name);
        let winner = expected.entry(number).or_insert((rank.clone(), record.status.clone()));
        if rank > winner.0 {
            *winner = (rank, record.status.clone());
        }
        records.push(record);
    }
    let selected = select_change_requests(&records);
    // The returned source tag is the exact winner, even when observations tie.
    for (_, (object, index)) in
        flotilla_resources::select_change_request_sources(records.iter().enumerate().map(|(index, object)| (object, index)))
    {
        assert_eq!(object.metadata.name, records[index].metadata.name);
        assert_eq!(object.status, records[index].status);
    }
    for (subject, record) in &selected {
        let number = subject.id.parse::<u64>().expect("number");
        assert_eq!(record.status, expected[&number].1);
        assert_eq!(record.spec.subject_of.len(), records.iter().filter(|record| record.spec.number == number).count() + 1);
        let latest = records
            .iter()
            .filter(|record| record.spec.number == number)
            .flat_map(|record| &record.spec.subject_of)
            .filter(|entry| entry.convoy == "shared-producer")
            .map(|entry| entry.last_seen)
            .max()
            .expect("producer history");
        assert_eq!(
            record.spec.subject_of.iter().find(|entry| entry.convoy == "shared-producer").expect("merged history").last_seen,
            latest
        );
    }
    let convoys = [42, 43].map(|number| {
        let mut row =
            ConvoyRow::builder().resource(convoy_ref("dev", "live")).name("live").workflow_ref("dev").phase(ConvoyPhase::Active).build();
        row.subjects.push(flotilla_protocol::result_set::ConvoySubjectRow {
            subject: flotilla_protocol::Subject {
                kind: flotilla_protocol::SubjectKind::ChangeRequest,
                source: IssueSource { service: "github".into(), scope: "org/repo".into() },
                id: number.to_string(),
            },
            relationship: Relationship::Produces,
            declared: false,
            short: number.to_string(),
            url: None,
            repository_key: None,
        });
        row
    });
    let observations = SubjectCatalogInput { change_requests: records.clone(), ..Default::default() };
    let mut input = catalog_input(&convoys);
    input.subjects = Some(&observations);
    let patches = project_catalog(&input, &mint()).reassert_patches();
    for (number, (_, status)) in &expected {
        let patch = find_entity(&patches, &entity::change_request("github", "org/repo", &number.to_string()));
        let expected_title = status.as_ref().expect("status").title.value.as_deref();
        assert_eq!(
            patch.set.get("flotilla.change_request.title").and_then(|value| match &value.value {
                MetadataValue::Text(text) => Some(text.as_str()),
                _ => None,
            }),
            expected_title
        );
    }
    records.reverse();
    if count > 0 {
        records.rotate_left(offset % count);
    }
    assert_eq!(
        serde_json::to_value(select_change_requests(&records).into_values().collect::<Vec<_>>()).expect("json"),
        serde_json::to_value(selected.into_values().collect::<Vec<_>>()).expect("json")
    );
}

// Behaviour (ADR 0051): an open produced request outlives its deleted convoy,
// preserves reverse links, and vanishes when merged/closed. Unknown is retained
// conservatively by storage but is not presented as an observed open orphan.
#[test]
fn orphaned_request_keeps_departed_convoy_edge_until_terminal() {
    use flotilla_protocol::Relationship;
    use flotilla_resources::{ChangeRequest, ChangeRequestSpec, ChangeRequestSubjectHistory, InMemoryBackend, InputMeta, ResourceBackend};
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let runtime = tokio::runtime::Builder::new_current_thread().build().expect("runtime");
    let now = "2026-10-02T12:00:00Z".parse().expect("time");
    let mut record = runtime
        .block_on(
            backend.using::<ChangeRequest>("dev").create(
                &InputMeta::builder().name("noncanonical".into()).build(),
                &ChangeRequestSpec::builder()
                    .service("github".into())
                    .scope("org/repo".into())
                    .number(42)
                    .observing_authority("node".into())
                    .subject_of(vec![ChangeRequestSubjectHistory::builder()
                        .namespace("dev".into())
                        .convoy("departed".into())
                        .origin("kiwi".into())
                        .relationship(Relationship::Produces)
                        .role("coder".into())
                        .last_seen(now)
                        .build()])
                    .build(),
            ),
        )
        .expect("record");
    let cr = entity::change_request("github", "org/repo", "42");
    for state in [
        ObservedChangeRequestState::Open,
        ObservedChangeRequestState::Draft,
        ObservedChangeRequestState::Merged,
        ObservedChangeRequestState::Closed,
    ] {
        record.status = Some(ChangeRequestStatus {
            title: Observation::default(),
            author: Observation::default(),
            state: Observation::known(state, now),
            checks: Observation::default(),
            mergeable: Observation::default(),
            head_sha: Observation::default(),
            review: ChangeRequestReviewObservation { actionable_at_head: Observation::default() },
            review_decision: Observation::default(),
            review_requested_from_owner: Observation::default(),
        });
        let observations = SubjectCatalogInput { change_requests: vec![record.clone()], now: Some(now), ..Default::default() };
        let mut input = catalog_input(&[]);
        input.subjects = Some(&observations);
        let catalog = project_catalog(&input, &mint());
        let patches = catalog.reassert_patches();
        if matches!(state, ObservedChangeRequestState::Open | ObservedChangeRequestState::Draft) {
            let patch = find_entity(&patches, &cr);
            assert_eq!(patch.set["flotilla.orphaned"].value, MetadataValue::Bool(true));
            assert_eq!(
                patch.set["flotilla.subject_of.produces"].value,
                MetadataValue::EntityRefs(vec![entity::convoy("dev", "departed", "kiwi")])
            );
        } else {
            assert!(!patches.iter().any(|patch| patch.target == MetadataTarget::Entity(cr.clone())));
        }
    }
}

// Behaviour (#2508, ADR 0051): both subject kinds resolve github.com to a
// presentation-only default, while every declared covering Forge wins.
#[hegel::test]
fn builtin_github_forge_resolves_subjects(tc: hegel::TestCase) {
    use flotilla_protocol::{result_set::ConvoySubjectRow, Relationship, Subject, SubjectKind};
    use flotilla_resources::{ChangeRequestSpec, ForgeKind, ForgeSpec, InMemoryBackend, InputMeta, IssueSpec, ResourceBackend};
    use hegel::generators as gs;

    // Cover absent, unrelated, installation-URL and host-alias
    // declarations, empty through duplicate links, and number boundaries.
    let awareness = tc.draw(gs::booleans());
    let count = tc.draw(gs::integers::<usize>().min_value(0).max_value(3));
    let number = tc.draw(gs::integers::<u32>().min_value(1).max_value(u32::MAX));
    // Forge resource IDs are DNS labels, so github.com cannot be an exact ID.
    // Exercise every valid covering mode on every generated input.
    for mode in 0..=3 {
        let runtime = tokio::runtime::Builder::new_current_thread().build().expect("runtime");
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let mut observations = runtime.block_on(async {
            SubjectCatalogInput {
                change_requests: vec![backend
                    .using::<ChangeRequest>("dev")
                    .create(
                        &InputMeta::builder().name("cr".into()).build(),
                        &ChangeRequestSpec::builder()
                            .service("github.com".into())
                            .scope("org/repo".into())
                            .number(number.into())
                            .observing_authority("kiwi".into())
                            .build(),
                    )
                    .await
                    .expect("change request")],
                issues: vec![backend
                    .using::<Issue>("dev")
                    .create(
                        &InputMeta::builder().name("issue".into()).build(),
                        &IssueSpec::builder()
                            .service("github.com".into())
                            .scope("org/repo".into())
                            .number(number.into())
                            .observing_authority("kiwi".into())
                            .build(),
                    )
                    .await
                    .expect("issue")],
                ..Default::default()
            }
        });
        let source = IssueSource { service: "github.com".into(), scope: "org/repo".into() };
        let subjects = [Subject { kind: SubjectKind::ChangeRequest, source: source.clone(), id: number.to_string() }, Subject {
            kind: SubjectKind::Issue,
            source,
            id: number.to_string(),
        }];
        let mut convoy =
            ConvoyRow::builder().resource(convoy_ref("dev", "work")).name("work").workflow_ref("dev").phase(ConvoyPhase::Active).build();
        convoy.subjects = (0..count)
            .flat_map(|_| {
                subjects.iter().map(|subject| ConvoySubjectRow {
                    subject: subject.clone(),
                    relationship: Relationship::Produces,
                    declared: false,
                    short: subject.id.clone(),
                    url: None,
                    repository_key: None,
                })
            })
            .collect();
        let convoys = [convoy];
        let build = |observations: &SubjectCatalogInput| {
            let mut input = catalog_input(&convoys);
            input.subjects = Some(observations);
            if awareness {
                input.awareness = Some(&[]);
            }
            project_catalog(&input, &mint())
        };
        let implicit = build(&observations);
        if mode > 0 {
            let (id, url, host) = match mode {
                1 => ("lab", "https://lab.example", "lab.example"),
                2 => ("declared", "https://github.com/", "declared.example"),
                _ => ("declared", "https://declared.example", "github.com"),
            };
            observations.forges.push(runtime.block_on(async {
                backend
                    .using::<Forge>("dev")
                    .create(
                        &InputMeta::builder().name(id.into()).build(),
                        &ForgeSpec::builder()
                            .forge_id(id.into())
                            .kind(if mode == 3 { ForgeKind::Forgejo } else { ForgeKind::Github })
                            .hosts([host.into()].into_iter().collect())
                            .https_url(url.into())
                            .git_ssh_host(host.into())
                            .build(),
                    )
                    .await
                    .expect("forge")
            }));
        }
        let catalog = build(&observations);
        let patches = catalog.reassert_patches();
        let expected_id = if mode < 2 { "github.com" } else { "declared" };
        let forge = entity::forge(expected_id);
        let facts = find_entity(&patches, &forge);
        assert_eq!(text(facts, "flotilla.forge.kind"), if mode == 3 { "forgejo" } else { "github" });
        assert_eq!(text(facts, "flotilla.forge.web_url"), if mode == 3 { "https://declared.example" } else { "https://github.com" });
        assert_eq!(
            text(facts, "flotilla.forge.change_request_url_template"),
            if mode == 3 { "{web_url}/{scope}/pulls/{number}" } else { "{web_url}/{scope}/pull/{number}" }
        );
        assert_eq!(text(facts, "flotilla.forge.issue_url_template"), "{web_url}/{scope}/issues/{number}");
        for subject in &subjects {
            let target = match subject.kind {
                SubjectKind::ChangeRequest => entity::change_request("github.com", "org/repo", &subject.id),
                SubjectKind::Issue => entity::issue(&IssueRef { source: subject.source.clone(), id: subject.id.clone() }),
            };
            if count == 0 {
                assert!(!patches.iter().any(|patch| patch.target == MetadataTarget::Entity(target.clone())));
                continue;
            }
            let patch = find_entity(&patches, &target);
            assert_eq!(text(patch, "flotilla.subject.service"), "github.com");
            assert_eq!(text(patch, "flotilla.subject.scope"), "org/repo");
            assert_eq!(text(patch, "flotilla.subject.number"), subject.id);
            // The serialized PM connector payload is the consumer's contract.
            let wire = serde_json::to_value(patch).expect("wire patch");
            assert_eq!(
                wire["set"]["flotilla.forge"]["value"],
                serde_json::json!({
                    "type": "entity-refs", "value": [{"kind": "forge", "id": expected_id}]
                })
            );
        }
        if mode >= 2 {
            assert!(!patches.iter().any(|patch| patch.target == MetadataTarget::Entity(entity::forge("github.com"))));
            let diff = catalog.diff_patches(&implicit);
            assert!(find_entity(&diff, &entity::forge("github.com")).unset.contains(&"flotilla.forge.kind".into()));
        }
        // Projection never creates a Forge resource for the fallback.
        let stored = runtime.block_on(backend.using::<Forge>("dev").list()).expect("stored forges");
        assert_eq!(stored.items.len(), usize::from(mode > 0));
    }
}

// Behaviour (ADR 0051): non-built-in uncovered services produce a structured
// warning once per service, keep subjects visible, and omit unresolved edges.
#[test]
fn uncovered_subject_service_warns_with_service() {
    use std::sync::{Arc, Mutex};

    use flotilla_protocol::{result_set::ConvoySubjectRow, Relationship, Subject, SubjectKind};
    use flotilla_resources::{ChangeRequestSpec, InMemoryBackend, InputMeta, IssueSpec, ResourceBackend};
    use tracing::{
        field::{Field, Visit},
        span::{Attributes, Id, Record},
        Event, Metadata, Subscriber,
    };

    #[derive(Clone, Default)]
    struct Warnings(Arc<Mutex<Vec<BTreeMap<String, String>>>>);
    struct Fields(BTreeMap<String, String>);
    impl Visit for Fields {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            self.0.insert(field.name().into(), format!("{value:?}"));
        }
        fn record_str(&mut self, field: &Field, value: &str) {
            self.0.insert(field.name().into(), value.into());
        }
    }
    // Captures the diagnostic output boundary; projection and storage are real.
    impl Subscriber for Warnings {
        fn enabled(&self, metadata: &Metadata<'_>) -> bool {
            *metadata.level() == tracing::Level::WARN
        }
        fn new_span(&self, _: &Attributes<'_>) -> Id {
            Id::from_u64(1)
        }
        fn record(&self, _: &Id, _: &Record<'_>) {}
        fn record_follows_from(&self, _: &Id, _: &Id) {}
        fn event(&self, event: &Event<'_>) {
            if *event.metadata().level() == tracing::Level::WARN {
                let mut fields = Fields(BTreeMap::new());
                event.record(&mut fields);
                self.0.lock().expect("warnings").push(fields.0);
            }
        }
        fn enter(&self, _: &Id) {}
        fn exit(&self, _: &Id) {}
    }
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let runtime = tokio::runtime::Builder::new_current_thread().build().expect("runtime");
    let observations = runtime.block_on(async {
        SubjectCatalogInput {
            change_requests: vec![backend
                .using::<ChangeRequest>("dev")
                .create(
                    &InputMeta::builder().name("cr".into()).build(),
                    &ChangeRequestSpec::builder()
                        .service("forge.example".into())
                        .scope("org/repo".into())
                        .number(42)
                        .observing_authority("kiwi".into())
                        .build(),
                )
                .await
                .expect("change request")],
            issues: vec![backend
                .using::<Issue>("dev")
                .create(
                    &InputMeta::builder().name("issue".into()).build(),
                    &IssueSpec::builder()
                        .service("forge.example".into())
                        .scope("org/repo".into())
                        .number(42)
                        .observing_authority("kiwi".into())
                        .build(),
                )
                .await
                .expect("issue")],
            ..Default::default()
        }
    });
    let source = IssueSource { service: "forge.example".into(), scope: "org/repo".into() };
    let mut convoy =
        ConvoyRow::builder().resource(convoy_ref("dev", "work")).name("work").workflow_ref("dev").phase(ConvoyPhase::Active).build();
    convoy.subjects = [SubjectKind::ChangeRequest, SubjectKind::Issue]
        .into_iter()
        .map(|kind| ConvoySubjectRow {
            subject: Subject { kind, source: source.clone(), id: "42".into() },
            relationship: Relationship::Produces,
            declared: false,
            short: "42".into(),
            url: None,
            repository_key: None,
        })
        .collect();
    let convoys = [convoy];
    let mut input = catalog_input(&convoys);
    input.subjects = Some(&observations);
    let warnings = Warnings::default();
    let patches = tracing::subscriber::with_default(warnings.clone(), || project_catalog(&input, &mint()).reassert_patches());
    let warnings = warnings.0.lock().expect("warnings");
    assert_eq!(warnings.len(), 1);
    assert_eq!(warnings[0]["service"], "forge.example");
    for target in [entity::change_request("forge.example", "org/repo", "42"), entity::issue(&IssueRef { source, id: "42".into() })] {
        assert!(!find_entity(&patches, &target).set.contains_key("flotilla.forge"));
    }
}
