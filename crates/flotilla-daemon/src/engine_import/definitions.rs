//! The importer of the declared definition kinds and Project: every namespace's copies at once,
//! since the engine has no namespaces, and every home's, since a definition's record goes to the
//! home that declares it.

use flotilla_engine_store::{Import, ImportTarget};
use flotilla_provisioning::engine::{
    definition_import::{import_definitions, DefinitionCopies, DefinitionCopy},
    import::ImportReport,
};
use flotilla_resources::{
    ConvoyEnsure, CredentialGrant, CredentialSpec, CrewDefaults, FleetDesignation, Forge, ImageLayer, Project, Resource, WorkflowTemplate,
};

use super::{Carried, Source};

pub(super) const KINDS: [&str; 9] = [
    "forges",
    "fleetdesignations",
    "credentialspecs",
    "credentialgrants",
    "crewdefaults",
    "imagelayers",
    "workflowtemplates",
    "convoyensures",
    "projects",
];

fn copies<R: Resource>(source: &Source, plural: &str, target: &Import, report: &mut ImportReport) -> Vec<DefinitionCopy<R>> {
    source
        .owned::<R>(plural, None, R::API_PATHS.kind, target, report)
        .into_iter()
        .map(|copy| DefinitionCopy { namespace: copy.namespace, object: copy.object, home: copy.home })
        .collect()
}

pub(super) fn definitions(source: &Source, target: &mut Import, _: &mut Carried, report: &mut ImportReport) {
    let convoy_ensures = copies::<ConvoyEnsure>(source, "convoyensures", target, report);
    // A standing role's status is derived from its generations and its hold attention on the
    // engine (#3108), so none is carried.
    let statuses = convoy_ensures.iter().filter(|copy| target.imports(&copy.home) && copy.object.status.is_some()).count();
    if statuses > 0 {
        report.notes.push(format!("{statuses} ConvoyEnsure statuses not carried: derived from each role's convoy generations"));
    }
    let copies = DefinitionCopies {
        forges: copies::<Forge>(source, "forges", target, report),
        fleet_designations: copies::<FleetDesignation>(source, "fleetdesignations", target, report),
        credential_specs: copies::<CredentialSpec>(source, "credentialspecs", target, report),
        credential_grants: copies::<CredentialGrant>(source, "credentialgrants", target, report),
        crew_defaults: copies::<CrewDefaults>(source, "crewdefaults", target, report),
        image_layers: copies::<ImageLayer>(source, "imagelayers", target, report),
        workflow_templates: copies::<WorkflowTemplate>(source, "workflowtemplates", target, report),
        convoy_ensures,
        projects: copies::<Project>(source, "projects", target, report),
    };
    import_definitions(&copies, target, report);
}
