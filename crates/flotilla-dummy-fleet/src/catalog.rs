//! The definitions a dummy fleet runs against: its forge, a fulfilment kind
//! the dummy environment provider realises, and one workflow per crew mix.
//! Load drivers and scenarios create these resources before dispatching.

use std::path::{Path, PathBuf};

use flotilla_protocol::{CommandAction, ConvoyAutoAttach, ConvoyStartIntent};
use flotilla_resources::{
    definition_provenance::{MATERIALIZED_PROJECT_ANNOTATION, SOURCE_COMMIT_ANNOTATION, SOURCE_REPOSITORY_ANNOTATION},
    CapabilityNeed, ConvoyEnsure, ConvoyEnsureSpec, CrewSource, CrewSpec, ExitDeclaration, FleetDesignation, FleetDesignationSpec, Forge,
    ForgeKind, ForgeSpec, FulfilmentGrant, FulfilmentKind, FulfilmentKindSpec, FulfilmentRealisation, PlacementImageSource, Platform,
    Project, ProjectRepositorySpec, ProjectSpec, Repository, RepositorySpec, Resource, Selector, WorkflowTemplate, WorkflowTemplateSpec,
    FLEET_DESIGNATION_NAME,
};

use crate::{
    crew::{CrewScript, SCRIPT_LINE_PREFIX},
    DUMMY_AGENT_ADAPTER, DUMMY_TERMINAL_POOL,
};

/// Namespace every catalog resource is applied in.
pub const CATALOG_NAMESPACE: &str = "flotilla";
/// The Fleet project: the fleet designation names it, and every dummy convoy
/// belongs to it.
pub const FLEET_PROJECT: &str = "fleet";
/// Forge definition name; subjects on the dummy forge carry it as their service.
pub const DUMMY_FORGE_ID: &str = "dummy-forge";
/// Repository every dummy crew works on, as `owner/name` on the dummy forge.
pub const DUMMY_REPOSITORY: &str = "dummy/fleet";
/// FulfilmentKind name selecting dummy environments.
pub const DUMMY_FULFILMENT_KIND: &str = "dummy-crews";
/// FulfilmentKind name selecting dummy cloud sandboxes.
pub const DUMMY_CLOUD_FULFILMENT_KIND: &str = "dummy-cloud-crews";
/// The Fleet project's standing governor: its convoy role, crew role and ConvoyEnsure name.
pub const GOVERNOR_ROLE: &str = "governor";
/// The standing governor's WorkflowTemplate.
pub const GOVERNOR_WORKFLOW: &str = "dummy-governor";
/// Image named by dummy environments; nothing pulls it.
pub const DUMMY_IMAGE: &str = "dummy/crew:latest";
/// Image named by dummy cloud sandboxes: a cloud backing records the digest
/// its image reference pins.
pub const DUMMY_CLOUD_IMAGE: &str = "dummy/crew@sha256:0000000000000000000000000000000000000000000000000000000000000000";

/// The crew compositions a load mix draws from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CrewMix {
    /// One coder that opens and submits a change request.
    Coder,
    /// A coder whose change request a reviewer answers before it completes.
    CoderReviewer,
    /// A governor-like long runner exchanging many turns with an aide.
    LongRunner,
    /// An eagerly launched supervisor waits for a coder in another vessel.
    CoderSupervisor,
}

impl CrewMix {
    pub const ALL: [Self; 4] = [Self::Coder, Self::CoderReviewer, Self::LongRunner, Self::CoderSupervisor];

    /// WorkflowTemplate name for this mix.
    pub fn workflow_name(self) -> &'static str {
        match self {
            Self::Coder => "dummy-coder",
            Self::CoderReviewer => "dummy-coder-reviewer",
            Self::LongRunner => "dummy-long-runner",
            Self::CoderSupervisor => "dummy-coder-supervisor",
        }
    }

    /// The mix's workflow, authored as roles: admission allocates their
    /// vessels, as it does a real template's. Crews that work on the
    /// repository (coder, reviewer, aide) need `network:scoped` to fetch and
    /// push it; a governor coordinates through crew commands to its daemon and
    /// needs no network. So a coder and its reviewer share a vessel, and a
    /// governor gets one of its own, apart from the crew it supervises or
    /// directs. Crews address one another by role.
    pub fn workflow(self, long_runner_turns: u32) -> WorkflowTemplateSpec {
        let repository = DUMMY_REPOSITORY.to_string();
        let roles = match self {
            Self::Coder => vec![worker("coder", CrewScript::coder(None, repository))],
            Self::CoderReviewer => vec![
                worker("coder", CrewScript::coder(Some("reviewer".into()), repository)),
                worker("reviewer", CrewScript::Responder { reply_to: "coder".into() }),
            ],
            // The governor comes first, so its vessel launches it eagerly.
            Self::CoderSupervisor => vec![
                crew("governor", CrewScript::Responder { reply_to: "coder".into() }),
                worker("coder", CrewScript::coder(Some("governor".into()), repository)),
            ],
            Self::LongRunner => vec![
                crew("governor", CrewScript::Governor { aide: "aide".into(), turns: long_runner_turns }),
                worker("aide", CrewScript::Responder { reply_to: "governor".into() }),
            ],
        };
        WorkflowTemplateSpec::builder().exit(ExitDeclaration::standard_table()).roles(roles).build()
    }
}

/// A crew that works on the repository: it needs scoped network reach.
fn worker(role: &str, script: CrewScript) -> CrewSpec {
    CrewSpec { needs: [CapabilityNeed::Network("scoped".into())].into(), ..crew(role, script) }
}

fn crew(role: &str, script: CrewScript) -> CrewSpec {
    CrewSpec::builder()
        .role(role.to_string())
        .deliverer(matches!(&script, CrewScript::Coder { .. }))
        .promises(vec![flotilla_resources::TemplatePromise::decision_ledger(role)])
        .source(CrewSource::Agent {
            selector: Selector { capability: "coding".into(), adapter: Some(DUMMY_AGENT_ADAPTER.into()), model: None },
            prompt: Some(format!("Dummy crew `{role}`.\n{SCRIPT_LINE_PREFIX} {}", script.render())),
            brief_template: None,
        })
        .build()
}

/// The dummy forge at `service_url` (the daemon's `forge_service_url`).
pub fn forge(service_url: &str) -> ForgeSpec {
    let service_url = service_url.trim_end_matches('/');
    let host = service_url.split_once("://").map_or(service_url, |(_, rest)| rest).split('/').next().unwrap_or_default().to_string();
    ForgeSpec {
        forge_id: DUMMY_FORGE_ID.into(),
        kind: ForgeKind::Forgejo,
        hosts: [host.clone()].into(),
        https_url: service_url.into(),
        git_ssh_host: host,
    }
}

/// The Fleet project: the fleet designation names it, and every dummy convoy
/// is dispatched into it, on the dummy repository.
pub fn fleet_project(repository: &RepositorySpec) -> ProjectSpec {
    ProjectSpec::builder()
        .display_name("Fleet".to_string())
        .default_workflow_ref(CrewMix::Coder.workflow_name().to_string())
        .repositories(vec![ProjectRepositorySpec::builder().repo(repository.key()).build()])
        .build()
}

/// Dispatch the convoy `name` of `workflow` onto the fulfilment kind `kind` in
/// the Fleet project, as `flotilla convoy NAME create --template WORKFLOW
/// --project fleet --ref dummy/NAME --fulfilment KIND` sends it. Each convoy
/// reserves its own branch.
pub fn dispatch(name: &str, workflow: &str, kind: &str) -> CommandAction {
    CommandAction::ConvoyStart {
        intent: Box::new(
            ConvoyStartIntent::builder()
                .project_ref(FLEET_PROJECT.into())
                .name(name.into())
                .branch(format!("dummy/{name}"))
                .workflow_ref(workflow.into())
                .placement_policy(kind.into())
                .auto_attach(ConvoyAutoAttach::Never)
                .build(),
        ),
    }
}

/// The repository every dummy crew works on.
pub fn repository(forge: &ForgeSpec) -> Result<RepositorySpec, String> {
    RepositorySpec::remote(repository_url(forge))?.on_forge(forge)
}

pub fn repository_url(forge: &ForgeSpec) -> String {
    format!("{}/{DUMMY_REPOSITORY}.git", forge.https_url.trim_end_matches('/'))
}

/// Per-vessel dummy environments on `host_ref`, crews in the dummy pool.
pub fn fulfilment_kind(host_ref: &str) -> FulfilmentKindSpec {
    FulfilmentKindSpec::builder()
        .host_ref(host_ref.into())
        .pool(DUMMY_TERMINAL_POOL.to_string())
        .grants([FulfilmentGrant::platform(Platform::Linux.to_string()), FulfilmentGrant::network("scoped".into())].into())
        .realisation(FulfilmentRealisation::DockerPerVessel { image: Some(PlacementImageSource::Literal(DUMMY_IMAGE.into())) })
        .build()
}

/// Per-vessel dummy cloud sandboxes, crews in the dummy pool. The controller
/// advertising the sandbox vendor realises them; `host_ref` names that host
/// for the readiness admission reads.
pub fn cloud_fulfilment_kind(host_ref: &str) -> FulfilmentKindSpec {
    FulfilmentKindSpec::builder()
        .host_ref(host_ref.into())
        .pool(DUMMY_TERMINAL_POOL.to_string())
        .grants([FulfilmentGrant::platform(Platform::Linux.to_string()), FulfilmentGrant::network("scoped".into())].into())
        .realisation(FulfilmentRealisation::CloudSandboxPerVessel {
            image: Some(PlacementImageSource::Literal(DUMMY_CLOUD_IMAGE.into())),
            size: None,
        })
        .build()
}

/// One catalog resource as the document `flotilla resource apply --file` reads.
#[derive(Debug, Clone, PartialEq)]
pub struct CatalogDocument {
    pub kind: &'static str,
    pub name: String,
    /// `{apiVersion, kind, metadata: {name, namespace}, spec}`.
    pub document: serde_json::Value,
}

impl CatalogDocument {
    fn of<T: Resource>(name: &str, spec: &T::Spec) -> Result<Self, String> {
        let spec = serde_json::to_value(spec).map_err(|error| format!("encode {} {name}: {error}", T::API_PATHS.kind))?;
        let document = serde_json::json!({
            "apiVersion": format!("{}/{}", T::API_PATHS.group, T::API_PATHS.version),
            "kind": T::API_PATHS.kind,
            "metadata": { "name": name, "namespace": CATALOG_NAMESPACE },
            "spec": spec,
        });
        Ok(Self { kind: T::API_PATHS.kind, name: name.to_string(), document })
    }

    /// File name for the document at `position` in apply order.
    fn file_name(&self, position: usize) -> String {
        let name: String =
            self.name.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') { c } else { '_' }).collect();
        format!("{position:02}-{}-{name}.json", self.kind)
    }
}

/// Everything a host needs before it admits a dummy convoy, in apply order:
/// the Fleet project and its designation, the dummy forge at
/// `forge_service_url` and its repository, the dummy fulfilment kind on
/// `host_ref` (the local Host's name), and one workflow per crew mix.
pub fn documents(forge_service_url: &str, host_ref: &str, long_runner_turns: u32) -> Result<Vec<CatalogDocument>, String> {
    // Fleet identity and charter Project are declared independently; the
    // engine path resolves fleet roles from the replicated fleet identity.
    let designation = FleetDesignationSpec { fleet: Some("lab".into()), project: FLEET_PROJECT.into(), image_cache: None };
    let forge = forge(forge_service_url);
    // A dispatched repository is tracked first; admission reads its resolved default branch.
    let repository = repository(&forge)?;
    let project = fleet_project(&repository);
    let mut documents = vec![
        CatalogDocument::of::<Project>(FLEET_PROJECT, &project)?,
        CatalogDocument::of::<FleetDesignation>(FLEET_DESIGNATION_NAME, &designation)?,
        CatalogDocument::of::<Forge>(&forge.forge_id, &forge)?,
        CatalogDocument::of::<Repository>(&repository.key().to_string(), &repository)?,
        CatalogDocument::of::<FulfilmentKind>(DUMMY_FULFILMENT_KIND, &fulfilment_kind(host_ref))?,
    ];
    for mix in CrewMix::ALL {
        documents.push(CatalogDocument::of::<WorkflowTemplate>(mix.workflow_name(), &mix.workflow(long_runner_turns))?);
    }
    Ok(documents)
}

/// The standing governor's workflow: one governor that resumes the stalled crew its supervision
/// turns name, and declares no exit, so it runs until it is stopped.
pub fn governor_workflow() -> WorkflowTemplateSpec {
    WorkflowTemplateSpec::builder().roles(vec![crew(GOVERNOR_ROLE, CrewScript::Supervisor)]).build()
}

/// The Fleet project's standing governor, as an operator declares one: the workflow and the
/// ConvoyEnsure that keeps a generation of it running on dummy environments. A convoy's default
/// supervision reaches the project's live `governor` convoy, so every stalled dummy crew's
/// supervision turn goes to it.
pub fn standing_governor_documents(forge_service_url: &str) -> Result<Vec<CatalogDocument>, String> {
    let repository = repository(&forge(forge_service_url))?;
    let ensure = ConvoyEnsureSpec::builder()
        .project_ref(FLEET_PROJECT.into())
        .role(GOVERNOR_ROLE.into())
        .workflow_ref(GOVERNOR_WORKFLOW.into())
        .placement_policy(DUMMY_FULFILMENT_KIND.into())
        .repositories(vec![repository.key()])
        .build();
    // The stamps project operations give an ensure they materialise from the project's charter;
    // the reconciler admits only a materialised ensure's generations.
    let source = repository.key().to_string();
    let stamps = [
        (MATERIALIZED_PROJECT_ANNOTATION, FLEET_PROJECT),
        (SOURCE_REPOSITORY_ANNOTATION, source.as_str()),
        (SOURCE_COMMIT_ANNOTATION, "dummy-catalog"),
    ];
    let mut ensure = CatalogDocument::of::<ConvoyEnsure>(GOVERNOR_ROLE, &ensure)?;
    ensure.document["metadata"]["annotations"] =
        stamps.into_iter().map(|(key, value)| (key.to_string(), serde_json::Value::from(value))).collect();
    Ok(vec![CatalogDocument::of::<WorkflowTemplate>(GOVERNOR_WORKFLOW, &governor_workflow())?, ensure])
}

/// Write one JSON file per document under `dir`, named so they sort in apply
/// order, and return the paths in that order.
pub fn write_documents(dir: &Path, documents: &[CatalogDocument]) -> Result<Vec<PathBuf>, String> {
    std::fs::create_dir_all(dir).map_err(|error| format!("create {}: {error}", dir.display()))?;
    let mut paths = Vec::with_capacity(documents.len());
    for (position, document) in documents.iter().enumerate() {
        let path = dir.join(document.file_name(position));
        let text = serde_json::to_string_pretty(&document.document).map_err(|error| format!("encode {}: {error}", path.display()))?;
        std::fs::write(&path, text + "\n").map_err(|error| format!("write {}: {error}", path.display()))?;
        paths.push(path);
    }
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use flotilla_config::dummy_fleet::DEFAULT_DUMMY_FORGE_SERVICE_URL;

    use super::*;

    /// `document`'s spec decoded as a `T`, having checked the envelope.
    fn decoded<T: Resource>(document: &serde_json::Value) -> T::Spec {
        assert_eq!(document["apiVersion"], "flotilla.work/v1");
        assert_eq!(document["kind"], T::API_PATHS.kind);
        assert_eq!(document["metadata"]["namespace"], "flotilla");
        let spec: T::Spec = serde_json::from_value(document["spec"].clone()).expect("typed spec");
        assert_eq!(serde_json::to_value(&spec).expect("spec"), document["spec"], "{} loses nothing decoding", T::API_PATHS.kind);
        spec
    }

    // What the emitter writes is what an operator applies: every file decodes
    // as its kind's typed spec, in the order admission needs.
    #[test]
    fn written_documents_decode_as_their_typed_resources_in_apply_order() {
        let dir = tempfile::tempdir().expect("dir");
        let documents = documents(DEFAULT_DUMMY_FORGE_SERVICE_URL, "host-a", 7).expect("catalog");
        let paths = write_documents(dir.path(), &documents).expect("write");
        let mut sorted = paths.clone();
        sorted.sort();
        assert_eq!(paths, sorted, "file names sort in apply order");
        let read: Vec<serde_json::Value> =
            paths.iter().map(|path| serde_json::from_str(&std::fs::read_to_string(path).expect("read")).expect("json")).collect();
        let named: Vec<_> = read
            .iter()
            .map(|document| (document["kind"].as_str().expect("kind"), document["metadata"]["name"].as_str().expect("name")))
            .collect();
        let forge = forge(DEFAULT_DUMMY_FORGE_SERVICE_URL);
        let repository = repository(&forge).expect("repository");
        let key = repository.key().to_string();
        assert_eq!(
            named,
            [
                ("Project", "fleet"),
                ("FleetDesignation", "fleet"),
                ("Forge", "dummy-forge"),
                ("Repository", key.as_str()),
                ("FulfilmentKind", "dummy-crews"),
                ("WorkflowTemplate", "dummy-coder"),
                ("WorkflowTemplate", "dummy-coder-reviewer"),
                ("WorkflowTemplate", "dummy-long-runner"),
                ("WorkflowTemplate", "dummy-coder-supervisor"),
            ]
        );
        assert_eq!(decoded::<Project>(&read[0]), fleet_project(&repository));
        assert_eq!(decoded::<FleetDesignation>(&read[1]).project, "fleet");
        assert_eq!(decoded::<Forge>(&read[2]), forge);
        assert_eq!(decoded::<Repository>(&read[3]), repository);
        assert_eq!(decoded::<FulfilmentKind>(&read[4]), fulfilment_kind("host-a"));
        for (document, mix) in read[5..].iter().zip(CrewMix::ALL) {
            assert_eq!(decoded::<WorkflowTemplate>(document), mix.workflow(7));
        }
    }

    #[test]
    fn every_mix_names_only_dummy_crews_with_parseable_scripts() {
        for mix in CrewMix::ALL {
            let workflow = mix.workflow(4);
            assert!(workflow.vessels.is_empty(), "{mix:?}: admission allocates the vessels");
            let crew = workflow.roles.iter().collect::<Vec<_>>();
            assert!(!crew.is_empty(), "{mix:?}");
            for member in crew {
                let CrewSource::Agent { selector, prompt, .. } = &member.source else { panic!("{mix:?} crew must be agents") };
                assert_eq!(selector.adapter.as_deref(), Some(DUMMY_AGENT_ADAPTER));
                CrewScript::from_brief(prompt.as_deref().expect("prompt"), 1).expect("script parses");
            }
        }
    }

    // The standing supervisor must launch eagerly with no declared claim;
    // otherwise the load mix would only exercise latent-role availability. Its
    // needs differ from the coder's, so allocation gives it a vessel of its own,
    // where it is the first agent.
    #[test]
    fn supervisor_is_eager_and_claim_less() {
        let workflow = CrewMix::CoderSupervisor.workflow(1);
        let [governor, coder] = workflow.roles.as_slice() else { panic!("a governor and a coder") };
        assert_eq!(governor.role, "governor");
        assert_ne!(governor.needs, coder.needs);
        assert_eq!(governor.promises, vec![flotilla_resources::TemplatePromise::decision_ledger("governor")]);
        assert!(governor.completion_conditions.is_empty());
        let CrewSource::Agent { prompt: Some(prompt), .. } = &governor.source else { panic!("agent prompt") };
        assert!(matches!(CrewScript::from_brief(prompt, 1).unwrap(), CrewScript::Responder { .. }));
    }

    #[test]
    fn the_repository_resolves_onto_the_dummy_forge() {
        let forge = forge("https://forge.dummy.invalid/");
        assert_eq!(forge.hosts, ["forge.dummy.invalid".to_string()].into());
        let repository = repository(&forge).expect("repository");
        let identity = repository.forge().expect("forge identity");
        assert_eq!((identity.service_url.as_str(), identity.repository.as_str()), ("https://forge.dummy.invalid", DUMMY_REPOSITORY));
    }
}
