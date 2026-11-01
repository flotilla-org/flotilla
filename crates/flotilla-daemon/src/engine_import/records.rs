//! The importers of the kinds whose records map one to one: ManifestRoot, Repository, Host,
//! Artifact and the account kinds.

use flotilla_engine_store::Import;
use flotilla_provisioning::engine::{
    account::import_accounts,
    artifact_import::{import_artifacts, ArtifactCopy},
    host_import::{import_hosts, HostCopies, HostCopy},
    import::ImportReport,
    manifest_root_import::import_roots,
    repository_import::{import_repositories, RepositoryCopy},
};
use flotilla_resources::{AccountBinding, AccountProvision, Artifact, Host, ManifestRoot, Repository};

use super::{Carried, Source};

pub(super) const ACCOUNT_KINDS: [&str; 2] = ["accountbindings", "accountprovisions"];

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

/// Every namespace's at once: the engine has no namespaces.
pub(super) fn artifacts(source: &Source, target: &mut Import, _: &mut Carried, report: &mut ImportReport) {
    let copies: Vec<_> = source
        .owned::<Artifact>("artifacts", None, "Artifact", target, report)
        .into_iter()
        .map(|copy| ArtifactCopy { namespace: copy.namespace, object: copy.object, home: copy.home })
        .collect();
    import_artifacts(&copies, target, report);
}

pub(super) fn accounts(source: &Source, target: &mut Import, _: &mut Carried, report: &mut ImportReport) {
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
        import_accounts(&bindings, &provisions, target, report);
    }
}
