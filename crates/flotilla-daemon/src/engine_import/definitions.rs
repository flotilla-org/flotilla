//! The importer of the declared definition kinds and Project: every namespace's copies at once,
//! since the engine has no namespaces, and every home's, since a definition's record goes to the
//! home that declares it.

use flotilla_engine_store::Import;
use flotilla_provisioning::engine::{
    definition_import::{import_definitions, DefinitionCopies, DefinitionCopy},
    import::ImportReport,
};
use flotilla_resources::{
    CredentialGrant, CredentialSpec, CrewDefaults, FleetDesignation, Forge, ImageLayer, Project, Resource, WorkflowTemplate,
};

use super::{Carried, Source};

pub(super) const KINDS: [&str; 8] =
    ["forges", "fleetdesignations", "credentialspecs", "credentialgrants", "crewdefaults", "imagelayers", "workflowtemplates", "projects"];

fn copies<R: Resource>(source: &Source, plural: &str, target: &Import, report: &mut ImportReport) -> Vec<DefinitionCopy<R>> {
    source
        .owned::<R>(plural, None, R::API_PATHS.kind, target, report)
        .into_iter()
        .map(|copy| DefinitionCopy { namespace: copy.namespace, object: copy.object, home: copy.home })
        .collect()
}

pub(super) fn definitions(source: &Source, target: &mut Import, _: &mut Carried, report: &mut ImportReport) {
    let copies = DefinitionCopies {
        forges: copies::<Forge>(source, "forges", target, report),
        fleet_designations: copies::<FleetDesignation>(source, "fleetdesignations", target, report),
        credential_specs: copies::<CredentialSpec>(source, "credentialspecs", target, report),
        credential_grants: copies::<CredentialGrant>(source, "credentialgrants", target, report),
        crew_defaults: copies::<CrewDefaults>(source, "crewdefaults", target, report),
        image_layers: copies::<ImageLayer>(source, "imagelayers", target, report),
        workflow_templates: copies::<WorkflowTemplate>(source, "workflowtemplates", target, report),
        projects: copies::<Project>(source, "projects", target, report),
    };
    import_definitions(&copies, target, report);
}
