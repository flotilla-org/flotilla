//! The decided store (ADR 0064, #3108): the engine store every daemon composes with. It holds
//! the engine kinds under their production codes and is opened, once the daemon's identity is
//! resolved, before anything reads a resource.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
};

use flotilla_provisioning::engine::{
    access_deliveries::EngineAccessDeliveries, account::EngineAccounts, artifacts::EngineArtifacts, definitions::EngineDefinitions,
    hosts::EngineHosts, kinds::EngineKinds, manifest_root::EngineManifestRoots, observations::EngineObservations, projects::EngineProjects,
    repositories::EngineRepositories,
};
use flotilla_resources::engine_records::EngineRecords;

use super::{ComposedEngine, EngineIdentity, EngineOpener};

/// The decided store's file in the daemon's state directory.
pub const ENGINE_STORE: &str = "decided.sqlite";

/// The decided store's kinds ([`flotilla_provisioning::stored::decided_registry`]).
pub fn registry() -> Result<flotilla_schema::KindRegistry, flotilla_schema::RegistryError> {
    flotilla_provisioning::stored::decided_registry()
}

/// The machine diagram of each kind in [`registry`], by kind name.
pub fn diagrams() -> Result<BTreeMap<String, String>, flotilla_schema::RegistryError> {
    flotilla_provisioning::stored::decided_diagrams()
}

/// Runs in the state directory before the store opens, given the daemon's home and host: the
/// daemon's first-start import of its old store.
pub type BeforeOpen = Box<dyn FnOnce(&Path, &str, &str) -> Result<(), String> + Send>;

/// The opener of the decided store at `state_dir`, for [`super::InProcessDaemon::new_composed`].
pub fn opener(state_dir: impl Into<PathBuf>) -> EngineOpener {
    opener_with(state_dir, Box::new(|_, _, _| Ok(())))
}

/// [`opener`], running `before` first.
pub fn opener_with(state_dir: impl Into<PathBuf>, before: BeforeOpen) -> EngineOpener {
    let state_dir = state_dir.into();
    Box::new(move |identity: EngineIdentity| {
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                before(&state_dir, identity.node_id.as_str(), &identity.host_id.to_string())?;
                open(&state_dir, identity)
            })
            .await
            .map_err(|error| error.to_string())?
        }) as futures::future::BoxFuture<'static, _>
    })
}

/// What an operator does about a store this build refuses for its shape: the old store is the
/// source the decided store is imported from at first start, so a fresh import replaces it.
fn remedy(error: &flotilla_schema_sqlite::StoreError) -> &'static str {
    use flotilla_schema_sqlite::StoreError::*;
    match error {
        LegacyDictionarySchema
        | LegacyLogSchema
        | KindTableDrift { .. }
        | KindCodeDrift { .. }
        | KindCodeRemoved { .. }
        | KindCodeUnreadable { .. } => "; delete it and restart to re-import from the old store",
        _ => "",
    }
}

/// Opens the decided store at `state_dir`, homed at this daemon's node id, and the ports serving
/// its kinds, so nothing the daemon composes reads them anywhere else.
pub fn open(state_dir: &Path, identity: EngineIdentity) -> Result<ComposedEngine, String> {
    let home = identity.node_id.as_str().to_owned();
    let own_host = identity.host_id.to_string();
    let registry = registry().map_err(|error| format!("decided store registry: {error:?}"))?;
    std::fs::create_dir_all(state_dir).map_err(|error| format!("create state directory {}: {error}", state_dir.display()))?;
    let path = state_dir.join(ENGINE_STORE);
    let service = Arc::new(
        flotilla_schema_sqlite::stream::Service::open(&path, &home, registry)
            .map_err(|error| format!("open decided store {}: {error}{}", path.display(), remedy(&error)))?,
    );
    let clock = || Arc::clone(&identity.clock);
    // #3824 layers 1 and 2c: the account kinds and ManifestRoot, which the generic resource verbs
    // reach through their records as well.
    let accounts = Arc::new(EngineAccounts::open(Arc::clone(&service), clock())?);
    let manifest_roots = Arc::new(EngineManifestRoots::open(Arc::clone(&service), clock())?);
    let records: Vec<Arc<dyn EngineRecords>> = vec![
        Arc::clone(&accounts) as Arc<dyn EngineRecords>,
        Arc::clone(&manifest_roots) as Arc<dyn EngineRecords>,
        // #3824 layer 2b-ii: the definition kinds.
        Arc::new(EngineDefinitions::open(Arc::clone(&service), clock())?),
        // #3824 layer 3a and 3b: Repository, and Project with its operations' record.
        Arc::new(EngineRepositories::open(Arc::clone(&service), clock())?),
        Arc::new(EngineProjects::open(Arc::clone(&service), clock())?),
        Arc::new(EngineArtifacts::open(Arc::clone(&service), clock())?),
        Arc::new(EngineAccessDeliveries::open(Arc::clone(&service), clock())?),
        // FulfilmentKind, which admission and the vessel reconciler read and admission's snapshots write.
        Arc::new(EngineKinds::open(Arc::clone(&service), clock())?),
        // Host, which every placement reads and this daemon's observers write only on change.
        Arc::new(EngineHosts::open(Arc::clone(&service), own_host, clock())?),
    ];
    // ChangeRequest and Issue: written by their source's observer, read at every home.
    let source_observers = Arc::new(flotilla_resources::engine_records::LateSourceObservers::default());
    let observations = Arc::new(EngineObservations::open(Arc::clone(&service), clock())?.observed_by(source_observers.clone()));
    Ok(ComposedEngine {
        service,
        records,
        observations: observations.clone(),
        observation_failures: observations.clone(),
        change_request_branches: observations,
        source_observers,
        accounts,
        manifest_roots,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Behaviour: a decided store this build refuses for its shape names the file and tells the
    // operator to delete it and restart, which re-imports it from the old store.
    #[test]
    fn a_refused_store_names_its_remedy() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(ENGINE_STORE);
        let conn = rusqlite::Connection::open(&path).expect("connection");
        conn.execute_batch("CREATE TABLE strings (id INTEGER PRIMARY KEY, s TEXT NOT NULL UNIQUE) STRICT;").expect("old dictionary");
        drop(conn);
        let identity = EngineIdentity {
            node_id: flotilla_protocol::NodeId::new("node"),
            host_id: flotilla_protocol::qualified_path::HostId::new("host"),
            clock: Arc::new(flotilla_resources::SystemClock),
        };
        let Err(error) = open(dir.path(), identity) else { panic!("an old dictionary is refused") };
        assert!(error.contains(&path.display().to_string()), "{error}");
        assert!(error.ends_with("; delete it and restart to re-import from the old store"), "{error}");
    }
}
