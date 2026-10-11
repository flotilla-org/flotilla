use axum::{
    extract::{Path, State},
    http::Uri,
    routing::get,
    Json, Router,
};
use flotilla_protocol::NodeId;
use flotilla_resources::{
    Convoy, InputMeta, Message, MessagePhase, MessageRelation, MessageSpec, MessageStatus, MessageSubmission, OwnerReference,
    ResourceProvenance,
};
use flotilla_store::{query_resource_kind, MessageQuery, OwnerQuery, ResourceBackend, StoreExt, StoreQuery};
use flotilla_store_http::HttpBackend;
use flotilla_store_memory::InMemoryBackend;
use flotilla_store_sqlite::SqliteBackend;
use flotilla_store_testkit::fixtures::{convoy_meta, convoy_spec};
use flotilla_store_testkit::ReadCountsBackendExt;
use std::collections::BTreeMap;

// HTTP boundary stand-in: it rejects missing/invalid query parameters and
// executes the received contract against an independent real memory store.
async fn endpoint(
    State(backend): State<ResourceBackend>,
    Path((namespace, kind)): Path<(String, String)>,
    uri: Uri,
) -> Json<serde_json::Value> {
    let parameters: BTreeMap<String, String> =
        url::form_urlencoded::parse(uri.query().unwrap_or_default().as_bytes()).into_owned().collect();
    assert_eq!(parameters.len(), 1, "query protocol sends exactly storeQuery");
    let query: StoreQuery = serde_json::from_str(parameters.get("storeQuery").expect("required storeQuery")).expect("valid query");
    Json(query_resource_kind(&backend, &namespace, &kind, &query).await.expect("enforce query"))
}
async fn seed(backend: &ResourceBackend) {
    for (name, label) in [("a", "target"), ("b", "other"), ("shadow", "other")] {
        let mut meta = convoy_meta(name);
        meta.labels.insert("team".into(), label.into());
        meta.labels.insert("role".into(), "coder".into());
        meta.labels.insert("literal".into(), format!("{name}+value"));
        meta.owner_references.push(OwnerReference {
            api_version: "flotilla.work/v1".into(),
            kind: "Project".into(),
            name: "project".into(),
            controller: false,
        });
        backend.using::<Convoy>("flotilla").create(&meta, &convoy_spec("template")).await.unwrap();
    }
    let messages = backend.using::<Message>("flotilla");
    let at = chrono::DateTime::from_timestamp(1, 0).unwrap();
    for name in ["active", "audit"] {
        let spec = MessageSpec::builder()
            .sender("flotilla/sender".into())
            .receiver("flotilla/receiver".into())
            .relation(MessageRelation::Peer)
            .body("body".into())
            .in_reply_to("parent".into())
            .build();
        let mut meta = InputMeta::builder().name(name.into()).build();
        meta.labels.insert("team".into(), "target".into());
        meta.owner_references.push(OwnerReference {
            api_version: "flotilla.work/v1".into(),
            kind: "Project".into(),
            name: "project".into(),
            controller: false,
        });
        let created = messages.create(&meta, &spec).await.unwrap();
        let mut status = MessageStatus::builder()
            .phase(if name == "active" { MessagePhase::Accepted } else { MessagePhase::Expired })
            .since(at)
            .reason("fixture".into())
            .build();
        if name == "active" {
            status.submission = Some(
                MessageSubmission::builder()
                    .batch_id("batch".into())
                    .crew_id("crew".into())
                    .session("session".into())
                    .started_at(at)
                    .members(vec!["active".into()])
                    .build(),
            );
        }
        messages.update_status(name, &created.metadata.resource_version, &status).await.unwrap();
    }

    let source = ResourceBackend::new(InMemoryBackend::default());
    for name in ["shadow", "remote", "0-first"] {
        let mut meta = convoy_meta(name);
        meta.labels.insert("team".into(), "target".into());
        source.using::<Convoy>("flotilla").create(&meta, &convoy_spec("template")).await.unwrap();
    }
    let listed = source.using::<Convoy>("flotilla").list().await.unwrap();
    backend.replace_replicas_typed::<Convoy>(&NodeId::new("local"), "flotilla", &listed, chrono::Utc::now(), None).await.unwrap();
}
async fn contract(backend: &ResourceBackend, authority: &ResourceBackend) {
    // Finite sets and alternatives are indexed selections: duplicates never
    // multiply rows, empty alternatives select nothing, and outer predicates
    // still intersect the union. The same contract runs over every adapter.
    for (alternatives, expected) in [(vec![], vec![]), (vec!["a", "a", "b"], vec!["a", "b"])] {
        let observed = backend.clone().with_read_counts();
        let query = StoreQuery {
            any_of: Some(
                alternatives
                    .into_iter()
                    .map(|name| StoreQuery { fields: BTreeMap::from([("metadata.name".into(), name.into())]), ..Default::default() })
                    .collect(),
            ),
            ..Default::default()
        };
        let result = observed.query_declared::<Convoy>("flotilla", &query).await.unwrap();
        assert_eq!(result.items.iter().map(|source| source.object.metadata.name.as_str()).collect::<Vec<_>>(), expected);
        assert_eq!(observed.read_counts().get("Convoy"), Some(&expected.len()));
        assert_eq!(observed.read_calls().get("Convoy"), Some(&1));
    }
    for (values, expected) in [(vec![], 0), (vec!["a", "a", "missing"], 1), (vec!["a", "b"], 2)] {
        let query = StoreQuery {
            field_values: BTreeMap::from([("metadata.name".into(), values.into_iter().map(String::from).collect())]),
            ..Default::default()
        };
        assert_eq!(backend.query_declared::<Convoy>("flotilla", &query).await.unwrap().items.len(), expected);
    }
    // Prefixes are literal (not SQL LIKE patterns), combine with labels and
    // respect namespace isolation and unfiltered local replica shadowing.
    let query = StoreQuery {
        label_prefixes: BTreeMap::from([("literal".into(), "a+".into())]),
        field_prefixes: BTreeMap::from([("metadata.name".into(), "a".into())]),
        ..Default::default()
    };
    assert_eq!(backend.query_declared::<Convoy>("flotilla", &query).await.unwrap().items.len(), 1);
    assert!(backend.query_declared::<Convoy>("other", &query).await.unwrap().items.is_empty());
    let alternatives = vec![StoreQuery {
        field_values: BTreeMap::from([("metadata.name".into(), vec!["shadow".into(), "remote".into()])]),
        ..Default::default()
    }];
    for (suppress, expected) in [(true, 2), (false, 3)] {
        let query =
            StoreQuery { any_of: Some(alternatives.clone()), include_replicas: true, suppress_self_origin: suppress, ..Default::default() };
        assert_eq!(backend.query_declared::<Convoy>("flotilla", &query).await.unwrap().items.len(), expected);
    }
    // Collection membership selects Project charters before decoding, and
    // writes remove obsolete membership entries without rewriting stored bodies.
    use flotilla_resources::{
        Project, ProjectRepositorySpec, ProjectSpec, RepositoryKey, TerminalSession, TerminalSessionSource, TerminalSessionSpec,
    };
    let projects = authority.using::<Project>("flotilla");
    let member = |repo: &str| ProjectRepositorySpec::builder().repo(RepositoryKey(repo.into())).build();
    let spec = ProjectSpec::builder().display_name("project".into()).repositories(vec![member("target"), member("other")]).build();
    let project = projects.create(&InputMeta::builder().name("membership".into()).build(), &spec).await.unwrap();
    let query = StoreQuery { fields: BTreeMap::from([("spec.repositories[].repo".into(), "target".into())]), ..Default::default() };
    assert_eq!(backend.definitions::<Project>("flotilla").query_declared(&query).await.unwrap().len(), 1);
    let observed = backend.clone().with_read_counts();
    assert_eq!(observed.query_declared::<Project>("flotilla", &query).await.unwrap().items.len(), 1);
    assert_eq!(observed.read_counts().get("Project"), Some(&1));
    let mut next = spec;
    next.repositories = vec![member("other")];
    projects.update(&InputMeta::from(&project.metadata), &project.metadata.resource_version, &next).await.unwrap();
    assert!(backend.query_declared::<Project>("flotilla", &query).await.unwrap().items.is_empty());
    // Hydration includes nonmatching siblings so the selected definition agrees
    // with ordinary causal merge, including a remote membership replacement.
    let peer = ResourceBackend::new(InMemoryBackend::default()).with_local_root(NodeId::new("membership-peer"));
    peer.definitions::<Project>("flotilla")
        .create(
            &InputMeta::builder().name("membership".into()).build(),
            &ProjectSpec::builder().display_name("peer".into()).repositories(vec![member("target")]).build(),
        )
        .await
        .unwrap();
    let peer_rows = peer.using::<Project>("flotilla").list().await.unwrap();
    authority
        .replace_replicas_typed::<Project>(&NodeId::new("membership-peer"), "flotilla", &peer_rows, chrono::Utc::now(), None)
        .await
        .unwrap();
    let observed = backend.clone().with_read_counts();
    let selected = observed.definitions::<Project>("flotilla").query_declared(&query).await.unwrap();
    let expected = authority
        .definitions::<Project>("flotilla")
        .list()
        .await
        .unwrap()
        .into_iter()
        .filter(|project| project.spec.repositories.iter().any(|member| member.repo.0 == "target"))
        .map(|project| project.metadata.name)
        .collect::<Vec<_>>();
    assert_eq!(selected.into_iter().map(|project| project.metadata.name).collect::<Vec<_>>(), expected);
    assert_eq!(observed.read_calls().get("Project"), Some(&1));
    assert_eq!(observed.read_counts().get("Project"), Some(&2), "every sibling of the selected name is hydrated");
    authority
        .replace_replicas_typed::<Project>(
            &NodeId::new("membership-peer"),
            "flotilla",
            &flotilla_resources::ResourceList { items: vec![], resource_version: "0".into(), generation: None },
            chrono::Utc::now(),
            None,
        )
        .await
        .unwrap();
    projects.delete("membership").await.unwrap();
    assert!(backend
        .definitions::<Project>("flotilla")
        .query_declared(&StoreQuery { fields: BTreeMap::from([("spec.repositories[].repo".into(), "other".into())]), ..Default::default() })
        .await
        .unwrap()
        .is_empty());
    // Quoted components address literal annotation keys, including dots and
    // slashes. Presence excludes missing/null fields and sees object values.
    let terminals = authority.using::<TerminalSession>("flotilla");
    let mut meta = InputMeta::builder().name("claimed".into()).build();
    meta.annotations.insert(flotilla_resources::ROLE_ADDRESS_ANNOTATION.into(), "flotilla/coder".into());
    let terminal = terminals
        .create(
            &meta,
            &TerminalSessionSpec::builder()
                .env_ref("env".into())
                .role("coder".into())
                .source(TerminalSessionSource::Tool { command: "sh".into() })
                .cwd("/work".into())
                .pool("pool".into())
    // Cross-kind scope discovery uses source indexes and never decodes an
    // unrelated source body. Quoted label keys retain their literal spelling.
    use flotilla_resources::{TerminalSession, TerminalSessionSource, TerminalSessionSpec};
    authority
        .using::<TerminalSession>("flotilla")
        .create(
            &InputMeta::builder().name("scope-source".into()).labels(BTreeMap::from([("flotilla.work/convoy".into(), "a".into())])).build(),
            &TerminalSessionSpec::builder()
                .env_ref("host".into())
                .role("coder".into())
                .cwd("/workspace".into())
                .pool("cleat".into())
                .source(TerminalSessionSource::Tool { command: "true".into() })
                .build(),
        )
        .await
        .unwrap();
    // Fleet environment aliases use a declared field across every adapter.
    let environment_query = StoreQuery { fields: BTreeMap::from([("spec.env_ref".into(), "env".into())]), ..Default::default() };
    assert_eq!(backend.query_declared::<TerminalSession>("flotilla", &environment_query).await.unwrap().items.len(), 1);
    let query = StoreQuery {
        fields: BTreeMap::from([("metadata.annotations.\"flotilla.work/role-address\"".into(), "flotilla/coder".into())]),
        present: vec!["spec.source.context.convoy".into()],
        ..Default::default()
    };
    assert!(backend.query_declared::<TerminalSession>("flotilla", &query).await.unwrap().items.is_empty());
    let query = StoreQuery { present: vec!["metadata.annotations.\"flotilla.work/role-address\"".into()], ..Default::default() };
    assert_eq!(backend.query_declared::<TerminalSession>("flotilla", &query).await.unwrap().items.len(), 1);
    let mut next_meta = InputMeta::from(&terminal.metadata);
    next_meta.annotations.clear();
    terminals.update(&next_meta, &terminal.metadata.resource_version, &terminal.spec).await.unwrap();
    assert!(backend.query_declared::<TerminalSession>("flotilla", &query).await.unwrap().items.is_empty());
    let query = StoreQuery::related_kind(
        "metadata.name",
        "TerminalSession",
        "metadata.labels.\"flotilla.work/convoy\"",
        StoreQuery::names(["scope-source".into()], true),
    );
    let result = backend.query_declared::<Convoy>("flotilla", &query).await.unwrap();
    assert_eq!(result.items.iter().map(|source| source.object.metadata.name.as_str()).collect::<Vec<_>>(), ["a"]);
    let mut invalid = query.clone();
    invalid.field_matches_query.get_mut("metadata.name").unwrap().source_kind = Some("undeclared-kind".into());
    assert!(invalid.validate(flotilla_store::ResourceKind::of::<Convoy>()).is_err());

    // Virtual repository aliases retain exactly the catalog matching semantics
    // without adding fields to stored bodies.
    let repository = flotilla_resources::RepositorySpec::remote("https://github.com/acme/repo").unwrap();
    authority
        .using::<flotilla_resources::Repository>("flotilla")
        .create(&InputMeta::builder().name(repository.key().to_string()).build(), &repository)
        .await
        .unwrap();
    for target in [repository.leaf_slug(), repository.catalog_slug(), "acme/repo".into(), "missing".into()] {
        let selected = backend
            .query_declared::<flotilla_resources::Repository>(
                "flotilla",
                &StoreQuery { fields: BTreeMap::from([("spec.catalog_targets.*".into(), target.clone())]), ..Default::default() },
            )
            .await
            .unwrap();
        assert_eq!(selected.items.len(), usize::from(repository.matches_catalog_target(&target)), "{target}");
    }
    // Canonical and aliased Forge hosts preserve case-insensitive remote
    // matching without decoding unrelated installations.
    let forge = flotilla_resources::ForgeSpec::builder()
        .forge_id("query-forge".into())
        .kind(flotilla_resources::ForgeKind::Forgejo)
        .hosts(["Forge.Example".into()].into())
        .https_url("https://canonical.example/git".into())
        .git_ssh_host("ssh-alias".into())
        .build();
    authority
        .definitions::<flotilla_resources::Forge>("flotilla")
        .create(&InputMeta::builder().name("query-forge".into()).build(), &forge)
        .await
        .unwrap();
    for host in ["forge.example", "canonical.example", "ssh-alias", "unrelated.example"] {
        let selected = backend
            .query_declared::<flotilla_resources::Forge>(
                "flotilla",
                &StoreQuery { fields: BTreeMap::from([("spec.remote_hosts.*".into(), host.into())]), ..Default::default() },
            )
            .await
            .unwrap();
        assert_eq!(selected.items.len(), usize::from(forge.matches_host(host)), "{host}");
    }
    // Numeric equality preserves the complete u64 domain, including values
    // above SQLite's signed integer range. Stored JSON is canonical decimal.
    for number in [0, 1, i64::MAX as u64, (i64::MAX as u64) + 1, u64::MAX] {
        let spec = flotilla_resources::ChangeRequestSpec::builder()
            .service("numeric-contract".into())
            .scope("scope".into())
            .number(number)
            .observing_authority("query".into())
            .build();
        let name = format!("numeric-{number}");
        authority
            .using::<flotilla_resources::ChangeRequest>("flotilla")
            .create(&InputMeta::builder().name(name.clone()).build(), &spec)
            .await
            .unwrap();
        let selected = backend
            .query_declared::<flotilla_resources::ChangeRequest>(
                "flotilla",
                &StoreQuery {
                    fields: BTreeMap::from([
                        ("spec.service".into(), "numeric-contract".into()),
                        ("spec.scope".into(), "scope".into()),
                        ("spec.number".into(), number.to_string()),
                    ]),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(selected.items.iter().map(|source| source.object.metadata.name.clone()).collect::<Vec<_>>(), [name], "{number}");
    }
    // Array selectors implement membership and absence over empty/wildcard
    // grants, and updates immediately replace old indexed values.
    use flotilla_resources::{CredentialGrant, CredentialGrantSelector, CredentialGrantSpec};
    for (name, projects) in [("wildcard", vec![]), ("selected", vec!["app", "other"]), ("unrelated", vec!["elsewhere"])] {
        authority
            .definitions::<CredentialGrant>("flotilla")
            .create(
                &InputMeta::builder().name(name.into()).build(),
                &CredentialGrantSpec::builder()
                    .credentials(["token".into()].into())
                    .selector(CredentialGrantSelector::builder().projects(projects.into_iter().map(str::to_string).collect()).build())
                    .build(),
            )
            .await
            .unwrap();
    }
    for (query, expected) in [
        (
            StoreQuery { fields: BTreeMap::from([("spec.selector.projects.*".into(), "app".into())]), ..Default::default() },
            vec!["selected"],
        ),
        (StoreQuery { missing_fields: ["spec.selector.projects.*".into()].into(), ..Default::default() }, vec!["wildcard"]),
    ] {
        let selected = backend.query_declared::<CredentialGrant>("flotilla", &query).await.unwrap();
        assert_eq!(selected.items.iter().map(|source| source.object.metadata.name.as_str()).collect::<Vec<_>>(), expected);
    }

    // Membership selects a named inventory once, with empty sets selecting
    // nothing and local shadowing independent of the selected predicates.
    for (names, expected) in
        [(vec!["a", "remote", "shadow", "missing"], vec!["a", "remote", "shadow"]), (vec![], vec![]), (vec!["a", "a"], vec!["a"])]
    {
        let query = StoreQuery::names(names.into_iter().map(str::to_string), true);
        let result = backend.query_declared::<Convoy>("flotilla", &query).await.expect("named inventory");
        assert_eq!(result.items.iter().map(|item| item.object.metadata.name.as_str()).collect::<Vec<_>>(), expected);
    }
    // Membership intersects other selectors rather than broadening them.
    let mut query = StoreQuery::names(["a".into(), "b".into()], true);
    query.labels.insert("team".into(), "target".into());
    let result = backend.query_declared::<Convoy>("flotilla", &query).await.expect("combined membership");
    assert_eq!(result.items.iter().map(|item| item.object.metadata.name.as_str()).collect::<Vec<_>>(), vec!["a"]);
    query.fields.insert("metadata.name".into(), "b".into());
    assert!(backend.query_declared::<Convoy>("flotilla", &query).await.expect("contradictory selectors").items.is_empty());
    let mut query = StoreQuery::names(["a".into()], true);
    query.field_values.insert("spec.undeclared".into(), ["value".into()].into());
    assert!(query.validate(flotilla_store::ResourceKind::of::<Convoy>()).is_err());
    // Literal prefix, absence and alternatives intersect the outer selector;
    // related values span partitions even when only a replica supplies a source.
    for (query, expected) in [
        (
            StoreQuery {
                field_prefixes: BTreeMap::from([("metadata.name".into(), "sh".into())]),
                include_replicas: true,
                ..Default::default()
            },
            vec!["shadow"],
        ),
        (StoreQuery { missing_fields: ["spec.continuation".into()].into(), ..Default::default() }, vec!["a", "b", "shadow"]),
        (
            StoreQuery {
                any_of: vec![StoreQuery::names(["a".into()], false), StoreQuery::names(["remote".into()], true)],
                include_replicas: true,
                ..Default::default()
            },
            vec!["a", "remote"],
        ),
        (
            StoreQuery {
                field_matches_query: BTreeMap::from([(
                    "metadata.name".into(),
                    flotilla_store::FieldQuery {
                        source_kind: None,
                        field: "metadata.name".into(),
                        query: Box::new(StoreQuery::names(["remote".into()], true)),
                    },
                )]),
                include_replicas: true,
                ..Default::default()
            },
            vec!["remote"],
        ),
    ] {
        let records = backend.query_declared::<Convoy>("flotilla", &query).await.expect("composed selectors");
        assert_eq!(records.items.iter().map(|record| record.object.metadata.name.as_str()).collect::<Vec<_>>(), expected);
    }
    // A replica-only source's shared role also selects local target rows;
    // source scope controls which partitions may contribute join values.
    let mut related = StoreQuery::related("spec.role", "spec.role", StoreQuery::names(["remote".into()], true));
    related.include_replicas = true;
    let names = backend
        .query_declared::<Convoy>("flotilla", &related)
        .await
        .unwrap()
        .items
        .into_iter()
        .map(|record| record.object.metadata.name)
        .collect::<Vec<_>>();
    assert_eq!(names, ["0-first", "a", "b", "remote", "shadow"]);
    related.field_matches_query.get_mut("spec.role").unwrap().query.include_replicas = false;
    assert!(backend.query_declared::<Convoy>("flotilla", &related).await.unwrap().items.is_empty());
    // Undeclared nested fields and excessive nesting are refused before access.
    let mut nested = StoreQuery::related("metadata.name", "spec.invalid", StoreQuery::default());
    assert!(backend.query_declared::<Convoy>("flotilla", &nested).await.is_err());
    nested = StoreQuery::default();
    for _ in 0..10 {
        nested = StoreQuery { any_of: vec![nested], ..Default::default() };
    }
    assert!(backend.query_declared::<Convoy>("flotilla", &nested).await.is_err());
    // Equality predicates combine with AND; empty selections return all local rows.
    let query = StoreQuery::default();
    assert_eq!(backend.query_declared::<Convoy>("flotilla", &query).await.unwrap().items.len(), 3);
    let plus = backend
        .query_declared::<Convoy>("flotilla", &StoreQuery::labels(&BTreeMap::from([("literal".into(), "a+value".into())]), false, true))
        .await
        .unwrap();
    assert_eq!(plus.items.iter().map(|item| item.object.metadata.name.as_str()).collect::<Vec<_>>(), vec!["a"]);

    for (fields, labels, expected) in [
        (BTreeMap::new(), BTreeMap::from([("team".into(), "target".into()), ("role".into(), "coder".into())]), vec!["a"]),
        (BTreeMap::from([("metadata.name".into(), "b".into())]), BTreeMap::new(), vec!["b"]),
        (BTreeMap::new(), BTreeMap::from([("team".into(), "missing".into())]), vec![]),
    ] {
        let result = backend.query_declared::<Convoy>("flotilla", &StoreQuery { fields, labels, ..Default::default() }).await.unwrap();
        assert_eq!(result.items.iter().map(|o| o.object.metadata.name.as_str()).collect::<Vec<_>>(), expected);
    }
    // Owner identity is API version + kind + name, and undeclared fields are refused.
    assert_eq!(
        backend
            .query_declared::<Convoy>(
                "flotilla",
                &StoreQuery {
                    owner: Some(OwnerQuery { api_version: "flotilla.work/v1".into(), kind: "Project".into(), name: "project".into() }),
                    ..Default::default()
                }
            )
            .await
            .unwrap()
            .items
            .len(),
        3
    );
    assert!(backend
        .query_declared::<Convoy>(
            "flotilla",
            &StoreQuery { fields: BTreeMap::from([("spec.unknown".into(), "x".into())]), ..Default::default() }
        )
        .await
        .is_err());
    // Local existence suppresses self-origin replicas even when local labels differ.
    let mut query = StoreQuery::labels(&BTreeMap::from([("team".into(), "target".into())]), true, true);
    let result = backend.query_declared::<Convoy>("flotilla", &query).await.unwrap();
    assert_eq!(result.items.iter().map(|o| o.object.metadata.name.as_str()).collect::<Vec<_>>(), vec!["0-first", "a", "remote"]);
    assert!(result.items.iter().any(|o| matches!(o.provenance, ResourceProvenance::Replica { .. })));
    query.suppress_self_origin = false;
    assert_eq!(backend.query_declared::<Convoy>("flotilla", &query).await.unwrap().items.len(), 4);
    // MessageQuery is a case of the same vocabulary on every transport, with
    // history excluded before decoding and an exclusive audit time boundary.
    for (message, expected) in [
        (MessageQuery::active(), 1),
        (MessageQuery::Active { receiver: Some("flotilla/receiver".into()) }, 1),
        (MessageQuery::Active { receiver: Some("flotilla/other".into()) }, 0),
        (MessageQuery::ReplyTo { name: "parent".into() }, 2),
        (MessageQuery::Batch { id: "batch".into() }, 1),
        (MessageQuery::AuditBefore { before: chrono::DateTime::from_timestamp(1, 0).unwrap() }, 0),
        (MessageQuery::AuditBefore { before: chrono::DateTime::from_timestamp(2, 0).unwrap() }, 1),
    ] {
        let observed = backend.clone().with_read_counts();
        let result = observed
            .query_declared::<Message>("flotilla", &StoreQuery { message: Some(message.clone()), ..Default::default() })
            .await
            .unwrap();
        assert_eq!(result.items.len(), expected);
        assert_eq!(observed.read_counts().get("Message"), Some(&expected));
        let combined = StoreQuery {
            message: Some(message),
            fields: BTreeMap::from([("spec.receiver".into(), "flotilla/receiver".into())]),
            labels: BTreeMap::from([("team".into(), "target".into())]),
            owner: Some(OwnerQuery { api_version: "flotilla.work/v1".into(), kind: "Project".into(), name: "project".into() }),
            ..Default::default()
        };
        assert_eq!(
            backend.query_declared::<Message>("flotilla", &combined).await.unwrap().items.len(),
            expected,
            "combined Message predicates"
        );
    }
    for (path, value, expected) in [
        ("spec.receiver", "flotilla/receiver", 2),
        ("spec.in_reply_to", "parent", 2),
        ("status.phase", "accepted", 1),
        ("status.submission.batch_id", "batch", 1),
    ] {
        let result = backend
            .query_declared::<Message>(
                "flotilla",
                &StoreQuery { fields: BTreeMap::from([(path.into(), value.into())]), ..Default::default() },
            )
            .await
            .unwrap();
        assert_eq!(result.items.len(), expected, "declared field {path}");
    }
    assert!(backend.query_declared::<Convoy>("different", &StoreQuery::default()).await.unwrap().items.is_empty());
    assert!(backend
        .query_declared::<Convoy>(
            "flotilla",
            &StoreQuery {
                owner: Some(OwnerQuery { api_version: "other/v1".into(), kind: "Project".into(), name: "project".into() }),
                ..Default::default()
            }
        )
        .await
        .unwrap()
        .items
        .is_empty());
    // Named definition inventories merge every author using the ordinary causal
    // resolver semantics; query pushdown may not omit conflicts or tombstones.
    let projects = authority.definitions::<flotilla_resources::Project>("flotilla");
    projects
        .create(
            &InputMeta::builder().name("declared".into()).build(),
            &flotilla_resources::ProjectSpec::builder().display_name("local".into()).build(),
        )
        .await
        .unwrap();
    let peer = ResourceBackend::new(InMemoryBackend::default()).with_local_root(NodeId::new("peer"));
    peer.definitions::<flotilla_resources::Project>("flotilla")
        .create(
            &InputMeta::builder().name("declared".into()).build(),
            &flotilla_resources::ProjectSpec::builder().display_name("remote".into()).build(),
        )
        .await
        .unwrap();
    authority
        .replace_replicas_typed::<flotilla_resources::Project>(
            &NodeId::new("peer"),
            "flotilla",
            &peer.using::<flotilla_resources::Project>("flotilla").list().await.unwrap(),
            chrono::Utc::now(),
            None,
        )
        .await
        .unwrap();
    let merged = backend.definitions::<flotilla_resources::Project>("flotilla").list_names(["declared".into()]).await.unwrap();
    assert_eq!(merged.len(), 1);
    assert_eq!(serde_json::to_value(&merged[0]).unwrap(), serde_json::to_value(projects.get("declared").await.unwrap()).unwrap());
    // Numeric declared identity fields distinguish alternate record names with
    // the same subject from other requests in that repository.
    for (name, number) in [("request", 42), ("alternate", 42), ("unrelated", 43)] {
        authority
            .using::<flotilla_resources::ChangeRequest>("flotilla")
            .create(
                &InputMeta::builder().name(name.into()).build(),
                &flotilla_resources::ChangeRequestSpec::builder()
                    .service("github.com".into())
                    .scope("owner/repo".into())
                    .number(number)
                    .observing_authority("local".into())
                    .build(),
            )
            .await
            .unwrap();
    }
    let result = backend
        .query_declared::<flotilla_resources::ChangeRequest>(
            "flotilla",
            &StoreQuery {
                fields: BTreeMap::from([
                    ("spec.service".into(), "github.com".into()),
                    ("spec.scope".into(), "owner/repo".into()),
                    ("spec.number".into(), "42".into()),
                ]),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(result.items.iter().map(|item| item.object.metadata.name.as_str()).collect::<Vec<_>>(), ["alternate", "request"]);
    // Indexed queries decode only selected rows, and updates/deletes remove old
    // label, owner and field index entries in the same write transaction.
    let observed = backend.clone().with_read_counts();
    observed
        .query_declared::<Convoy>(
            "flotilla",
            &StoreQuery { fields: BTreeMap::from([("metadata.name".into(), "a".into())]), ..Default::default() },
        )
        .await
        .unwrap();
    assert_eq!(observed.read_counts().get("Convoy"), Some(&1));
    let resolver = authority.using::<Convoy>("flotilla");
    let old = resolver.get("a").await.unwrap();
    let mut meta = flotilla_resources::InputMeta::from(&old.metadata);
    meta.labels.insert("team".into(), "changed".into());
    meta.owner_references.clear();
    resolver.update(&meta, &old.metadata.resource_version, &old.spec).await.unwrap();
    assert!(backend
        .query_declared::<Convoy>("flotilla", &StoreQuery::labels(&BTreeMap::from([("team".into(), "target".into())]), false, true))
        .await
        .unwrap()
        .items
        .is_empty());
    assert_eq!(
        backend
            .query_declared::<Convoy>(
                "flotilla",
                &StoreQuery {
                    owner: Some(OwnerQuery { api_version: "flotilla.work/v1".into(), kind: "Project".into(), name: "project".into() }),
                    ..Default::default()
                }
            )
            .await
            .unwrap()
            .items
            .len(),
        2
    );
    resolver.delete("a").await.unwrap();
    assert!(backend
        .query_declared::<Convoy>("flotilla", &StoreQuery::labels(&BTreeMap::from([("team".into(), "changed".into())]), false, true))
        .await
        .unwrap()
        .items
        .is_empty());
    // A full replacement removes absent replica names from the index.
    authority
        .replace_replicas_typed::<Convoy>(
            &NodeId::new("local"),
            "flotilla",
            &flotilla_resources::ResourceList { items: vec![], resource_version: "0".into(), generation: None },
            chrono::Utc::now(),
            None,
        )
        .await
        .unwrap();
    assert!(backend
        .query_declared::<Convoy>("flotilla", &StoreQuery::labels(&BTreeMap::from([("team".into(), "target".into())]), true, true))
        .await
        .unwrap()
        .items
        .is_empty());
}
#[tokio::test]
async fn shared_query_contract() {
    let sqlite = ResourceBackend::new(SqliteBackend::open_in_memory().unwrap());
    let memory = ResourceBackend::new(InMemoryBackend::default());
    for backend in [&sqlite, &memory] {
        seed(backend).await;
        contract(backend, backend).await;
    }
    // Durable local names deliberately differ from the composing memory store:
    // remote exists only durably; shadow exists only in the memory local set.
    let durable = ResourceBackend::new(SqliteBackend::open_in_memory().unwrap());
    durable.using::<Convoy>("flotilla").create(&convoy_meta("remote"), &convoy_spec("template")).await.unwrap();
    durable.using::<Convoy>("flotilla").create(&convoy_meta("durable-only"), &convoy_spec("template")).await.unwrap();
    let hybrid = ResourceBackend::new(InMemoryBackend::observed_with_durable_replicas(durable));
    seed(&hybrid).await;
    contract(&hybrid, &hybrid).await;
    let server = ResourceBackend::new(InMemoryBackend::default());
    seed(&server).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = Router::new().route("/apis/flotilla.work/v1/namespaces/{namespace}/{kind}", get(endpoint)).with_state(server.clone());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let http = ResourceBackend::new(HttpBackend::new(flotilla_tls::client(), format!("http://{addr}")));
    contract(&http, &server).await;
    task.abort();
}

// Index results agree with direct equality semantics after every generated
// insert/update/delete, including duplicate labels and empty/missing fields.
#[hegel::test]
fn inverted_index_tracks_mutations(tc: hegel::TestCase) {
    use hegel::generators as gs;
    let mut index = flotilla_store::QueryIndex::default();
    let mut objects = BTreeMap::new();
    // Small key/value spaces deliberately cause collisions, repeated updates,
    // missing-label transitions, and repeated deletes; zero covers an empty store.
    let steps = tc.draw(gs::integers::<usize>().min_value(0).max_value(20));
    for _ in 0..steps {
        let name = format!("n{}", tc.draw(gs::integers::<usize>().min_value(0).max_value(3)));
        if tc.draw(gs::booleans()) {
            let label = if tc.draw(gs::booleans()) { "a" } else { "b" };
            let value = serde_json::json!({"kind":"Convoy","metadata":{"name":name,"labels":{"team":label}},"status":{"phase":"running"}});
            index.update(&name, Some(&value));
            objects.insert(name, value);
        } else {
            index.update(&name, None);
            objects.remove(&name);
        }
        for label in ["a", "b", "absent"] {
            let query = StoreQuery::labels(&BTreeMap::from([("team".into(), label.into())]), false, true);
            let expected = objects.iter().filter(|(_, v)| v["metadata"]["labels"]["team"] == label).map(|(k, _)| k.clone()).collect();
            assert_eq!(index.names(&query), expected);
        }
        assert_eq!(index.names(&StoreQuery::default()), objects.keys().cloned().collect());
        for name in ["n0", "n1", "n2", "n3"] {
            let query = StoreQuery { fields: BTreeMap::from([("metadata.name".into(), name.into())]), ..Default::default() };
            let expected = objects.keys().filter(|key| key.as_str() == name).cloned().collect();
            assert_eq!(index.names(&query), expected);
        }
        // Generated mutations maintain membership indexes too, including empty
        // sets and values that disappear after an update or deletion.
        for names in [vec![], vec!["n0"], vec!["n1", "n3"], vec!["missing"]] {
            let query = StoreQuery::names(names.iter().map(|name| (*name).into()), false);
            let expected = objects.keys().filter(|name| names.contains(&name.as_str())).cloned().collect();
            assert_eq!(index.names(&query), expected);
        }
    }
}
