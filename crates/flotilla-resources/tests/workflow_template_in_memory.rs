mod common;

use common::{
    contract::{
        assert_metadata_roundtrip, assert_namespace_isolation, assert_stale_resource_version_conflicts, in_memory_backend,
        WorkflowTemplateFixture,
    },
    updated_workflow_template_spec, valid_workflow_template_spec, workflow_template_meta,
};
use flotilla_resources::WorkflowTemplate;
use rstest::rstest;

// Keep the rstest shape even with a single fixture so this suite can grow into
// shared backend contract coverage without restructuring each test.
#[rstest]
#[case(WorkflowTemplateFixture)]
#[tokio::test]
async fn create_get_list_roundtrip_for_workflow_templates(#[case] _fixture: WorkflowTemplateFixture) {
    let backend = in_memory_backend();
    let templates = backend.definitions::<WorkflowTemplate>("flotilla");
    let created = templates.apply(&workflow_template_meta("alpha"), &valid_workflow_template_spec()).await.expect("apply should succeed");

    let fetched = templates.get("alpha").await.expect("get should succeed");
    assert_eq!(fetched.metadata.resource_version, created.metadata.resource_version);
    let listed = templates.list().await.expect("list should succeed");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].metadata.resource_version, created.metadata.resource_version);
}

#[rstest]
#[case(WorkflowTemplateFixture)]
#[tokio::test]
async fn update_requires_current_resource_version_for_workflow_templates(#[case] _fixture: WorkflowTemplateFixture) {
    assert_stale_resource_version_conflicts::<WorkflowTemplateFixture>().await;
}

#[rstest]
#[case(WorkflowTemplateFixture)]
#[tokio::test]
async fn delete_removes_workflow_template_definition(#[case] _fixture: WorkflowTemplateFixture) {
    let backend = in_memory_backend();
    let templates = backend.definitions::<WorkflowTemplate>("flotilla");
    templates.apply(&workflow_template_meta("alpha"), &valid_workflow_template_spec()).await.expect("apply should succeed");

    templates.delete("alpha").await.expect("delete should succeed");
    assert!(templates.list().await.expect("list should succeed").is_empty());
}

#[rstest]
#[case(WorkflowTemplateFixture)]
#[tokio::test]
async fn apply_updates_workflow_template_definition(#[case] _fixture: WorkflowTemplateFixture) {
    let backend = in_memory_backend();
    let templates = backend.definitions::<WorkflowTemplate>("flotilla");
    let created =
        templates.apply(&workflow_template_meta("alpha"), &valid_workflow_template_spec()).await.expect("initial apply should succeed");
    let updated =
        templates.apply(&workflow_template_meta("alpha"), &updated_workflow_template_spec()).await.expect("updated apply should succeed");

    assert_ne!(updated.metadata.resource_version, created.metadata.resource_version);
    let fetched = templates.get("alpha").await.expect("get should succeed");
    assert_eq!(fetched.metadata.resource_version, updated.metadata.resource_version);
    assert_eq!(fetched.spec.vessels, updated.spec.vessels);
}

#[rstest]
#[case(WorkflowTemplateFixture)]
#[tokio::test]
async fn workflow_templates_are_namespace_isolated(#[case] _fixture: WorkflowTemplateFixture) {
    assert_namespace_isolation::<WorkflowTemplateFixture>().await;
}

#[rstest]
#[case(WorkflowTemplateFixture)]
#[tokio::test]
async fn workflow_template_metadata_roundtrips(#[case] _fixture: WorkflowTemplateFixture) {
    assert_metadata_roundtrip::<WorkflowTemplateFixture>().await;
}
