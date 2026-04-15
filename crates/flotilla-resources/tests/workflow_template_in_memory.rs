mod common;

use common::contract::{
    assert_create_get_list_roundtrip, assert_delete_emits_event, assert_metadata_roundtrip, assert_namespace_isolation,
    assert_stale_resource_version_conflicts, assert_watch_from_version_replays, assert_watch_now_semantics, WorkflowTemplateFixture,
};
use rstest::rstest;

// Keep the rstest shape even with a single fixture so this suite can grow into
// shared backend contract coverage without restructuring each test.
#[rstest]
#[case(WorkflowTemplateFixture)]
#[tokio::test]
async fn create_get_list_roundtrip_for_workflow_templates(#[case] _fixture: WorkflowTemplateFixture) {
    assert_create_get_list_roundtrip::<WorkflowTemplateFixture>().await;
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
async fn delete_emits_deleted_event_for_workflow_templates(#[case] _fixture: WorkflowTemplateFixture) {
    assert_delete_emits_event::<WorkflowTemplateFixture>().await;
}

#[rstest]
#[case(WorkflowTemplateFixture)]
#[tokio::test]
async fn watch_from_version_replays_update_then_delete_for_workflow_templates(#[case] _fixture: WorkflowTemplateFixture) {
    assert_watch_from_version_replays::<WorkflowTemplateFixture>().await;
}

#[rstest]
#[case(WorkflowTemplateFixture)]
#[tokio::test]
async fn watch_now_only_sees_future_events_for_workflow_templates(#[case] _fixture: WorkflowTemplateFixture) {
    assert_watch_now_semantics::<WorkflowTemplateFixture>().await;
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
