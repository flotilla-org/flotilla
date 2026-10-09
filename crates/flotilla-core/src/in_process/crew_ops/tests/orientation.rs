use std::collections::BTreeMap;

use chrono::Utc;
use flotilla_protocol::CrewCommandContext;
use flotilla_resources::{
    Convoy as ResourceConvoy, CrewWorkPhase, InMemoryBackend, InputMeta, Project, Repository, RepositoryKey, ResourceBackend,
    TerminalSession as ResourceTerminalSession, Vessel,
};

use super::fixture;

#[hegel::test]
fn orientation_follows_live_project(tc: hegel::TestCase) {
    use flotilla_resources::{ProjectRepositoryRole, ProjectRepositorySpec, ProjectSpec, VesselSpec};
    use hegel::generators as gs;
    // Sequences cover empty membership, additions/removals, duplicate repo keys
    // with distinct subpaths, and every role (including multiple roles).
    let replicated = tc.draw(gs::booleans());
    let steps = tc.draw(gs::integers::<usize>().min_value(2).max_value(5));
    let operations: Vec<_> = (0..steps)
        .map(|_| (tc.draw(gs::integers::<usize>().min_value(0).max_value(3)), tc.draw(gs::integers::<usize>().min_value(0).max_value(7))))
        .collect();
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    runtime.block_on(async {
        let (crew, backend, _probe, _config) = fixture(CrewWorkPhase::Working).await;
        backend
            .using::<Vessel>("flotilla")
            .create(
                &InputMeta::builder().name("vessel".into()).build(),
                &VesselSpec {
                    convoy_ref: "crew".into(),
                    vessel_name: "work".into(),
                    placement_policy_ref: "test".into(),
                    adopted_checkout_refs: BTreeMap::new(),
                },
            )
            .await
            .expect("vessel");
        let convoys = backend.using::<ResourceConvoy>("flotilla");
        let mut convoy = convoys.get("crew").await.expect("convoy");
        convoy.spec.project_ref = Some("island".into());
        let admission = convoys
            .update(&InputMeta::from(&convoy.metadata), &convoy.metadata.resource_version, &convoy.spec)
            .await
            .expect("project reference");
        let sessions = backend.using::<ResourceTerminalSession>("flotilla");
        let session = sessions.get("session").await.expect("session");
        sessions
            .update_status(
                "session",
                &session.metadata.resource_version,
                &flotilla_resources::TerminalSessionStatus {
                    crew: Some(flotilla_resources::CrewSessionStatus {
                        id: "governor-id".into(),
                        adapter: "codex".into(),
                        model: None,
                        stance: "trusted-implicit".into(),
                    }),
                    ..Default::default()
                },
            )
            .await
            .expect("crew identity");
        let repository_spec = flotilla_resources::RepositorySpec::remote("https://github.com/example/live").expect("repository spec");
        let repository_key = repository_spec.key();
        backend
            .using::<Repository>("flotilla")
            .create(&InputMeta::builder().name(repository_key.0.clone()).build(), &repository_spec)
            .await
            .expect("live Repository");
        let origin_root = flotilla_protocol::NodeId::new("project-home");
        let origin = if replicated {
            ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(origin_root.clone())
        } else {
            backend.clone()
        };
        let projects = origin.using::<Project>("flotilla");
        let writer = backend.replica_writer::<Project>(origin_root, "flotilla");
        let context = CrewCommandContext::builder().convoy("crew".into()).vessel_ref("vessel".into()).role("coder".into()).build();
        let unavailable = crew.crew_list_internal(&context).await.expect("crew state with missing Project");
        assert!(unavailable.project.is_none());
        assert!(unavailable.project_error.as_deref().expect("explicit charter error").contains("live Project"));
        assert_eq!(unavailable.members[0].role, "coder");
        let mut spec = ProjectSpec::builder().display_name("Island".into()).default_workflow_ref("workflow".into()).build();
        let mut project = projects.create(&InputMeta::builder().name("island".into()).build(), &spec).await.expect("project");
        for (count, role_bits) in operations {
            spec.repositories = (0..count)
                .map(|index| {
                    ProjectRepositorySpec::builder()
                        .repo(if index % 2 == 0 { repository_key.clone() } else { RepositoryKey("missing-repo".into()) })
                        .alias(format!("member-{index}"))
                        .subpath(format!("part-{index}"))
                        .default_branch("main".into())
                        .roles(
                            [ProjectRepositoryRole::Code, ProjectRepositoryRole::Ops, ProjectRepositoryRole::Knowledge]
                                .into_iter()
                                .enumerate()
                                .filter_map(|(bit, role)| (role_bits & (1 << bit) != 0).then_some(role))
                                .collect(),
                        )
                        .build()
                })
                .collect();
            project = projects
                .update(&InputMeta::from(&project.metadata), &project.metadata.resource_version, &spec)
                .await
                .expect("live membership edit");
            if replicated {
                writer.replace(&projects.list().await.expect("source projects"), Utc::now()).await.expect("replicate live Project");
            }
            let ambient = CrewCommandContext::builder().crew_id("governor-id".into()).build();
            let result = crew.crew_list_internal(&ambient).await.expect("orientation through crew identity");
            assert_eq!(result, crew.crew_list_internal(&context).await.expect("explicit orientation"));
            assert!(result.project_error.is_none(), "successful orientation clears the charter error");
            let charter = result.project.expect("live charter");
            assert_eq!((charter.namespace.as_str(), charter.name.as_str()), ("flotilla", "island"));
            assert_eq!(charter.repositories.len(), spec.repositories.len());
            for (actual, expected) in charter.repositories.iter().zip(&spec.repositories) {
                assert_eq!(actual.key, expected.repo);
                assert_eq!(actual.roles, expected.roles);
                assert_eq!(actual.alias, expected.alias);
                assert_eq!(actual.subpath, expected.subpath);
                assert_eq!(actual.default_branch, expected.default_branch);
                if actual.key == repository_key {
                    assert_eq!(actual.remotes, repository_spec.remotes());
                } else {
                    assert!(actual.remotes.is_empty(), "missing Repository retains declared membership");
                }
            }
            let current = convoys.get("crew").await.expect("unchanged convoy");
            assert_eq!(current.metadata.resource_version, admission.metadata.resource_version);
            assert_eq!(current.spec, admission.spec);
            assert_eq!(current.status, admission.status);
        }
        projects.delete("island").await.expect("remove Project");
        if replicated {
            writer.replace(&projects.list().await.expect("source projects"), Utc::now()).await.expect("replicate removal");
        }
        let removed = crew.crew_list_internal(&context).await.expect("crew state after Project removal");
        assert!(removed.project.is_none(), "a removed charter must not fall back to admission");
        assert!(removed.project_error.is_some());
        assert_eq!(removed.members, unavailable.members);
        let mut current = convoys.get("crew").await.expect("convoy");
        current.spec.project_ref = None;
        convoys
            .update(&InputMeta::from(&current.metadata), &current.metadata.resource_version, &current.spec)
            .await
            .expect("unscoped convoy");
        let unscoped = crew.crew_list_internal(&context).await.expect("unscoped query");
        assert!(unscoped.project.is_none());
        assert!(unscoped.project_error.is_none());
    });
}

// A delivered-open subject suppresses the next intent. Admission returns the
// predecessor's durable reference so producers cannot latch an absent Message.

// #2952: orientation reports crew state without reading Message bodies or
// resolving inbox roles. Empty, local and replicated histories give identical
// output and read cost, even with thousands of unrelated messages and expired
// messages for this crew. No transport concurrency affects this read scenario.
#[tokio::test]
async fn crew_list_never_reads_message_history() {
    use flotilla_resources::{Message, MessagePhase, MessageRelation, MessageSpec};

    fn delta(before: &BTreeMap<String, usize>, after: &BTreeMap<String, usize>, kind: &str) -> usize {
        after.get(kind).copied().unwrap_or_default() - before.get(kind).copied().unwrap_or_default()
    }

    let (crew, backend, _, _config) = fixture(CrewWorkPhase::Working).await;
    backend
        .using::<flotilla_resources::Vessel>("flotilla")
        .create(
            &InputMeta::builder().name("crew-work".into()).build(),
            &flotilla_resources::VesselSpec {
                convoy_ref: "crew".into(),
                vessel_name: "work".into(),
                placement_policy_ref: "policy".into(),
                adopted_checkout_refs: Default::default(),
            },
        )
        .await
        .unwrap();
    let context = CrewCommandContext {
        crew_id: None,
        namespace: Some("flotilla".into()),
        convoy: Some("crew".into()),
        vessel_ref: Some("crew-work".into()),
        role: Some("coder".into()),
    };
    let ResourceBackend::InMemory(memory) = &backend else { unreachable!() };
    let before = memory.read_counts();
    let expected = crew.crew_list_internal(&context).await.unwrap();
    assert_eq!(expected.members[0].role, "coder");
    let baseline = memory.read_counts();
    let convoy_reads = delta(&before, &baseline, "Convoy");
    assert_eq!(delta(&before, &baseline, "Message"), 0);
    let messages = backend.using::<Message>("flotilla");
    for index in 0..2003 {
        let receiver = if index < 2000 { format!("flotilla/other-{index}/work/coder") } else { "flotilla/crew/work/coder".into() };
        let message = messages
            .create(
                &InputMeta::builder().name(format!("inbox-{index}")).build(),
                &MessageSpec::builder()
                    .sender("system:test".into())
                    .receiver(receiver)
                    .relation(MessageRelation::System)
                    .body("test".into())
                    .build(),
            )
            .await
            .unwrap();
        if index == 2002 {
            let mut status = message.status.unwrap_or_default();
            status.phase = MessagePhase::Expired;
            messages.update_status(&message.metadata.name, &message.metadata.resource_version, &status).await.unwrap();
        }
    }
    for replicated in [false, true] {
        if replicated {
            backend
                .replica_writer::<Message>(flotilla_protocol::NodeId::new("message-home"), "flotilla")
                .replace(&messages.list().await.unwrap(), Utc::now())
                .await
                .unwrap();
            for record in messages.list().await.unwrap().items {
                messages.delete(&record.metadata.name).await.unwrap();
            }
        }
        let before = memory.read_counts();
        let response = crew.crew_list_internal(&context).await.unwrap();
        let after = memory.read_counts();
        assert_eq!(delta(&before, &after, "Message"), 0, "orientation must not read messages (replicated={replicated})");
        assert_eq!(delta(&before, &after, "Convoy"), convoy_reads, "orientation must not resolve message receivers");
        assert_eq!(response, expected, "history must not affect crew state");
        assert!(serde_json::to_value(response).unwrap()["members"][0].get("messages").is_none(), "orientation has no inbox wire field");
    }
}
