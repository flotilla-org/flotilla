//! The definitions import (#3824 layer 2b, ADR 0047): old copies of the declared definition
//! kinds mapped onto the engine's plain kinds and read back from a rehearsal store.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, TimeZone, Utc};
use flotilla_provisioning::{
    engine::definition_import::{import_kind, rehearse_definitions, Copy, DefinitionCopies, ImportedDefinition},
    engine::import::ImportReport,
    stored::definition::{
        CharterOrigin, CredentialGrantDefinition, CredentialSpecDefinition, CrewDefaultsDefinition, DefinitionOrigin,
        FleetDesignationDefinition, ForgeDefinition, ImageLayerDefinition, WorkflowTemplateDefinition,
    },
};
use flotilla_resources::{
    builtins::builtin_definitions,
    definition_provenance::{
        CHARTER_REVISION_ANNOTATION, CHARTER_SCOPE_ANNOTATION, CHARTER_SOURCE_ANNOTATION, MANIFEST_MANAGED_BY_VALUE,
        MANIFEST_PATH_ANNOTATION, MANIFEST_RECONCILER_ROOT_ANNOTATION, MANIFEST_REVISION_ANNOTATION, MANIFEST_SOURCE_ANNOTATION,
        MATERIALIZED_PROJECT_ANNOTATION, PROJECT_IMAGE_MANAGER, SOURCE_COMMIT_ANNOTATION, SOURCE_ENTRY_PATH_ANNOTATION,
        SOURCE_REPOSITORY_ANNOTATION,
    },
    CredentialConsumer, CredentialGrantSelector, CredentialGrantSpec, CredentialLifecycle, CredentialPlacementRequirements,
    CredentialSource, CredentialSpecSpec, CrewDefaultsSpec, FleetDesignationSpec, ForgeKind, ForgeSpec, HostActionSelector,
    HostImageAction, ImageCacheBinding, ImageLayerResourceSpec, ImageSelection, LandingCredentialScope, ObjectMeta, RepositoryKey,
    RepositoryTrust, Resource, ResourceObject, RoleDefinition, RoleSubscription, WorkflowTemplateSpec, MANAGED_BY_LABEL,
    PREPARED_SNAPSHOT_LABEL, WORKFLOW_SNAPSHOT_KIND,
};
use hegel::{generators as gs, TestCase};

fn at(seconds: i64) -> DateTime<Utc> {
    Utc.timestamp_opt(seconds, 0).single().expect("time")
}

fn meta(name: &str, labels: BTreeMap<String, String>, annotations: BTreeMap<String, String>, created: i64) -> ObjectMeta {
    ObjectMeta {
        name: name.to_owned(),
        namespace: "flotilla".to_owned(),
        resource_version: "1".to_owned(),
        labels,
        annotations,
        owner_references: Vec::new(),
        finalizers: Vec::new(),
        deletion_timestamp: None,
        creation_timestamp: at(created),
        merge: None,
    }
}

fn copy<R: Resource>(
    name: &str,
    home: &str,
    labels: BTreeMap<String, String>,
    annotations: BTreeMap<String, String>,
    spec: R::Spec,
) -> Copy<R> {
    Copy {
        namespace: "flotilla".to_owned(),
        object: ResourceObject { metadata: meta(name, labels, annotations, 1), spec, status: None },
        home: home.to_owned(),
    }
}

fn pick<'a, T>(tc: &TestCase, items: &'a [T]) -> &'a T {
    &items[tc.draw(gs::integers::<usize>().min_value(0).max_value(items.len() - 1))]
}

fn word(tc: &TestCase) -> String {
    pick(tc, &["a", "b", "c"]).to_string()
}

fn words(tc: &TestCase) -> BTreeSet<String> {
    (0..tc.draw(gs::integers::<usize>().min_value(0).max_value(2))).map(|index| format!("{}{index}", word(tc))).collect()
}

fn maybe<T>(tc: &TestCase, value: impl FnOnce(&TestCase) -> T) -> Option<T> {
    tc.draw(gs::booleans()).then(|| value(tc))
}

/// The six stamps the writers make, by index: an operator's, a manifest document's, a charter
/// document's with its materialised project, project ops', a project image's and a snapshot's;
/// each with the origin the import must read from it.
const STAMPS: u8 = 6;

fn provenance(stamp: u8) -> (BTreeMap<String, String>, BTreeMap<String, String>, DefinitionOrigin) {
    let pairs = |items: &[(&str, &str)]| items.iter().map(|(key, value)| (key.to_string(), value.to_string())).collect::<BTreeMap<_, _>>();
    match stamp {
        0 => (BTreeMap::new(), BTreeMap::new(), DefinitionOrigin::Operator),
        1 | 2 => {
            let mut annotations = pairs(&[
                (MANIFEST_RECONCILER_ROOT_ANNOTATION, "root"),
                (MANIFEST_SOURCE_ANNOTATION, "src"),
                (MANIFEST_PATH_ANNOTATION, "a.yaml"),
                (MANIFEST_REVISION_ANNOTATION, "r1"),
            ]);
            let charter =
                (stamp == 2).then(|| CharterOrigin { source: "charter".into(), revision: "c1".into(), scope: "flotilla/p".into() });
            if charter.is_some() {
                annotations.extend(pairs(&[
                    (CHARTER_SOURCE_ANNOTATION, "charter"),
                    (CHARTER_REVISION_ANNOTATION, "c1"),
                    (CHARTER_SCOPE_ANNOTATION, "flotilla/p"),
                ]));
            }
            let materialized_project = (stamp == 2).then(|| "p".to_owned());
            if materialized_project.is_some() {
                annotations.insert(MATERIALIZED_PROJECT_ANNOTATION.into(), "p".into());
            }
            let origin = DefinitionOrigin::Manifest {
                root: "root".into(),
                source: "src".into(),
                path: "a.yaml".into(),
                revision: "r1".into(),
                charter,
                materialized_project,
            };
            (pairs(&[(MANAGED_BY_LABEL, MANIFEST_MANAGED_BY_VALUE)]), annotations, origin)
        }
        3 => (
            BTreeMap::new(),
            pairs(&[
                (MATERIALIZED_PROJECT_ANNOTATION, "p"),
                (SOURCE_REPOSITORY_ANNOTATION, "repo"),
                (SOURCE_COMMIT_ANNOTATION, "abc"),
                (SOURCE_ENTRY_PATH_ANNOTATION, ".flotilla/ops/x.yaml"),
            ]),
            DefinitionOrigin::ProjectOps {
                project: "p".into(),
                repository: "repo".into(),
                commit: "abc".into(),
                entry_path: ".flotilla/ops/x.yaml".into(),
            },
        ),
        4 => (
            pairs(&[(MANAGED_BY_LABEL, PROJECT_IMAGE_MANAGER)]),
            pairs(&[(MATERIALIZED_PROJECT_ANNOTATION, "p"), (SOURCE_COMMIT_ANNOTATION, "abc")]),
            DefinitionOrigin::ProjectImage { project: "p".into(), commit: "abc".into() },
        ),
        _ => (pairs(&[(PREPARED_SNAPSHOT_LABEL, WORKFLOW_SNAPSHOT_KIND)]), BTreeMap::new(), DefinitionOrigin::Snapshot),
    }
}

// Every consumer and source variant, every lifecycle, with and without their optional parts.
fn credential_spec(tc: &TestCase) -> CredentialSpecSpec {
    let consumer = match tc.draw(gs::integers::<u8>().min_value(0).max_value(8)) {
        0 => CredentialConsumer::Gh,
        1 => CredentialConsumer::GithubApp {
            actor_login: maybe(tc, word),
            installation_id: maybe(tc, |tc| tc.draw(gs::integers::<u64>().min_value(0).max_value(u64::MAX))),
            installation_repository: maybe(tc, word),
            permissions: maybe(tc, |tc| words(tc).into_iter().map(|name| (name, "read".to_owned())).collect()),
        },
        2 => CredentialConsumer::Forgejo { forge_ref: word(tc), username: word(tc) },
        3 => CredentialConsumer::GitHttpToken { host: word(tc), username: word(tc) },
        4 => CredentialConsumer::Claude,
        5 => CredentialConsumer::ClaudeOauth { account_email: word(tc) },
        6 => CredentialConsumer::Codex,
        7 => CredentialConsumer::DockerRegistry { registry: word(tc), username: word(tc) },
        _ => CredentialConsumer::ReviewBundleStore {
            endpoint: word(tc),
            bucket: word(tc),
            region: word(tc),
            public_base_url: word(tc),
            allow_http: tc.draw(gs::booleans()),
            virtual_hosted_style: tc.draw(gs::booleans()),
        },
    };
    let source = match tc.draw(gs::integers::<u8>().min_value(0).max_value(3)) {
        0 => CredentialSource::File { path: word(tc) },
        1 => CredentialSource::Env { name: word(tc) },
        2 => CredentialSource::IssueCommand { command: word(tc), args: words(tc).into_iter().collect() },
        _ => CredentialSource::GithubApp { app_id_path: word(tc), private_key_path: word(tc) },
    };
    CredentialSpecSpec {
        consumer,
        source,
        lifecycle: *pick(tc, &[CredentialLifecycle::Static, CredentialLifecycle::Refreshable, CredentialLifecycle::Issued]),
        placement: CredentialPlacementRequirements { binaries: words(tc) },
        authorization_epoch: tc.draw(gs::integers::<u32>().min_value(1).max_value(u32::MAX)),
    }
}

// A work grant or a host-action grant, each selector set empty or not, both landing scopes.
fn credential_grant(tc: &TestCase) -> CredentialGrantSpec {
    let host_action = maybe(tc, |tc| HostActionSelector {
        action: *pick(tc, &[HostImageAction::ImagePull, HostImageAction::ImagePush, HostImageAction::ImageBuildSecret]),
        hosts: words(tc),
    });
    CredentialGrantSpec {
        selector: CredentialGrantSelector {
            host_action,
            projects: words(tc),
            repositories: words(tc).into_iter().map(RepositoryKey).collect(),
            roles: words(tc),
            repository_trust: maybe(tc, |tc| *pick(tc, &[RepositoryTrust::Own, RepositoryTrust::Fork])),
        },
        credentials: words(tc),
        permissions: words(tc)
            .into_iter()
            .map(|credential| (credential, BTreeMap::from([("contents".to_owned(), "write".to_owned())])))
            .collect(),
        landing_credentials: words(tc)
            .into_iter()
            .map(|credential| {
                let scope = if tc.draw(gs::booleans()) {
                    LandingCredentialScope::Branch { repository: RepositoryKey(word(tc)), branch: word(tc) }
                } else {
                    LandingCredentialScope::TemporalOnly
                };
                (credential, scope)
            })
            .collect(),
    }
}

// A crew defaults layer: roles with and without each optional part, skills and image or none.
fn crew_defaults(tc: &TestCase) -> CrewDefaultsSpec {
    let role = |tc: &TestCase| RoleDefinition {
        agent: maybe(tc, word),
        model: maybe(tc, word),
        workflow: maybe(tc, word),
        brief_template: maybe(tc, word),
        subscriptions: maybe(tc, |tc| vec![RoleSubscription { topic: word(tc), subtree: tc.draw(gs::booleans()), priority: 2 }]),
        adoptable: maybe(tc, |tc| tc.draw(gs::booleans())),
        principal: maybe(tc, word),
    };
    CrewDefaultsSpec {
        project_ref: maybe(tc, word),
        default_workflow_ref: maybe(tc, word),
        roles: words(tc).into_iter().map(|name| (name, role(tc))).collect(),
        skills: words(tc).into_iter().map(|name| (name, words(tc).into_iter().collect())).collect(),
        image: maybe(tc, |tc| ImageSelection { base: maybe(tc, word), layers: words(tc).into_iter().collect() }),
    }
}

fn forge(tc: &TestCase) -> ForgeSpec {
    ForgeSpec {
        forge_id: word(tc),
        kind: *pick(tc, &[ForgeKind::Github, ForgeKind::Forgejo]),
        hosts: words(tc),
        https_url: format!("https://{}", word(tc)),
        git_ssh_host: word(tc),
    }
}

fn fleet(tc: &TestCase) -> FleetDesignationSpec {
    FleetDesignationSpec {
        fleet: maybe(tc, word),
        project: word(tc),
        image_cache: maybe(tc, |tc| ImageCacheBinding {
            repository: format!("reg/{}", word(tc)),
            pull_credential: word(tc),
            push_credential: word(tc),
        }),
    }
}

/// The shipped templates and stdlib layers: the wide kinds' real documents, under names of their own.
fn shipped<S: serde::de::DeserializeOwned>(kind: &str) -> Vec<S> {
    builtin_definitions(kind)
        .expect("built-ins")
        .iter()
        .map(|builtin| serde_json::from_value(builtin.spec.clone()).expect("decodes"))
        .collect()
}

// Behaviour (ADR 0047, #3824 layer 2b): every definition kind's old spec, under each origin its
// writers stamp, imports to a record that reads back from the store unchanged, whose spec is the
// old spec and whose origin is the one the stamp names. Specs are drawn across every enum variant
// and optional part; the wide kinds take the shipped templates and layers.
#[hegel::test]
fn every_definition_imports_with_its_spec_and_origin(tc: TestCase) {
    for stamp in 0..STAMPS {
        imports_with_its_spec_and_origin(&tc, stamp);
    }
}

fn imports_with_its_spec_and_origin(tc: &TestCase, stamp: u8) {
    let (labels, annotations, origin) = provenance(stamp);
    let templates: Vec<WorkflowTemplateSpec> = shipped("WorkflowTemplate");
    let layers: Vec<ImageLayerResourceSpec> = shipped("ImageLayer");
    let (forge_spec, fleet_spec, credential, grant, defaults) =
        (forge(tc), fleet(tc), credential_spec(tc), credential_grant(tc), crew_defaults(tc));
    let (template, layer) = (pick(tc, &templates).clone(), pick(tc, &layers).clone());
    let copies = DefinitionCopies {
        forges: vec![copy("forge", "alpha", labels.clone(), annotations.clone(), forge_spec.clone())],
        fleet_designations: vec![copy("fleet", "alpha", labels.clone(), annotations.clone(), fleet_spec.clone())],
        credential_specs: vec![copy("credential", "alpha", labels.clone(), annotations.clone(), credential.clone())],
        credential_grants: vec![copy("grant", "alpha", labels.clone(), annotations.clone(), grant.clone())],
        crew_defaults: vec![copy("defaults", "alpha", labels.clone(), annotations.clone(), defaults.clone())],
        image_layers: vec![copy("layer", "alpha", labels.clone(), annotations.clone(), layer.clone())],
        workflow_templates: vec![copy("template", "alpha", labels.clone(), annotations.clone(), template.clone())],
    };
    let report = rehearse_definitions(&copies);
    assert!(report.refused.is_empty(), "{:?}", report.refused);
    assert_eq!(report.imported.values().sum::<usize>(), 7, "{report:?}");

    let mut report = ImportReport::default();
    let forge = only(import_kind::<ForgeDefinition>(&copies.forges, &mut report));
    let fleet = only(import_kind::<FleetDesignationDefinition>(&copies.fleet_designations, &mut report));
    let credential_record = only(import_kind::<CredentialSpecDefinition>(&copies.credential_specs, &mut report));
    let grant_record = only(import_kind::<CredentialGrantDefinition>(&copies.credential_grants, &mut report));
    let defaults_record = only(import_kind::<CrewDefaultsDefinition>(&copies.crew_defaults, &mut report));
    let layer_record = only(import_kind::<ImageLayerDefinition>(&copies.image_layers, &mut report));
    let template_record = only(import_kind::<WorkflowTemplateDefinition>(&copies.workflow_templates, &mut report));
    for record_origin in [
        &forge.origin,
        &fleet.origin,
        &credential_record.origin,
        &grant_record.origin,
        &defaults_record.origin,
        &layer_record.origin,
        &template_record.origin,
    ] {
        assert_eq!(record_origin, &origin);
    }
    assert_eq!(forge.spec(), forge_spec);
    assert_eq!(fleet.spec(), fleet_spec);
    assert_eq!(credential_record.spec(), credential);
    assert_eq!(grant_record.spec(), grant);
    assert_eq!(defaults_record.spec(), defaults);
    assert_eq!(Some(layer_record.spec()), layer.current());
    assert_eq!(template_record.spec(), &template);
}

fn only<S>(imported: Vec<ImportedDefinition<S>>) -> S {
    let [imported] = <[_; 1]>::try_from(imported).ok().expect("one record");
    imported.record
}

fn manifest_stamp(root: &str) -> (BTreeMap<String, String>, BTreeMap<String, String>) {
    let labels = BTreeMap::from([(MANAGED_BY_LABEL.to_owned(), MANIFEST_MANAGED_BY_VALUE.to_owned())]);
    let annotations = [
        (MANIFEST_RECONCILER_ROOT_ANNOTATION, root),
        (MANIFEST_SOURCE_ANNOTATION, "src"),
        (MANIFEST_PATH_ANNOTATION, "forge.yaml"),
        (MANIFEST_REVISION_ANNOTATION, "r1"),
    ]
    .iter()
    .map(|(key, value)| (key.to_string(), value.to_string()))
    .collect();
    (labels, annotations)
}

fn forge_spec(url: &str) -> ForgeSpec {
    ForgeSpec {
        forge_id: "f".into(),
        kind: ForgeKind::Forgejo,
        hosts: BTreeSet::from(["h".into()]),
        https_url: url.into(),
        git_ssh_host: "h".into(),
    }
}

// Behaviour (#3824 layer 2b plan, the module's table): which copies the import carries and where.
// A declared name gets one record at each declaring home; an undeclared one a record at the home
// that created it, the other copies folding into it; deleted copies, retired layers and stored
// copies of a built-in are dropped and counted; a merge conflict, an incomplete stamp and a name
// held in two namespaces at one home are refused.
#[test]
fn the_import_carries_each_definition_to_the_homes_that_declare_it() {
    let homes = |copies: &[Copy<flotilla_resources::Forge>]| {
        let mut report = ImportReport::default();
        let records = import_kind::<ForgeDefinition>(copies, &mut report);
        (records.into_iter().map(|record| (record.namespace, record.name, record.home)).collect::<Vec<_>>(), report)
    };
    let row = |namespace: &str, name: &str, home: &str, created: i64| (namespace.to_owned(), name.to_owned(), home.to_owned(), created);
    let at_home = |home: &str, stamp: Option<&str>, spec: ForgeSpec, created: i64| {
        let (labels, annotations) = stamp.map(manifest_stamp).unwrap_or_default();
        let mut copy = copy::<flotilla_resources::Forge>("f", home, labels, annotations, spec);
        copy.object.metadata.creation_timestamp = at(created);
        copy
    };
    let place =
        |records: Vec<(String, String, String)>| records.into_iter().map(|(ns, name, home)| row(&ns, &name, &home, 0)).collect::<Vec<_>>();

    // Declared at two homes: a record at each.
    let (records, report) =
        homes(&[at_home("alpha", Some("a-root"), forge_spec("https://x"), 1), at_home("beta", Some("b-root"), forge_spec("https://x"), 2)]);
    assert_eq!(place(records), vec![row("flotilla", "f", "alpha", 0), row("flotilla", "f", "beta", 0)]);
    assert!(report.notes.is_empty() && report.refused.is_empty());

    // Undeclared at two homes: the creating home's record; the other copy folds into it.
    let (records, report) = homes(&[at_home("alpha", None, forge_spec("https://x"), 5), at_home("beta", None, forge_spec("https://x"), 3)]);
    assert_eq!(place(records), vec![row("flotilla", "f", "beta", 0)]);
    assert_eq!(report.notes.len(), 1);

    // Declared at one home, copied at another: the declaring home's record.
    let (records, report) =
        homes(&[at_home("alpha", None, forge_spec("https://x"), 1), at_home("beta", Some("b-root"), forge_spec("https://x"), 2)]);
    assert_eq!(place(records), vec![row("flotilla", "f", "beta", 0)]);
    assert_eq!(report.notes.len(), 1);

    // Two homes' concurrent values: a conflict, refused.
    let (records, report) = homes(&[at_home("alpha", None, forge_spec("https://x"), 1), at_home("beta", None, forge_spec("https://y"), 1)]);
    assert!(records.is_empty());
    assert_eq!(report.refused.iter().map(|refusal| refusal.record.as_str()).collect::<Vec<_>>(), ["f"]);

    // Deleted: dropped and counted.
    let mut deleted = at_home("alpha", None, forge_spec("https://x"), 1);
    deleted.object.metadata.deletion_timestamp = Some(at(2));
    let (records, report) = homes(&[deleted]);
    assert!(records.is_empty() && report.refused.is_empty());
    assert_eq!(report.dropped.values().sum::<usize>(), 1);

    // A stamp naming the manifest without all of its fields: refused, whether the root alone is
    // missing or only the managed-by label is left.
    let mut unstamped = at_home("alpha", Some("a-root"), forge_spec("https://x"), 1);
    unstamped.object.metadata.annotations.remove(MANIFEST_RECONCILER_ROOT_ANNOTATION);
    let mut label_only = at_home("alpha", Some("a-root"), forge_spec("https://x"), 1);
    label_only.object.metadata.annotations.clear();
    for copy in [unstamped, label_only] {
        let (records, report) = homes(&[copy]);
        assert!(records.is_empty());
        assert_eq!(report.refused.len(), 1);
    }

    // One name in two namespaces at one home: the engine has no namespaces, so both are refused.
    let mut other = at_home("alpha", None, forge_spec("https://x"), 1);
    other.namespace = "lab".into();
    let (records, report) = homes(&[at_home("alpha", None, forge_spec("https://x"), 1), other]);
    assert!(records.is_empty());
    assert_eq!(report.refused.len(), 2);

    // A built-in's stored copy and a retired layer: dropped and counted, never imported.
    let layers: Vec<ImageLayerResourceSpec> = shipped("ImageLayer");
    let copies = DefinitionCopies {
        workflow_templates: vec![copy("single-agent", "alpha", BTreeMap::new(), BTreeMap::new(), WorkflowTemplateSpec::builder().build())],
        image_layers: vec![
            copy("retired", "alpha", BTreeMap::new(), BTreeMap::new(), ImageLayerResourceSpec::Retired),
            copy(&builtin_definitions("ImageLayer").expect("stdlib")[0].name, "alpha", BTreeMap::new(), BTreeMap::new(), layers[0].clone()),
        ],
        ..Default::default()
    };
    let report = rehearse_definitions(&copies);
    assert!(report.imported.is_empty() && report.refused.is_empty(), "{report:?}");
    assert_eq!(report.dropped.values().sum::<usize>(), 3, "{:?}", report.dropped);
}
