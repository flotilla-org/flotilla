use flotilla_protocol::{
    result_set::{AwarenessCounts, AwarenessEntry, AwarenessKind, AwarenessNode, AwarenessState, CrewMemberSummary},
    HostName, RepoKey, ResourceRef,
};

use super::*;
use crate::{recipe::FlotillaRecipes, wire::MetadataPathValue};

fn mint() -> FlotillaRecipes {
    FlotillaRecipes::new("flotilla")
}

fn convoy_ref(namespace: &str, name: &str) -> ResourceRef {
    ResourceRef::new("flotilla/v1", "Convoy", namespace, name).on_host(HostName::new("kiwi"))
}

fn session_ref(namespace: &str, name: &str) -> ResourceRef {
    ResourceRef::new("flotilla/v1", "TerminalSession", namespace, name).on_host(HostName::new("feta"))
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

#[bon::builder]
fn independent(namespace: &str, name: &str, phase: SessionPhase, repo: Option<&str>, attach: Option<&str>) -> IndependentRow {
    IndependentRow::builder()
        .resource(session_ref(namespace, name))
        .name(name)
        .maybe_repo(repo.map(|repo| RepoKey(repo.to_owned())))
        .maybe_repo_fact(repo.map(|repo| RepoKey(repo.to_owned())))
        .host(HostName::new("feta"))
        .maybe_attach(attach.map(str::to_owned))
        .phase(phase)
        .build()
}

fn group(segments: Vec<GroupSegment>) -> MetadataTarget {
    MetadataTarget::Group(GroupPath(segments))
}

fn session_identity(value: &str) -> MetadataTarget {
    MetadataTarget::Identity(MetadataIdentity { key: KEY_SESSION.to_owned(), value: MetadataValue::text(value) })
}

fn find<'a>(patches: &'a [MetadataPatch], target: &MetadataTarget) -> &'a MetadataPatch {
    patches.iter().find(|patch| &patch.target == target).unwrap_or_else(|| panic!("no patch for {target:?}"))
}

fn text(patch: &MetadataPatch, key: &str) -> String {
    match &patch.set.get(key).unwrap_or_else(|| panic!("no {key} on {:?}", patch.target)).value {
        MetadataValue::Text(value) => value.clone(),
        other => panic!("{key} is not text: {other:?}"),
    }
}

#[test]
fn awareness_tree_projects_project_issue_and_materializable_entries() {
    let issue = AwarenessEntry::builder()
        .id("issue/flotilla-org/flotilla/862".to_string())
        .kind(AwarenessKind::Issue)
        .label("#862 awareness band".to_string())
        .state(AwarenessState::Waiting)
        .as_of(flotilla_protocol::result_set::Timestamp::UNIX_EPOCH)
        .build();
    let vessel = AwarenessEntry::builder()
        .id("vessel/dev/ship-it/coder".to_string())
        .kind(AwarenessKind::Vessel)
        .label("coder".to_string())
        .state(AwarenessState::Active)
        .as_of(flotilla_protocol::result_set::Timestamp::UNIX_EPOCH)
        .phase(flotilla_protocol::AwarenessPhase::Work(WorkPhase::Running))
        .annotations(std::collections::HashMap::from([
            (KEY_VESSEL_HOST.to_string(), "feta".to_string()),
            (SEGMENT_REPO.to_string(), "flotilla-org/flotilla".to_string()),
        ]))
        .build();
    let node = AwarenessNode::builder()
        .id("project/dev/platform".to_string())
        .kind(AwarenessKind::Project)
        .label("platform".to_string())
        .state(AwarenessState::Waiting)
        .as_of(flotilla_protocol::result_set::Timestamp::UNIX_EPOCH)
        .counts(AwarenessCounts::builder().total(2).issues(1).vessels(1).build())
        .entries(vec![issue, vessel])
        .build();

    let catalog = project_catalog(&CatalogInput { awareness: Some(&[node]), convoys: &[], independents: &[] }, &mint());
    let patches = catalog.reassert_patches();

    let project = GroupSegment::text(SEGMENT_PROJECT, "platform");
    let issue_patch = find(
        &patches,
        &group(vec![
            project.clone(),
            GroupSegment::text(SEGMENT_ISSUE, "issue/flotilla-org/flotilla/862").with_label("#862 awareness band"),
        ]),
    );
    assert_eq!(text(issue_patch, KEY_STATUS_STATE), "waiting");
    assert_eq!(issue_patch.set[KEY_STATUS_ATTENTION].value, MetadataValue::Bool(true));

    let vessel_patch = find(
        &patches,
        &group(vec![
            project,
            GroupSegment::text(SEGMENT_REPO, "flotilla-org/flotilla").with_label("flotilla"),
            GroupSegment::text(SEGMENT_CONVOY, "dev/ship-it").with_label("ship-it"),
            GroupSegment::text(SEGMENT_VESSEL, "coder"),
        ]),
    );
    assert_eq!(text(vessel_patch, KEY_WORK_PHASE), "running");
    assert_eq!(text(vessel_patch, KEY_VESSEL_HOST), "feta");
    assert_eq!(text(vessel_patch, KEY_MATERIALIZE_TARGET), "workspace");
    assert_eq!(text(vessel_patch, KEY_MATERIALIZE_RECIPE), "flotilla view 'vessel/dev/ship-it/coder'");
}

#[test]
fn convoy_with_project_ref_projects_the_full_spine() {
    let reference = convoy_ref("dev", "manifest-extraction");
    let convoy = ConvoyRow::builder()
        .resource(reference.clone())
        .name("manifest-extraction")
        .workflow_ref("implement-review")
        .phase(ConvoyPhase::Active)
        .repo(RepoKey("flotilla-org/flotilla".to_owned()))
        .project_ref("my-project")
        .vessels(vec![
            VesselRow::builder()
                .resource(reference.subresource("vessels/implement"))
                .name("implement")
                .phase(WorkPhase::Running)
                .crew(vec![
                    CrewMemberSummary {
                        role: "coder".to_owned(),
                        command_preview: "implement it".to_owned(),
                        requested_stance: None,
                        effective_stance: None,
                    },
                    CrewMemberSummary {
                        role: "reviewer".to_owned(),
                        command_preview: "review it".to_owned(),
                        requested_stance: None,
                        effective_stance: None,
                    },
                ])
                .host(HostName::new("feta"))
                .materialize("terminal-implement")
                .build(),
            vessel().convoy(&reference).name("review").phase(WorkPhase::Complete).call(),
        ])
        .build();
    let catalog = project_catalog(&CatalogInput { awareness: None, convoys: &[convoy], independents: &[] }, &mint());
    let patches = catalog.reassert_patches();

    let project_segment = GroupSegment::text(SEGMENT_PROJECT, "my-project");
    let repo_segment = GroupSegment::text(SEGMENT_REPO, "flotilla-org/flotilla").with_label("flotilla");
    let project = find(&patches, &group(vec![project_segment.clone()]));
    assert_eq!(text(project, KEY_PROJECT_NAME), "my-project");
    assert_eq!(text(project, KEY_FACTORY_ID), "flotilla:projects/my-project");

    let convoy_segment = GroupSegment::text(SEGMENT_CONVOY, "dev/manifest-extraction");
    let convoy_patch = find(&patches, &group(vec![project_segment.clone(), repo_segment.clone(), convoy_segment.clone()]));
    assert_eq!(text(convoy_patch, KEY_CONVOY_PHASE), "active");
    assert_eq!(text(convoy_patch, KEY_CONVOY_WORKFLOW), "implement-review");
    assert_eq!(text(convoy_patch, KEY_STATUS_STATE), "active");
    assert_eq!(text(convoy_patch, KEY_SUMMARY_TEXT), "1/2 vessels done");
    assert_eq!(text(convoy_patch, KEY_FACTORY_ID), "flotilla:convoys/dev/manifest-extraction");
    assert!(!convoy_patch.set.contains_key(KEY_STATUS_ATTENTION));
    assert_eq!(convoy_patch.set[KEY_STATUS_STATE].ttl_ms, Some(CATALOG_TTL_MS));
    assert_eq!(convoy_patch.set[KEY_STATUS_STATE].ordinal, None, "projected groups carry no archipelago ordinal");

    let implement = find(
        &patches,
        &group(vec![
            project_segment.clone(),
            repo_segment.clone(),
            convoy_segment.clone(),
            GroupSegment::text(SEGMENT_VESSEL, "implement"),
        ]),
    );
    assert_eq!(text(implement, KEY_WORK_PHASE), "running");
    assert_eq!(text(implement, KEY_STATUS_STATE), "active");
    assert_eq!(text(implement, KEY_VESSEL_HOST), "feta");
    assert_eq!(text(implement, KEY_MATERIALIZE_TARGET), "workspace");
    assert_eq!(text(implement, KEY_MATERIALIZE_RECIPE), "flotilla attach --host 'feta' 'terminal-implement'");
    assert_eq!(text(implement, KEY_FACTORY_ID), "flotilla:convoys/dev/manifest-extraction/implement");
    assert_eq!(implement.set[KEY_CREW_ROLES].value, MetadataValue::StringList(vec!["coder".to_owned(), "reviewer".to_owned()]));

    // No daemon-resolvable attach ⇒ truthfully recipe-less.
    let review = find(&patches, &group(vec![project_segment, repo_segment, convoy_segment, GroupSegment::text(SEGMENT_VESSEL, "review")]));
    assert_eq!(text(review, KEY_STATUS_STATE), "done");
    assert!(!review.set.contains_key(KEY_MATERIALIZE_RECIPE));
    assert!(!review.set.contains_key(KEY_MATERIALIZE_TARGET));
}

#[test]
fn awareness_repository_group_does_not_masquerade_as_project() {
    let independent = AwarenessEntry::builder()
        .id("independent/dev/governor".to_string())
        .kind(AwarenessKind::Independent)
        .label("governor".to_string())
        .state(AwarenessState::Active)
        .as_of(flotilla_protocol::result_set::Timestamp::UNIX_EPOCH)
        .annotations(std::collections::HashMap::from([(SEGMENT_REPO.to_string(), "flotilla-org/flotilla".to_string())]))
        .build();
    let node = AwarenessNode::builder()
        .id("repo/opaque-repository-key".to_string())
        .kind(AwarenessKind::Project)
        .label("flotilla-org/flotilla".to_string())
        .state(AwarenessState::Active)
        .as_of(flotilla_protocol::result_set::Timestamp::UNIX_EPOCH)
        .entries(vec![independent])
        .build();

    let catalog = project_catalog(&CatalogInput { awareness: Some(&[node]), convoys: &[], independents: &[] }, &mint());
    let patches = catalog.reassert_patches();
    let repo = GroupSegment::text(SEGMENT_REPO, "flotilla-org/flotilla").with_label("flotilla");

    find(&patches, &group(vec![repo.clone(), GroupSegment::text(SEGMENT_VESSEL, "governor").with_label("governor")]));
    assert!(
        patches
            .iter()
            .all(|patch| !matches!(&patch.target, MetadataTarget::Group(path) if path.0.iter().any(|s| s.key == SEGMENT_PROJECT))),
        "Repository-only awareness must not mint a Project segment"
    );
}

#[test]
fn failed_convoy_surfaces_attention_and_message() {
    let convoy = ConvoyRow::builder()
        .resource(convoy_ref("dev", "db-growth"))
        .name("db-growth")
        .workflow_ref("fix")
        .phase(ConvoyPhase::Failed)
        .message("vessel checkout failed: disk full")
        .repo(RepoKey("flotilla-org/flotilla".to_owned()))
        .build();
    let catalog = project_catalog(&CatalogInput { awareness: None, convoys: &[convoy], independents: &[] }, &mint());
    let patches = catalog.reassert_patches();

    let repo_segment = GroupSegment::text(SEGMENT_REPO, "flotilla-org/flotilla").with_label("flotilla");
    let convoy_patch = find(&patches, &group(vec![repo_segment, GroupSegment::text(SEGMENT_CONVOY, "dev/db-growth")]));
    assert_eq!(text(convoy_patch, KEY_STATUS_STATE), "failed");
    assert_eq!(convoy_patch.set[KEY_STATUS_ATTENTION].value, MetadataValue::Bool(true));
    assert_eq!(text(convoy_patch, KEY_CONVOY_MESSAGE), "vessel checkout failed: disk full");
    assert!(!convoy_patch.set.contains_key(KEY_SUMMARY_TEXT), "no vessels, no summary");
}

#[test]
fn initializing_convoy_reads_waiting_whatever_its_phase() {
    let convoy = ConvoyRow::builder()
        .resource(convoy_ref("dev", "warming-up"))
        .name("warming-up")
        .workflow_ref("implement-review")
        .phase(ConvoyPhase::Active)
        .initializing(true)
        .build();
    let catalog = project_catalog(&CatalogInput { awareness: None, convoys: &[convoy], independents: &[] }, &mint());
    let patches = catalog.reassert_patches();

    let convoy_patch = find(&patches, &group(vec![GroupSegment::text(SEGMENT_CONVOY, "dev/warming-up")]));
    assert_eq!(text(convoy_patch, KEY_STATUS_STATE), "waiting", "no workflow snapshot yet is truthfully not active");
    assert_eq!(text(convoy_patch, KEY_CONVOY_PHASE), "active", "the raw phase fact stays truthful too");
}

#[test]
fn ready_vessel_waits_with_attention() {
    let reference = convoy_ref("dev", "auth");
    let convoy = ConvoyRow::builder()
        .resource(reference.clone())
        .name("auth")
        .workflow_ref("implement-review")
        .phase(ConvoyPhase::Active)
        .vessels(vec![vessel().convoy(&reference).name("implement").phase(WorkPhase::Ready).call()])
        .build();
    let catalog = project_catalog(&CatalogInput { awareness: None, convoys: &[convoy], independents: &[] }, &mint());
    let patches = catalog.reassert_patches();

    let implement =
        find(&patches, &group(vec![GroupSegment::text(SEGMENT_CONVOY, "dev/auth"), GroupSegment::text(SEGMENT_VESSEL, "implement")]));
    assert_eq!(text(implement, KEY_STATUS_STATE), "waiting");
    assert_eq!(implement.set[KEY_STATUS_ATTENTION].value, MetadataValue::Bool(true), "gated open and not launched demands a look");
}

#[test]
fn independent_with_repo_groups_under_repo_and_publishes_identity() {
    let mut independent = independent()
        .namespace("dev")
        .name("terminal-scratch")
        .phase(SessionPhase::Running)
        .repo("flotilla-org/flotilla")
        .attach("terminal-scratch")
        .call();
    independent.repo = Some(RepoKey("github.com/flotilla-org/flotilla".to_owned()));
    let catalog = project_catalog(&CatalogInput { awareness: None, convoys: &[], independents: &[independent] }, &mint());
    let patches = catalog.reassert_patches();

    let repo_segment = GroupSegment::text(SEGMENT_REPO, "flotilla-org/flotilla").with_label("flotilla");
    let group_target = group(vec![repo_segment.clone(), GroupSegment::text(SEGMENT_INDEPENDENT, "terminal-scratch")]);
    let group_patch = find(&patches, &group_target);
    assert_eq!(text(group_patch, KEY_STATUS_STATE), "active");
    assert_eq!(text(group_patch, KEY_MATERIALIZE_TARGET), "pane");
    assert_eq!(text(group_patch, KEY_MATERIALIZE_RECIPE), "flotilla attach --host 'feta' 'terminal-scratch'");
    assert_eq!(text(group_patch, KEY_FACTORY_ID), "flotilla:independents/dev/terminal-scratch");
    assert_eq!(group_patch.set[KEY_STATUS_STATE].ordinal, None, "repo-parented independents are not archipelago-ordered");
    assert!(
        !patches.iter().any(|patch| matches!(&patch.target, MetadataTarget::Group(path) if path.0 == vec![repo_segment.clone()])),
        "repository knowledge must not mint a Project group"
    );

    let identity = find(&patches, &session_identity("feta/dev/terminal-scratch"));
    assert_eq!(text(identity, KEY_STATUS_STATE), "active");
    let MetadataValue::GroupPath(scope) = &identity.set[KEY_SCOPE].value else {
        panic!("tab.scope is not a group path");
    };
    assert_eq!(scope.len(), 2);
    assert_eq!(scope[0].key, SEGMENT_REPO);
    assert_eq!(scope[1].key, SEGMENT_INDEPENDENT);
    assert_eq!(scope[1].value, MetadataPathValue::Text("terminal-scratch".to_owned()));
}

#[test]
fn independent_without_repo_is_archipelago_ordered_first() {
    let independent = independent().namespace("dev").name("yeoman").phase(SessionPhase::Running).attach("yeoman").call();
    let catalog = project_catalog(&CatalogInput { awareness: None, convoys: &[], independents: &[independent] }, &mint());
    let patches = catalog.reassert_patches();

    let group_patch = find(&patches, &group(vec![GroupSegment::text(SEGMENT_INDEPENDENT, "yeoman")]));
    assert_eq!(group_patch.set[KEY_STATUS_STATE].ordinal, Some(ARCHIPELAGO_ORDINAL));
    let identity = find(&patches, &session_identity("feta/dev/yeoman"));
    let MetadataValue::GroupPath(scope) = &identity.set[KEY_SCOPE].value else {
        panic!("tab.scope is not a group path");
    };
    assert_eq!(scope.len(), 1, "no fake project segment for archipelago vessels");
}

#[test]
fn independent_without_attach_lists_without_recipe() {
    let independent = independent().namespace("dev").name("wedged").phase(SessionPhase::Failed).repo("flotilla-org/flotilla").call();
    let catalog = project_catalog(&CatalogInput { awareness: None, convoys: &[], independents: &[independent] }, &mint());
    let patches = catalog.reassert_patches();

    let group_patch = find(
        &patches,
        &group(vec![
            GroupSegment::text(SEGMENT_REPO, "flotilla-org/flotilla").with_label("flotilla"),
            GroupSegment::text(SEGMENT_INDEPENDENT, "wedged"),
        ]),
    );
    assert_eq!(text(group_patch, KEY_STATUS_STATE), "failed");
    assert_eq!(group_patch.set[KEY_STATUS_ATTENTION].value, MetadataValue::Bool(true));
    assert!(!group_patch.set.contains_key(KEY_MATERIALIZE_RECIPE));
}

#[test]
fn diff_sets_changes_and_unsets_disappearances() {
    let reference = convoy_ref("dev", "auth");
    let failed = ConvoyRow::builder()
        .resource(reference.clone())
        .name("auth")
        .workflow_ref("implement-review")
        .phase(ConvoyPhase::Failed)
        .message("boom")
        .build();
    let old_independent = independent().namespace("dev").name("scratch").phase(SessionPhase::Running).attach("scratch").call();
    let previous = project_catalog(&CatalogInput { awareness: None, convoys: &[failed], independents: &[old_independent] }, &mint());

    let recovered =
        ConvoyRow::builder().resource(reference).name("auth").workflow_ref("implement-review").phase(ConvoyPhase::Active).build();
    let current = project_catalog(&CatalogInput { awareness: None, convoys: &[recovered], independents: &[] }, &mint());

    let patches = current.diff_patches(&previous);

    let convoy_target = group(vec![GroupSegment::text(SEGMENT_CONVOY, "dev/auth")]);
    let convoy_patch = find(&patches, &convoy_target);
    assert_eq!(text(convoy_patch, KEY_CONVOY_PHASE), "active");
    assert_eq!(text(convoy_patch, KEY_STATUS_STATE), "active");
    assert_eq!(text(convoy_patch, KEY_SOURCE), SOURCE_FLOTILLA);
    assert!(!convoy_patch.set.contains_key(KEY_CONVOY_WORKFLOW), "unchanged facts are not re-sent in a diff");
    assert!(convoy_patch.unset.contains(&KEY_STATUS_ATTENTION.to_owned()));
    assert!(convoy_patch.unset.contains(&KEY_CONVOY_MESSAGE.to_owned()));

    // The vanished independent is explicitly unset on both its targets.
    let independent_group = find(&patches, &group(vec![GroupSegment::text(SEGMENT_INDEPENDENT, "scratch")]));
    assert_eq!(text(independent_group, KEY_SOURCE), SOURCE_FLOTILLA);
    assert!(independent_group.unset.contains(&KEY_STATUS_STATE.to_owned()));
    assert!(independent_group.unset.contains(&KEY_MATERIALIZE_RECIPE.to_owned()));
    let identity_patch = find(&patches, &session_identity("feta/dev/scratch"));
    assert_eq!(text(identity_patch, KEY_SOURCE), SOURCE_FLOTILLA);
    assert!(identity_patch.unset.contains(&KEY_SCOPE.to_owned()));

    assert!(current.diff_patches(&current).is_empty(), "identical catalogs need no patches");
}

#[test]
fn reassert_covers_every_target() {
    let independent =
        independent().namespace("dev").name("scratch").phase(SessionPhase::Running).repo("flotilla-org/flotilla").attach("scratch").call();
    let catalog = project_catalog(&CatalogInput { awareness: None, convoys: &[], independents: &[independent] }, &mint());
    let patches = catalog.reassert_patches();
    assert_eq!(patches.len(), 2, "independent group + independent identity");
    assert!(patches.iter().all(|patch| patch.unset.is_empty()));
    assert!(patches.iter().all(|patch| patch.source_id == SOURCE_CONNECTOR));
    assert!(patches.iter().all(|patch| text(patch, KEY_SOURCE) == SOURCE_FLOTILLA));
}

#[test]
fn latent_catalog_and_live_actuator_use_the_same_dispatched_convoy_group() {
    let reference = convoy_ref("dev", "manifest-extraction");
    let convoy = ConvoyRow::builder()
        .resource(reference.clone())
        .name("manifest-extraction")
        .workflow_ref("implement-review")
        .phase(ConvoyPhase::Active)
        .repo(RepoKey("flotilla-org/flotilla".to_owned()))
        .project_ref("project/dev/flotilla")
        .vessels(vec![vessel().convoy(&reference).name("implement").phase(WorkPhase::Running).call()])
        .build();
    let catalog = project_catalog(&CatalogInput { awareness: None, convoys: &[convoy], independents: &[] }, &mint());
    let patches = catalog.reassert_patches();

    // The actuator's tab stamp builds its scope with these helpers; they
    // must land on exactly the group nodes the catalog publishes.
    let project = project_segment(Some("project/dev/flotilla"));
    let repo = repo_segment(Some("flotilla-org/flotilla"));
    let vessel_target = MetadataTarget::Group(vessel_group_path(project.clone(), repo.clone(), "dev", "manifest-extraction", "implement"));
    let convoy_target = MetadataTarget::Group(convoy_group_path(project, repo, "dev", "manifest-extraction"));
    find(&patches, &vessel_target);
    find(&patches, &convoy_target);
}

#[test]
fn project_and_repo_segments_have_one_meaning_each() {
    assert_eq!(project_segment(Some("project/dev/platform")), Some(GroupSegment::text(SEGMENT_PROJECT, "platform")));
    assert_eq!(project_segment(None), None);
    assert_eq!(
        repo_segment(Some("github.com:flotilla-org/flotilla")),
        Some(GroupSegment::text(SEGMENT_REPO, "github.com:flotilla-org/flotilla").with_label("flotilla"))
    );
    assert_eq!(repo_segment(None), None);
}
