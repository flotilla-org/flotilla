//! Spelling a message in the old shape reads, by key, the records that message names (#3108):
//! its convoy role's convoy, whose snapshot names the role's vessel. Unrelated convoys, sessions,
//! artifacts, repositories and projects change no
//! read, and no request lists a kind whole. Each size runs on a fresh backend and Message path,
//! so nothing a read holds is warm.

use std::collections::BTreeMap;

use flotilla_engine_messages_testkit::{crewed_convoy, designate_fleet, EngineMessagesHome};
use flotilla_resources::{
    artifact::blob_digest_of, Artifact, ArtifactSpec, Convoy, ConvoySpec, InputMeta, MessageRelation, Project, ProjectSpec, Repository,
    RepositorySpec, Selector, TerminalBrief, TerminalCrewContext, TerminalSession, TerminalSessionSource, TerminalSessionSpec,
    CONVOY_LABEL, ROLE_LABEL, VESSEL_LABEL,
};
use flotilla_store::ResourceBackend;
use flotilla_store_memory::InMemoryBackend;
use flotilla_store_testkit::{assert_flat_reads, ReadCountsBackendExt, ReadOperation, ReadSnapshot};

const NAMESPACE: &str = "flotilla";
/// A role in a convoy held here, whose snapshot names its vessel.
const HELD: &str = "flotilla/crew/work/coder";

/// A crew session holding `role` in `convoy`'s work `vessel`, labelled as the vessel reconciler labels it.
fn session(convoy: &str, vessel: &str, role: &str) -> (InputMeta, TerminalSessionSpec) {
    let meta = InputMeta::builder()
        .name(format!("{convoy}-{vessel}-{role}"))
        .labels(BTreeMap::from([
            (CONVOY_LABEL.into(), convoy.into()),
            (VESSEL_LABEL.into(), vessel.into()),
            (ROLE_LABEL.into(), role.into()),
        ]))
        .build();
    let spec = TerminalSessionSpec {
        env_ref: "env".into(),
        role: role.into(),
        source: TerminalSessionSource::Agent {
            selector: Selector { capability: "code".into(), adapter: None, model: None },
            brief: TerminalBrief { artifact_digest: None, path: "brief.md".into(), content: "brief".into(), copies: vec![] },
            context: Box::new(TerminalCrewContext { namespace: NAMESPACE.into(), convoy: convoy.into(), vessel_ref: "vessel".into() }),
        },
        cwd: "/repo".into(),
        env: BTreeMap::new(),
        pool: "passthrough".into(),
    };
    (meta, spec)
}

async fn convoy(backend: &ResourceBackend, name: &str) {
    backend
        .using::<Convoy>(NAMESPACE)
        .create(
            &InputMeta::builder().name(name.into()).build(),
            &ConvoySpec::builder().workflow_ref("workflow".into()).role("crew".into()).build(),
        )
        .await
        .expect("convoy");
}

async fn terminal(backend: &ResourceBackend, convoy: &str, vessel: &str, role: &str) {
    let (meta, spec) = session(convoy, vessel, role);
    backend.using::<TerminalSession>(NAMESPACE).create(&meta, &spec).await.expect("session");
}

/// The reads of one cold open read of both roles' messages, with `unrelated` records of each kind held.
async fn open_read(unrelated: usize) -> ReadSnapshot {
    let backend = InMemoryBackend::default().with_read_counts();
    designate_fleet(&backend, NAMESPACE).await;
    let spec = ConvoySpec::builder().workflow_ref("workflow".into()).role("crew".into()).build();
    crewed_convoy(&backend, NAMESPACE, "crew", &spec, &["coder"]).await;
    terminal(&backend, "crew", "work", "coder").await;
    for n in 0..unrelated {
        convoy(&backend, &format!("other-{n}")).await;
        terminal(&backend, &format!("other-{n}"), "work", "coder").await;
        backend
            .using::<Artifact>(NAMESPACE)
            .create(
                &InputMeta::builder().name(format!("artifact-{n}")).build(),
                &ArtifactSpec::builder()
                    .convoy(format!("other-{n}"))
                    .producer("coder".into())
                    .kind("explainer".into())
                    .subject("other".into())
                    .digest(blob_digest_of(b"unrelated"))
                    .size(0)
                    .media_type("text/markdown".into())
                    .expires_at(chrono::Utc::now() + chrono::Duration::days(1))
                    .build(),
            )
            .await
            .expect("unrelated artifact");
        let repository = RepositorySpec::remote(format!("https://github.com/other/repo-{n}")).expect("repository");
        backend
            .using::<Repository>(NAMESPACE)
            .create(&InputMeta::builder().name(repository.key().0.clone()).build(), &repository)
            .await
            .expect("unrelated repository");
        backend
            .definitions::<Project>(NAMESPACE)
            .apply(
                &InputMeta::builder().name(format!("project-{n}")).build(),
                &ProjectSpec::builder().display_name("Other".into()).default_workflow_ref("work".into()).repositories(Vec::new()).build(),
            )
            .await
            .expect("unrelated project");
    }
    let home = EngineMessagesHome::open("home", NAMESPACE, &backend);
    let primitives = home.primitives();
    let intent = flotilla_engine_messages_testkit::intent("system:turn-rules", HELD, MessageRelation::System, "to-crew");
    primitives.admit("to-crew", &intent).await.expect("admit");

    let before = backend.read_snapshot();
    let open = primitives.seat_messages(NAMESPACE, &[HELD.to_owned()], &[]).await.expect("open read");
    let reads = backend.read_snapshot().since(&before);
    let spelled = open.iter().map(|read| (read.object.metadata.name.clone(), read.object.spec.receiver.clone())).collect::<Vec<_>>();
    assert_eq!(spelled, [("to-crew".to_owned(), HELD.to_owned())], "the message is spelled to its role's vessel, from its convoy");
    reads
}

// Behaviour: an old-shape read's record reads follow the messages it spells, not the tables.
#[tokio::test]
async fn spelling_reads_the_records_the_messages_name() {
    let reads = assert_flat_reads([0, 32], open_read).await;
    for kind in ["Artifact", "Convoy", "Forge", "Project", "Repository"] {
        assert_eq!(reads.calls(kind, ReadOperation::List), 0, "no {kind} listing: {reads:#?}");
    }
    assert_eq!(reads.objects("Convoy", ReadOperation::Get), 1, "the held convoy, read by name: {reads:#?}");
}
