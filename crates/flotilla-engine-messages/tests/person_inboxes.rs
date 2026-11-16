//! The person roles' inboxes under the declared projects are held and followed from the Projects'
//! changes (#3108): each delivery pass, attention read and person request reads them, so none of
//! those lists the Projects, and a change re-reads only the Project it names.

use flotilla_address::Address;
use flotilla_engine_messages_testkit::{designate_fleet, EngineMessagesHome};
use flotilla_resources::{InputMeta, Project, ProjectSpec, RoleDefinition};
use flotilla_store::ResourceBackend;
use flotilla_store_memory::InMemoryBackend;
use flotilla_store_testkit::{ReadCountsBackendExt, ReadOperation};

const NAMESPACE: &str = "flotilla";

/// Project `name`, whose charter declares `principal_role` held by a principal, if given.
async fn project(backend: &ResourceBackend, name: &str, principal_role: Option<&str>) {
    let mut spec = ProjectSpec::builder().display_name(name.into()).default_workflow_ref("work".into()).repositories(Vec::new()).build();
    if let Some(role) = principal_role {
        spec.role_definitions.insert(role.into(), RoleDefinition { principal: Some("principal:alice".into()), ..Default::default() });
    }
    backend.definitions::<Project>(NAMESPACE).apply(&InputMeta::builder().name(name.into()).build(), &spec).await.expect("project");
}

fn inbox(text: &str) -> Address {
    text.parse().expect("inbox")
}

// Behaviour: a charter's principal-backed role is a person inbox from the read after it is
// declared, and is not once the declaration or the project goes.
#[tokio::test]
async fn person_inboxes_follow_the_projects_changes() {
    let backend = ResourceBackend::new(InMemoryBackend::default());
    designate_fleet(&backend, NAMESPACE).await;
    let home = EngineMessagesHome::open("home", NAMESPACE, &backend);
    let primitives = home.primitives();
    let owner = inbox("project:alpha/role:owner");
    let operator = inbox("project:alpha/role:operator");

    let inboxes = primitives.person_inboxes(NAMESPACE).await.expect("inboxes");
    assert!(!inboxes.contains(&operator), "no project alpha yet: {inboxes:?}");

    project(&backend, "alpha", Some("owner")).await;
    let inboxes = primitives.person_inboxes(NAMESPACE).await.expect("inboxes");
    assert!(inboxes.contains(&owner) && inboxes.contains(&operator), "alpha's declared and person roles: {inboxes:?}");

    project(&backend, "alpha", None).await;
    let inboxes = primitives.person_inboxes(NAMESPACE).await.expect("inboxes");
    assert!(!inboxes.contains(&owner) && inboxes.contains(&operator), "the declaration is gone: {inboxes:?}");

    backend.definitions::<Project>(NAMESPACE).delete("alpha").await.expect("delete");
    let inboxes = primitives.person_inboxes(NAMESPACE).await.expect("inboxes");
    assert!(!inboxes.contains(&operator), "the project is gone: {inboxes:?}");
}

// Read cost: past the first, a read of the person inboxes lists no Project at 5 or 60 projects,
// and one project's change costs the same keyed reads at both sizes.
#[tokio::test]
async fn person_inboxes_list_no_project_after_the_first_read() {
    let mut after_a_change = Vec::new();
    for size in [5, 60] {
        let backend = InMemoryBackend::default().with_read_counts();
        designate_fleet(&backend, NAMESPACE).await;
        for n in 0..size {
            project(&backend, &format!("project-{n}"), Some("owner")).await;
        }
        let home = EngineMessagesHome::open("home", NAMESPACE, &backend);
        let primitives = home.primitives();
        let first = primitives.person_inboxes(NAMESPACE).await.expect("first read");
        assert!(first.contains(&inbox(&format!("project:project-{}/role:owner", size - 1))), "{first:?}");

        let before = backend.read_snapshot();
        let again = primitives.person_inboxes(NAMESPACE).await.expect("held read");
        let reads = backend.read_snapshot().since(&before);
        assert_eq!(again, first);
        assert_eq!(reads.calls("Project", ReadOperation::List), 0, "{size} projects: {reads:#?}");
        assert_eq!(reads.calls("Project", ReadOperation::Get), 0, "{size} projects: {reads:#?}");

        project(&backend, "project-0", None).await;
        let before = backend.read_snapshot();
        let changed = primitives.person_inboxes(NAMESPACE).await.expect("read after a change");
        let reads = backend.read_snapshot().since(&before);
        assert!(!changed.contains(&inbox("project:project-0/role:owner")), "{changed:?}");
        assert_eq!(reads.calls("Project", ReadOperation::List), 0, "{size} projects: {reads:#?}");
        assert_eq!(reads.calls("Project", ReadOperation::Query), 0, "{size} projects: {reads:#?}");
        after_a_change.push(reads.tally("Project", ReadOperation::Get));
    }
    assert_eq!(after_a_change[0], after_a_change[1], "a change's reads do not grow with the projects");
}
