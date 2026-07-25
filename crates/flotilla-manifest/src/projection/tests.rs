use flotilla_protocol::{
    result_set::{AwarenessCounts, AwarenessEntry, AwarenessKind, AwarenessNode, AwarenessState, CrewMemberSummary},
    HostName, IssueRef, IssueSource, RepoKey, ResourceRef,
};

use super::*;
use crate::{
    entity::{self, EntityKind},
    keys::{
        KEY_CONVOY, KEY_COUNT_ISSUES, KEY_ENTITY_ID, KEY_ENTITY_KIND, KEY_PRIMARY_ACTION_RECIPE, KEY_PRIMARY_ACTION_TARGET, KEY_SOURCE,
        KEY_VESSEL, SEGMENT_PROJECT, SEGMENT_REPO,
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
        .vessels(vec![vessel().convoy(&reference).name("coder").phase(WorkPhase::Running).materialize("terminal-cutover-coder").call()])
        .build();

    let patches = project_catalog(&CatalogInput { awareness: None, convoys: &[convoy], independents: &[] }, &mint()).reassert_patches();

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
    assert_eq!(text(vessel_patch, KEY_VESSEL), "dev/cutover/coder@feta");
    assert_eq!(
        text(convoy_patch, KEY_PRIMARY_ACTION_TARGET),
        vessel_entity.action_target(),
        "the one-vessel convoy and vessel point at the same live target"
    );
    assert_eq!(text(vessel_patch, KEY_PRIMARY_ACTION_TARGET), vessel_entity.action_target());
    assert_eq!(text(vessel_patch, KEY_PRIMARY_ACTION_RECIPE), "flotilla attach --host 'feta' 'terminal-cutover-coder'");
    assert!(patches.iter().all(|patch| text(patch, KEY_SOURCE) == "flotilla"), "every entity carries producer provenance");
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

    let patches = project_catalog(&CatalogInput { awareness: Some(&[node]), convoys: &[], independents: &[] }, &mint()).reassert_patches();
    let issue_patch = find_entity(&patches, &entity::issue(&issue_ref));
    let project_patch = find_entity(&patches, &entity::project("dev", "platform", "fleet"));
    assert_eq!(text(issue_patch, KEY_ENTITY_KIND), EntityKind::Issue.as_str());
    assert_eq!(
        project_patch.set[KEY_COUNT_ISSUES].value,
        MetadataValue::Integer(1),
        "counts stay on the project entity, not copied onto the issue"
    );
    assert!(!issue_patch.set.contains_key(KEY_COUNT_ISSUES));
    assert!(!issue_patch.set.contains_key(KEY_PRIMARY_ACTION_RECIPE));
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

    let patches =
        project_catalog(&CatalogInput { awareness: Some(&[node]), convoys: &[convoy], independents: &[] }, &mint()).reassert_patches();
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

    let patches = project_catalog(&CatalogInput { awareness: Some(&[node]), convoys: &[], independents: &[] }, &mint()).reassert_patches();

    find_entity(&patches, &entity::repo("flotilla-org/flotilla"));
    find_entity(&patches, &entity::session("independent/dev/governor"));
    assert!(
        patches.iter().all(|patch| !matches!(&patch.target, MetadataTarget::Entity(entity) if entity.kind == EntityKind::Project)),
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
    let patches = project_catalog(&CatalogInput { awareness: None, convoys: &[], independents: &[row] }, &mint()).reassert_patches();
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
    let previous = project_catalog(&CatalogInput { awareness: None, convoys: &[with_message], independents: &[] }, &mint());
    let current = project_catalog(&CatalogInput { awareness: None, convoys: &[without_message], independents: &[] }, &mint());
    let diff = current.diff_patches(&previous);
    let patch = find_entity(&diff, &entity::convoy("dev", "cutover", "kiwi"));
    assert!(patch.unset.contains(&KEY_CONVOY_MESSAGE.to_owned()));
}

#[test]
fn badges_preserve_normalized_status_and_attention() {
    assert_eq!(convoy_badge(ConvoyPhase::Failed, false), Badge { state: BadgeState::Failed, attention: true });
    assert_eq!(work_badge(WorkPhase::Ready), Badge { state: BadgeState::Waiting, attention: true });
    assert_eq!(session_badge(SessionPhase::Running), Badge { state: BadgeState::Active, attention: false });
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
    let patches = project_catalog(&CatalogInput { awareness: None, convoys: &[convoy], independents: &[] }, &mint()).reassert_patches();
    let patch = find_entity(&patches, &entity::vessel("dev", "cutover", "coder", "feta"));
    assert_eq!(patch.set[KEY_CREW_ROLES].value, MetadataValue::StringList(vec!["coder".to_owned()]));
}
