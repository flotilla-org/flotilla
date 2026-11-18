//! The importers of the kinds whose records map one to one: ManifestRoot, Repository, Host,
//! Artifact, AccessDelivery, FulfilmentKind (with each PlacementPolicy as a kind of its name) and the
//! account kinds.

use std::collections::BTreeMap;

use flotilla_engine_store::Import;
use flotilla_provisioning::engine::{
    access_delivery_import::{import_access_deliveries, AccessDeliveryCopy},
    account::import_accounts,
    artifact_import::{import_artifacts, ArtifactCopy},
    host_import::{import_hosts, HostCopies, HostCopy},
    import::ImportReport,
    manifest_root_import::import_roots,
    placement_import::{retired::RetiredPolicy, write_placement, PlacementCopies, PlacementCopy},
    repository_import::{import_repositories, RepositoryCopy},
};
use flotilla_resources::{
    AccessDelivery, AccountBinding, AccountProvision, Artifact, Convoy, FulfilmentKind, Host, ManifestRoot, Repository, VesselPlacementPin,
    PLACEMENT_SNAPSHOT_ANNOTATION, VESSEL_PLACEMENTS_ANNOTATION,
};

use super::{Carried, Decoded, Source};

pub(super) const ACCOUNT_KINDS: [&str; 2] = ["accountbindings", "accountprovisions"];
pub(super) const PLACEMENT_KINDS: [&str; 2] = ["fulfilmentkinds", "placementpolicies"];

pub(super) fn manifest_roots(source: &Source, target: &mut Import, _: &mut Carried, report: &mut ImportReport) {
    for namespace in source.namespaces(&["manifestroots"]) {
        let roots: Vec<_> = source
            .owned::<ManifestRoot>("manifestroots", Some(namespace), "ManifestRoot", target, report)
            .into_iter()
            .map(|root| (root.object, root.home))
            .collect();
        import_roots(&roots, target, report);
    }
}

/// Every namespace's at once: the engine has no namespaces.
pub(super) fn repositories(source: &Source, target: &mut Import, _: &mut Carried, report: &mut ImportReport) {
    let copies: Vec<_> = source
        .owned::<Repository>("repositories", None, "Repository", target, report)
        .into_iter()
        .map(|copy| RepositoryCopy { namespace: copy.namespace, object: copy.object, home: copy.home })
        .collect();
    import_repositories(&copies, target, report);
}

/// Every namespace's at once: the engine has no namespaces. Each home imported here is this
/// daemon's, whose own host the source names.
pub(super) fn hosts(source: &Source, target: &mut Import, _: &mut Carried, report: &mut ImportReport) {
    let copies: Vec<_> = source
        .owned::<Host>("hosts", None, "Host", target, report)
        .into_iter()
        .map(|copy| HostCopy { namespace: copy.namespace, object: copy.object, home: copy.home })
        .collect();
    let homes = match &source.own_host {
        Some(own_host) => copies.iter().map(|copy| (copy.home.clone(), own_host.clone())).collect(),
        None => Default::default(),
    };
    import_hosts(&HostCopies { copies, homes }, target, report);
}

/// Every namespace's at once: the engine has no namespaces. Every home's copies are read, as a
/// snapshot takes its platform from its host's kind wherever that is homed.
pub(super) fn placement(source: &Source, target: &mut Import, carried: &mut Carried, report: &mut ImportReport) {
    let kinds = source
        .owned::<FulfilmentKind>("fulfilmentkinds", None, "FulfilmentKind", target, report)
        .into_iter()
        .map(|copy| PlacementCopy { namespace: copy.namespace, object: copy.object, home: copy.home })
        .collect();
    let policies = source
        .owned_as("placementpolicies", None, "PlacementPolicy", target, report, RetiredPolicy::decode)
        .into_iter()
        .map(|(namespace, object, home)| PlacementCopy { namespace, object, home })
        .collect();
    let live_references = live_placement_references(source, carried);
    write_placement(&PlacementCopies { kinds, policies, live_references }, target, report);
}

/// The convoys, every home's, that the convoy import carries as live: bootstrapped and not ended.
/// Only these release their prepared snapshots on the engine.
pub(super) fn live_convoys(source: &Source) -> Vec<Decoded<Convoy>> {
    let mut convoys = source.context::<Convoy>("convoys", None);
    convoys.retain(|convoy| {
        convoy.object.status.as_ref().is_some_and(|status| status.workflow_snapshot.is_some() && !status.phase.is_terminal())
    });
    convoys
}

/// The placement snapshots each live convoy references, by home and snapshot, with each such
/// convoy's old name and the launch it imported as. A convoy the import did not write releases
/// nothing on the engine.
fn live_placement_references(source: &Source, carried: &Carried) -> BTreeMap<(String, String), BTreeMap<String, String>> {
    let mut references = BTreeMap::<(String, String), BTreeMap<String, String>>::new();
    for convoy in live_convoys(source) {
        let object = &convoy.object;
        let Some(launch) = carried.launch(&convoy.home, &object.metadata.name) else { continue };
        let annotations = &object.metadata.annotations;
        let pins = annotations
            .get(VESSEL_PLACEMENTS_ANNOTATION)
            .and_then(|pins| serde_json::from_str::<BTreeMap<String, VesselPlacementPin>>(pins).ok())
            .unwrap_or_default();
        let named = annotations
            .get(PLACEMENT_SNAPSHOT_ANNOTATION)
            .cloned()
            .into_iter()
            .chain(object.spec.placement_policy.clone())
            .chain(pins.into_values().map(|pin| pin.kind_ref));
        for snapshot in named {
            references.entry((convoy.home.clone(), snapshot)).or_default().insert(object.metadata.name.clone(), launch.clone());
        }
    }
    references
}

/// Every namespace's at once: the engine has no namespaces.
pub(super) fn artifacts(source: &Source, target: &mut Import, carried: &mut Carried, report: &mut ImportReport) {
    let copies: Vec<_> = source
        .owned::<Artifact>("artifacts", None, "Artifact", target, report)
        .into_iter()
        .map(|copy| ArtifactCopy { namespace: copy.namespace, object: copy.object, home: copy.home })
        .collect();
    import_artifacts(&copies, &carried.convoy_launches, target, report);
}

pub(super) fn accounts(source: &Source, target: &mut Import, carried: &mut Carried, report: &mut ImportReport) {
    for namespace in source.namespaces(&ACCOUNT_KINDS) {
        let bindings: Vec<_> = source
            .owned::<AccountBinding>("accountbindings", Some(namespace), "AccountBinding", target, report)
            .into_iter()
            .map(|binding| (binding.object, binding.home))
            .collect();
        let provisions: Vec<_> = source
            .owned::<AccountProvision>("accountprovisions", Some(namespace), "AccountProvision", target, report)
            .into_iter()
            .map(|provision| (provision.object, provision.home))
            .collect();
        import_accounts(&bindings, &provisions, &carried.convoy_launches, target, report);
    }
}

/// Every namespace's at once: the engine has no namespaces.
/// Each delivery under the launch its convoy imported as at the delivery's home.
pub(super) fn access_deliveries(source: &Source, target: &mut Import, carried: &mut Carried, report: &mut ImportReport) {
    let copies: Vec<_> = source
        .owned::<AccessDelivery>("accessdeliveries", None, "AccessDelivery", target, report)
        .into_iter()
        .map(|copy| AccessDeliveryCopy { namespace: copy.namespace, object: copy.object, home: copy.home })
        .collect();
    import_access_deliveries(&copies, |home: &str, convoy: &str| carried.launch(home, convoy), target, report);
}
