//! The importer of the forge observations: ChangeRequest, Issue and ForgeRead. Hosts' old stores
//! hold different copies of one subject, so each is written only at the home its source's
//! declarations name as observer, which every host's import reads the same.

use std::collections::BTreeMap;

use chrono::Utc;
use flotilla_engine_store::Import;
use flotilla_observation::forge_observation::Declarations;
use flotilla_protocol::IssueSource;
use flotilla_provisioning::engine::{
    forge_observation_import::{import_change_requests, import_forge_reads, import_issues, Demands, ObservationCopy},
    import::ImportReport,
};
use flotilla_resources::{ChangeRequest, Forge, ForgeRead, ForgeReadHeartbeat, Issue, Project, Repository, Resource};

use super::{Carried, Source};

pub(super) const KINDS: [&str; 3] = ["changerequests", "issues", "forgereads"];

/// The declarations every home's import reads alike: each Project at its earliest declaration, and
/// the forges and repositories the Projects resolve through.
fn declarations(source: &Source) -> Declarations<String> {
    let forges = source.context::<Forge>("forges", None).into_iter().map(|forge| forge.object.spec).collect();
    let mut projects = BTreeMap::new();
    for project in source.context::<Project>("projects", None) {
        let declaration = (project.object.metadata.creation_timestamp, project.home);
        match projects.get(&project.object.metadata.name) {
            Some((_, prior)) if *prior <= declaration => {}
            _ => {
                projects.insert(project.object.metadata.name.clone(), (project.object.spec, declaration));
            }
        }
    }
    let mut repositories = BTreeMap::new();
    for repository in source.context::<Repository>("repositories", None) {
        repositories.entry(repository.object.metadata.name.clone()).or_insert(Ok(repository.object));
    }
    Declarations { forges, projects: projects.into_values().collect(), repositories }
}

fn copies<R: Resource>(source: &Source, plural: &str, target: &Import, report: &mut ImportReport) -> Vec<ObservationCopy<R>> {
    source
        .owned::<R>(plural, None, R::API_PATHS.kind, target, report)
        .into_iter()
        .map(|copy| ObservationCopy { object: copy.object, home: copy.home })
        .collect()
}

pub(super) fn observations(source: &Source, target: &mut Import, _: &mut Carried, report: &mut ImportReport) {
    let declarations = declarations(source);
    let observer_of = |source: &IssueSource| declarations.preferred_observer(source).map(|(_, home)| home);
    let change_requests = copies::<ChangeRequest>(source, "changerequests", target, report);
    import_change_requests(&change_requests, &observer_of, target, report);
    let issues = copies::<Issue>(source, "issues", target, report);
    import_issues(&issues, &observer_of, target, report);
    let mut renewed = Demands::new();
    for heartbeat in source.context::<ForgeReadHeartbeat>("forgereadheartbeats", None) {
        let at = renewed.entry(heartbeat.object.metadata.name.clone()).or_insert(heartbeat.object.spec.demanded_at);
        *at = (*at).max(heartbeat.object.spec.demanded_at);
    }
    let reads = copies::<ForgeRead>(source, "forgereads", target, report);
    import_forge_reads(&reads, &renewed, &observer_of, Utc::now(), target, report);
}
