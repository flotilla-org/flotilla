use flotilla_protocol::{result_set::CrewMemberSummary, HostName, RepoKey, ResourceRef};

use super::*;
use crate::recipe::AttachOnlyRecipes;

fn mint() -> AttachOnlyRecipes {
    AttachOnlyRecipes::new("flotilla")
}

fn convoy_ref(namespace: &str, name: &str) -> ResourceRef {
    ResourceRef::new("flotilla/v1", "Convoy", namespace, name).on_host(HostName::new("kiwi"))
}

fn session_ref(namespace: &str, name: &str) -> ResourceRef {
    ResourceRef::new("flotilla/v1", "TerminalSession", namespace, name).on_host(HostName::new("feta"))
}

fn vessel(convoy: &ResourceRef, name: &str, phase: WorkPhase, attach: Option<&str>) -> VesselRow {
    VesselRow::builder()
        .resource(convoy.subresource(format!("vessels/{name}")))
        .name(name)
        .phase(phase)
        .host(HostName::new("feta"))
        .maybe_attach(attach.map(str::to_owned))
        .build()
}

fn session(namespace: &str, name: &str, phase: SessionPhase, repo: Option<&str>, attach: Option<&str>) -> SessionRow {
    SessionRow::builder()
        .resource(session_ref(namespace, name))
        .name(name)
        .maybe_repo(repo.map(|repo| RepoKey(repo.to_owned())))
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
                .crew(vec![CrewMemberSummary { role: "coder".to_owned(), command_preview: "implement it".to_owned() }, CrewMemberSummary {
                    role: "reviewer".to_owned(),
                    command_preview: "review it".to_owned(),
                }])
                .host(HostName::new("feta"))
                .attach("workspace-1")
                .build(),
            vessel(&reference, "review", WorkPhase::Complete, None),
        ])
        .build();
    let catalog = project_catalog(&CatalogInput { convoys: &[convoy], sessions: &[] }, &mint());
    let patches = catalog.reassert_patches();

    // project_ref wins over the repo as the project segment value.
    let project_segment = GroupSegment::text(SEGMENT_PROJECT, "my-project");
    let project = find(&patches, &group(vec![project_segment.clone()]));
    assert_eq!(text(project, KEY_PROJECT_NAME), "my-project");
    assert_eq!(text(project, KEY_FACTORY_ID), "flotilla:projects/my-project");

    let convoy_segment = GroupSegment::text(SEGMENT_CONVOY, "dev/manifest-extraction");
    let convoy_patch = find(&patches, &group(vec![project_segment.clone(), convoy_segment.clone()]));
    assert_eq!(text(convoy_patch, KEY_CONVOY_PHASE), "active");
    assert_eq!(text(convoy_patch, KEY_CONVOY_WORKFLOW), "implement-review");
    assert_eq!(text(convoy_patch, KEY_STATUS_STATE), "active");
    assert_eq!(text(convoy_patch, KEY_SUMMARY_TEXT), "1/2 vessels done");
    assert_eq!(text(convoy_patch, KEY_FACTORY_ID), "flotilla:convoys/dev/manifest-extraction");
    assert!(!convoy_patch.set.contains_key(KEY_STATUS_ATTENTION));
    assert_eq!(convoy_patch.set[KEY_STATUS_STATE].ttl_ms, Some(CATALOG_TTL_MS));
    assert_eq!(convoy_patch.set[KEY_STATUS_STATE].ordinal, None, "projected groups carry no archipelago ordinal");

    let implement =
        find(&patches, &group(vec![project_segment.clone(), convoy_segment.clone(), GroupSegment::text(SEGMENT_VESSEL, "implement")]));
    assert_eq!(text(implement, KEY_WORK_PHASE), "running");
    assert_eq!(text(implement, KEY_STATUS_STATE), "active");
    assert_eq!(text(implement, KEY_VESSEL_HOST), "feta");
    assert_eq!(text(implement, KEY_MATERIALIZE_TARGET), "workspace");
    assert_eq!(text(implement, KEY_MATERIALIZE_RECIPE), "flotilla attach workspace-1");
    assert_eq!(text(implement, KEY_FACTORY_ID), "flotilla:convoys/dev/manifest-extraction/implement");
    assert_eq!(implement.set[KEY_CREW_ROLES].value, MetadataValue::StringList(vec!["coder".to_owned(), "reviewer".to_owned()]));

    // No daemon-resolvable attach ⇒ truthfully recipe-less.
    let review = find(&patches, &group(vec![project_segment, convoy_segment, GroupSegment::text(SEGMENT_VESSEL, "review")]));
    assert_eq!(text(review, KEY_STATUS_STATE), "done");
    assert!(!review.set.contains_key(KEY_MATERIALIZE_RECIPE));
    assert!(!review.set.contains_key(KEY_MATERIALIZE_TARGET));
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
    let catalog = project_catalog(&CatalogInput { convoys: &[convoy], sessions: &[] }, &mint());
    let patches = catalog.reassert_patches();

    let project_segment = GroupSegment::text(SEGMENT_PROJECT, "flotilla-org/flotilla");
    let convoy_patch = find(&patches, &group(vec![project_segment, GroupSegment::text(SEGMENT_CONVOY, "dev/db-growth")]));
    assert_eq!(text(convoy_patch, KEY_STATUS_STATE), "failed");
    assert_eq!(convoy_patch.set[KEY_STATUS_ATTENTION].value, MetadataValue::Bool(true));
    assert_eq!(text(convoy_patch, KEY_CONVOY_MESSAGE), "vessel checkout failed: disk full");
    assert!(!convoy_patch.set.contains_key(KEY_SUMMARY_TEXT), "no vessels, no summary");
}

#[test]
fn ready_vessel_waits_with_attention() {
    let reference = convoy_ref("dev", "auth");
    let convoy = ConvoyRow::builder()
        .resource(reference.clone())
        .name("auth")
        .workflow_ref("implement-review")
        .phase(ConvoyPhase::Active)
        .vessels(vec![vessel(&reference, "implement", WorkPhase::Ready, None)])
        .build();
    let catalog = project_catalog(&CatalogInput { convoys: &[convoy], sessions: &[] }, &mint());
    let patches = catalog.reassert_patches();

    let implement =
        find(&patches, &group(vec![GroupSegment::text(SEGMENT_CONVOY, "dev/auth"), GroupSegment::text(SEGMENT_VESSEL, "implement")]));
    assert_eq!(text(implement, KEY_STATUS_STATE), "waiting");
    assert_eq!(implement.set[KEY_STATUS_ATTENTION].value, MetadataValue::Bool(true), "gated open and not launched demands a look");
}

#[test]
fn session_with_repo_groups_under_project_and_publishes_identity() {
    let session = session("dev", "terminal-scratch", SessionPhase::Running, Some("flotilla-org/flotilla"), Some("terminal-scratch"));
    let catalog = project_catalog(&CatalogInput { convoys: &[], sessions: &[session] }, &mint());
    let patches = catalog.reassert_patches();

    let project_segment = GroupSegment::text(SEGMENT_PROJECT, "flotilla-org/flotilla");
    let group_target = group(vec![project_segment.clone(), GroupSegment::text(SEGMENT_VESSEL, "terminal-scratch")]);
    let group_patch = find(&patches, &group_target);
    assert_eq!(text(group_patch, KEY_STATUS_STATE), "active");
    assert_eq!(text(group_patch, KEY_MATERIALIZE_TARGET), "pane");
    assert_eq!(text(group_patch, KEY_MATERIALIZE_RECIPE), "flotilla attach terminal-scratch");
    assert_eq!(text(group_patch, KEY_FACTORY_ID), "flotilla:sessions/dev/terminal-scratch");
    assert_eq!(group_patch.set[KEY_STATUS_STATE].ordinal, None, "project-parented sessions are not archipelago-ordered");

    let project = find(&patches, &group(vec![project_segment.clone()]));
    assert_eq!(text(project, KEY_PROJECT_NAME), "flotilla", "repo fallback labels the project with the short name");

    let identity = find(&patches, &session_identity("feta/dev/terminal-scratch"));
    assert_eq!(text(identity, KEY_STATUS_STATE), "active");
    let MetadataValue::GroupPath(scope) = &identity.set[KEY_SCOPE].value else {
        panic!("tab.scope is not a group path");
    };
    assert_eq!(scope.len(), 2);
    assert_eq!(scope[0].key, SEGMENT_PROJECT);
    assert_eq!(scope[1].key, SEGMENT_VESSEL);
    assert_eq!(scope[1].value, MetadataPathValue::Text("terminal-scratch".to_owned()));
}

#[test]
fn session_without_repo_is_archipelago_ordered_first() {
    let session = session("dev", "yeoman", SessionPhase::Running, None, Some("yeoman"));
    let catalog = project_catalog(&CatalogInput { convoys: &[], sessions: &[session] }, &mint());
    let patches = catalog.reassert_patches();

    let group_patch = find(&patches, &group(vec![GroupSegment::text(SEGMENT_VESSEL, "yeoman")]));
    assert_eq!(group_patch.set[KEY_STATUS_STATE].ordinal, Some(ARCHIPELAGO_ORDINAL));
    let identity = find(&patches, &session_identity("feta/dev/yeoman"));
    let MetadataValue::GroupPath(scope) = &identity.set[KEY_SCOPE].value else {
        panic!("tab.scope is not a group path");
    };
    assert_eq!(scope.len(), 1, "no fake project segment for archipelago vessels");
}

#[test]
fn session_without_attach_lists_without_recipe() {
    let session = session("dev", "wedged", SessionPhase::Failed, Some("flotilla-org/flotilla"), None);
    let catalog = project_catalog(&CatalogInput { convoys: &[], sessions: &[session] }, &mint());
    let patches = catalog.reassert_patches();

    let group_patch = find(
        &patches,
        &group(vec![GroupSegment::text(SEGMENT_PROJECT, "flotilla-org/flotilla"), GroupSegment::text(SEGMENT_VESSEL, "wedged")]),
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
    let old_session = session("dev", "scratch", SessionPhase::Running, None, Some("scratch"));
    let previous = project_catalog(&CatalogInput { convoys: &[failed], sessions: &[old_session] }, &mint());

    let recovered =
        ConvoyRow::builder().resource(reference).name("auth").workflow_ref("implement-review").phase(ConvoyPhase::Active).build();
    let current = project_catalog(&CatalogInput { convoys: &[recovered], sessions: &[] }, &mint());

    let patches = current.diff_patches(&previous);

    let convoy_target = group(vec![GroupSegment::text(SEGMENT_CONVOY, "dev/auth")]);
    let convoy_patch = find(&patches, &convoy_target);
    assert_eq!(text(convoy_patch, KEY_CONVOY_PHASE), "active");
    assert_eq!(text(convoy_patch, KEY_STATUS_STATE), "active");
    assert!(!convoy_patch.set.contains_key(KEY_CONVOY_WORKFLOW), "unchanged facts are not re-sent in a diff");
    assert!(convoy_patch.unset.contains(&KEY_STATUS_ATTENTION.to_owned()));
    assert!(convoy_patch.unset.contains(&KEY_CONVOY_MESSAGE.to_owned()));

    // The vanished session is explicitly unset on both its targets.
    let session_group = find(&patches, &group(vec![GroupSegment::text(SEGMENT_VESSEL, "scratch")]));
    assert!(session_group.set.is_empty());
    assert!(session_group.unset.contains(&KEY_STATUS_STATE.to_owned()));
    assert!(session_group.unset.contains(&KEY_MATERIALIZE_RECIPE.to_owned()));
    let identity_patch = find(&patches, &session_identity("feta/dev/scratch"));
    assert!(identity_patch.set.is_empty());
    assert!(identity_patch.unset.contains(&KEY_SCOPE.to_owned()));

    assert!(current.diff_patches(&current).is_empty(), "identical catalogs need no patches");
}

#[test]
fn reassert_covers_every_target() {
    let session = session("dev", "scratch", SessionPhase::Running, Some("flotilla-org/flotilla"), Some("scratch"));
    let catalog = project_catalog(&CatalogInput { convoys: &[], sessions: &[session] }, &mint());
    let patches = catalog.reassert_patches();
    assert_eq!(patches.len(), 3, "project group + session group + session identity");
    assert!(patches.iter().all(|patch| patch.unset.is_empty()));
    assert!(patches.iter().all(|patch| patch.source_id == SOURCE_CONNECTOR));
}
