//! The definitions a dummy fleet runs against: its forge, a placement policy
//! that selects the dummy environment provider, and one workflow per crew mix.
//! Load drivers and scenarios create these resources before dispatching.

use std::collections::BTreeMap;

use flotilla_resources::{
    CrewSource, CrewSpec, DockerCheckoutStrategy, DockerImageSource, DockerPerVesselPlacementPolicySpec, ExitDeclaration, ForgeKind,
    ForgeSpec, PlacementPolicySpec, RepositorySpec, Selector, VesselRequirement, WorkflowTemplateSpec,
};

use crate::{
    crew::{CrewScript, SCRIPT_LINE_PREFIX},
    DUMMY_AGENT_ADAPTER, DUMMY_ENVIRONMENT_INSTANCE, DUMMY_TERMINAL_POOL,
};

/// Forge definition name; subjects on the dummy forge carry it as their service.
pub const DUMMY_FORGE_ID: &str = "dummy-forge";
/// Repository every dummy crew works on, as `owner/name` on the dummy forge.
pub const DUMMY_REPOSITORY: &str = "dummy/fleet";
/// PlacementPolicy name selecting dummy environments.
pub const DUMMY_PLACEMENT_POLICY: &str = "dummy-crews";
/// Image named by dummy environments; nothing pulls it.
pub const DUMMY_IMAGE: &str = "dummy/crew:latest";

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

    pub fn workflow(self, long_runner_turns: u32) -> WorkflowTemplateSpec {
        let repository = DUMMY_REPOSITORY.to_string();
        let (vessel, crew) = match self {
            Self::Coder => ("implement", vec![crew("coder", CrewScript::Coder { reviewer: None, repository })]),
            Self::CoderReviewer => {
                // A review request retains the coder's PR responsibility.
                // Same-vessel handoff transfers promises; use a separate review
                // vessel for the peer's own ledger and review responsibility.
                return WorkflowTemplateSpec::builder()
                    .exit(ExitDeclaration::standard_table())
                    .vessels(vec![
                        VesselRequirement::builder()
                            .name("implement".into())
                            .crew(vec![crew("coder", CrewScript::Coder { reviewer: Some("review:reviewer".into()), repository })])
                            .build(),
                        VesselRequirement::builder()
                            .name("review".into())
                            .crew(vec![crew("reviewer", CrewScript::Responder { reply_to: "implement:coder".into() })])
                            .build(),
                    ])
                    .build();
            }
            Self::CoderSupervisor => {
                return WorkflowTemplateSpec::builder()
                    .exit(ExitDeclaration::standard_table())
                    .vessels(vec![
                        VesselRequirement::builder()
                            .name("supervise".into())
                            .crew(vec![crew("governor", CrewScript::Responder { reply_to: "implement:coder".into() })])
                            .build(),
                        VesselRequirement::builder()
                            .name("implement".into())
                            .crew(vec![crew("coder", CrewScript::Coder { reviewer: Some("supervise:governor".into()), repository })])
                            .build(),
                    ])
                    .build();
            }
            Self::LongRunner => {
                return WorkflowTemplateSpec::builder()
                    .exit(ExitDeclaration::standard_table())
                    .vessels(vec![
                        VesselRequirement::builder()
                            .name("govern".into())
                            .crew(vec![crew("governor", CrewScript::Governor { aide: "assist:aide".into(), turns: long_runner_turns })])
                            .build(),
                        VesselRequirement::builder()
                            .name("assist".into())
                            .crew(vec![crew("aide", CrewScript::Responder { reply_to: "govern:governor".into() })])
                            .build(),
                    ])
                    .build();
            }
        };
        WorkflowTemplateSpec::builder()
            .exit(ExitDeclaration::standard_table())
            .vessels(vec![VesselRequirement::builder().name(vessel.to_string()).crew(crew).build()])
            .build()
    }
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

/// The repository every dummy crew works on.
pub fn repository(forge: &ForgeSpec) -> Result<RepositorySpec, String> {
    RepositorySpec::remote(repository_url(forge))?.on_forge(forge)
}

pub fn repository_url(forge: &ForgeSpec) -> String {
    format!("{}/{DUMMY_REPOSITORY}.git", forge.https_url.trim_end_matches('/'))
}

/// Labels for the dummy placement policy: they select the dummy provider instance.
pub fn placement_policy_labels() -> BTreeMap<String, String> {
    BTreeMap::from([(
        flotilla_environment::environment::ENVIRONMENT_PROVIDER_INSTANCE_LABEL.to_string(),
        DUMMY_ENVIRONMENT_INSTANCE.to_string(),
    )])
}

/// Per-vessel dummy environments on `host_ref`, crews in the dummy pool.
pub fn placement_policy(host_ref: &str) -> PlacementPolicySpec {
    PlacementPolicySpec::builder()
        .pool(DUMMY_TERMINAL_POOL.to_string())
        .docker_per_vessel(DockerPerVesselPlacementPolicySpec {
            host_ref: host_ref.into(),
            image: Some(DockerImageSource::Literal(DUMMY_IMAGE.into())),
            legacy_image_baseline_ref: None,
            pull_policy: Default::default(),
            memory_policy: Default::default(),
            agent_adapters: [DUMMY_AGENT_ADAPTER.to_string()].into(),
            default_cwd: None,
            env: BTreeMap::new(),
            checkout: DockerCheckoutStrategy::FreshCloneInContainer { clone_path: "/workspace".into() },
        })
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_mix_names_only_dummy_crews_with_parseable_scripts() {
        for mix in CrewMix::ALL {
            let workflow = mix.workflow(4);
            let crew = workflow.vessels.iter().flat_map(|vessel| &vessel.crew).collect::<Vec<_>>();
            assert!(!crew.is_empty(), "{mix:?}");
            for member in crew {
                let CrewSource::Agent { selector, prompt, .. } = &member.source else { panic!("{mix:?} crew must be agents") };
                assert_eq!(selector.adapter.as_deref(), Some(DUMMY_AGENT_ADAPTER));
                CrewScript::from_brief(prompt.as_deref().expect("prompt"), 1).expect("script parses");
            }
        }
    }

    // The standing supervisor must launch eagerly with no declared claim;
    // otherwise the load mix would only exercise latent-role availability.
    #[test]
    fn supervisor_is_eager_and_claim_less() {
        let workflow = CrewMix::CoderSupervisor.workflow(1);
        let vessel = workflow.vessels.iter().find(|vessel| vessel.name == "supervise").expect("supervisor vessel");
        assert!(vessel.starts_eagerly(0));
        assert_eq!(vessel.crew[0].role, "governor");
        assert_eq!(vessel.crew[0].promises, vec![flotilla_resources::TemplatePromise::decision_ledger("governor")]);
        assert!(vessel.crew[0].completion_conditions.is_empty());
        let CrewSource::Agent { prompt: Some(prompt), .. } = &vessel.crew[0].source else { panic!("agent prompt") };
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
