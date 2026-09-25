use flotilla_protocol::{
    result_set::{AwarenessCounts, AwarenessEntry, AwarenessKind, AwarenessLink, AwarenessNode, AwarenessState, CrewMemberSummary},
    ChangeRequestStatus, ConvoyChangeRequest, HostName, IssueRef, IssueSource, RepoKey, RepositoryKey, ResourceRef,
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
        .change_request(ConvoyChangeRequest {
            id: "1044".to_owned(),
            status: ChangeRequestStatus::Open,
            repository_key: RepositoryKey("repo-flotilla".to_owned()),
        })
        .vessels(vec![vessel().convoy(&reference).name("coder").phase(WorkPhase::Running).materialize("terminal-cutover-coder").call()])
        .build();

    let patches = project_catalog(
        &CatalogInput { awareness: None, convoys: &[convoy], independents: &[], standing_roles: &[], project_repositories: &[] },
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
    assert_eq!(text(convoy_patch, KEY_CHANGE_REQUEST_NUMBER), "1044");
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
        &CatalogInput { awareness: None, convoys: &[convoy], independents: &[independent], standing_roles: &[], project_repositories: &[] },
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
        &CatalogInput { awareness: Some(&[node]), convoys: &[], independents: &[], standing_roles: &[], project_repositories: &[] },
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
fn awareness_composed_text_is_unchanged_alongside_granular_facts() {
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
        &CatalogInput { awareness: Some(&[node]), convoys: &[], independents: &[], standing_roles: &[], project_repositories: &[] },
        &mint(),
    )
    .reassert_patches();
    let convoy = find_entity(&patches, &entity::convoy("dev", "landing", "fleet"));
    assert_eq!(text(convoy, KEY_DISPLAY_LABEL), "landing · PR #1044");
    assert_eq!(text(convoy, KEY_DISPLAY_LABEL_MEDIUM), "landing");
    assert_eq!(text(convoy, KEY_DISPLAY_LABEL_SHORT), "l");
    assert_eq!(text(convoy, KEY_SUMMARY_TEXT), "landing · PR #1044");
    assert_eq!(text(convoy, KEY_CONVOY_NAME), "landing");
    assert_eq!(text(convoy, KEY_CHANGE_REQUEST_NUMBER), "1044");

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
        &CatalogInput { awareness: Some(&[node]), convoys: &[], independents: &[], standing_roles: &[], project_repositories: &[] },
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
        &CatalogInput { awareness: Some(&awareness), convoys: &convoys, independents: &[], standing_roles: &[], project_repositories: &[] },
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
        &CatalogInput { awareness: Some(&[node]), convoys: &[], independents: &[], standing_roles: &[], project_repositories: &[] },
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
        &CatalogInput { awareness: Some(&[node]), convoys: &[], independents: &[], standing_roles: &[], project_repositories: &[] },
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
        &CatalogInput { awareness: Some(&[node]), convoys: &[convoy], independents: &[], standing_roles: &[], project_repositories: &[] },
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
        &CatalogInput { awareness: Some(&[node]), convoys: &[], independents: &[], standing_roles: &[], project_repositories: &[] },
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
        &CatalogInput { awareness: None, convoys: &[], independents: &[row], standing_roles: &[], project_repositories: &[] },
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
        &CatalogInput { awareness: None, convoys: &[with_message], independents: &[], standing_roles: &[], project_repositories: &[] },
        &mint(),
    );
    let current = project_catalog(
        &CatalogInput { awareness: None, convoys: &[without_message], independents: &[], standing_roles: &[], project_repositories: &[] },
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
        &CatalogInput { awareness: None, convoys: &[convoy], independents: &[], standing_roles: &[], project_repositories: &[] },
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
        &CatalogInput { awareness: None, convoys: &[convoy], independents: &[], standing_roles: &[], project_repositories: &[] },
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
        &CatalogInput { awareness: None, convoys: &rows, independents: &[], standing_roles: &[], project_repositories: &[] },
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
        &CatalogInput { awareness: None, convoys: &rows, independents: &[], standing_roles: &[], project_repositories: &[] },
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
        &CatalogInput { awareness: None, convoys, independents: &[], standing_roles: roles, project_repositories: &[] },
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
        &CatalogInput { awareness: None, convoys: &[], independents: &[], standing_roles: &[], project_repositories: &projects },
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
            &CatalogInput { awareness: None, convoys: rows, independents: &[], standing_roles: &[], project_repositories: &[] },
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
