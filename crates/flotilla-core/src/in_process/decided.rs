//! The decided store (ADR 0064, #3108): the engine store every daemon composes with. It holds
//! the engine kinds under their production codes and is opened, once the daemon's identity is
//! resolved, before anything reads a resource.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
};

use flotilla_provisioning::engine::decided::{open_ports, DecidedPorts};

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

/// The decided store's opener, for [`super::InProcessDaemon::new_composed`].
pub fn opener(state_dir: impl Into<PathBuf>) -> EngineOpener {
    let state_dir = state_dir.into();
    Box::new(move |identity: EngineIdentity| {
        Box::pin(async move {
            tokio::task::spawn_blocking(flotilla_schema::reads::carry(move || open(&state_dir, identity)))
                .await
                .map_err(|error| error.to_string())?
        }) as futures::future::BoxFuture<'static, _>
    })
}

/// What an operator does about a store this build refuses for its shape. The store holds this
/// host's writes since the roll, so deleting it throws them away: the remedy is a build that opens it.
fn remedy(error: &flotilla_schema_sqlite::StoreError) -> String {
    use flotilla_schema_sqlite::StoreError::*;
    if !matches!(
        error,
        LegacyDictionarySchema
            | LegacyLogSchema
            | KindTableDrift { .. }
            | KindCodeDrift { .. }
            | KindCodeRemoved { .. }
            | KindCodeUnreadable { .. }
    ) {
        return String::new();
    }
    "; do not delete it: deleting it throws away every write since the roll; keep it, back it up, and install a generation that opens it".into()
}

/// Opens the decided store at `state_dir`, homed at this daemon's node id, and the ports serving
/// its kinds, so nothing the daemon composes reads them anywhere else.
pub fn open(state_dir: &Path, identity: EngineIdentity) -> Result<ComposedEngine, String> {
    let home = identity.node_id.as_str().to_owned();
    let own_host = identity.host_id.to_string();
    let registry = registry().map_err(|error| format!("decided store registry: {error:?}"))?;
    std::fs::create_dir_all(state_dir).map_err(|error| format!("create state directory {}: {error}", state_dir.display()))?;
    let path = state_dir.join(ENGINE_STORE);
    if path.exists() {
        // ADR 0047 N→N+1: the previous generation's retired tables go before the store opens.
        rusqlite::Connection::open(&path)
            .and_then(|connection| flotilla_engine_store::drop_retired_tables(&connection))
            .map_err(|error| format!("drop retired tables from decided store {}: {error}", path.display()))?;
    }
    let service = Arc::new(
        flotilla_schema_sqlite::stream::Service::open(&path, &home, registry)
            .map_err(|error| format!("open decided store {}: {error}{}", path.display(), remedy(&error)))?,
    );
    flotilla_engine_convoys::convoys::declare_carried(&service);
    let DecidedPorts {
        records,
        observations,
        observation_failures,
        change_request_branches,
        source_observers,
        accounts,
        manifest_roots,
        standing_roles,
    } = open_ports(&service, &own_host, identity.clock)?;
    Ok(ComposedEngine {
        service,
        records,
        observations,
        observation_failures,
        change_request_branches,
        source_observers,
        accounts,
        manifest_roots,
        standing_roles,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Behaviour (cut-over B6): a decided store this build refuses for its shape names the file and
    // never suggests deleting it; it says deleting it throws away every write since the roll.
    #[test]
    fn a_refused_store_is_kept() {
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
        assert!(error.contains("do not delete it"), "{error}");
        assert!(error.contains("throws away every write since the roll"), "{error}");
    }

    fn identity(node: &str) -> EngineIdentity {
        EngineIdentity {
            node_id: flotilla_protocol::NodeId::new(node),
            host_id: flotilla_protocol::qualified_path::HostId::new(node),
            clock: Arc::new(flotilla_resources::SystemClock),
        }
    }

    // Behaviour (#3108): the decided store declares what a Convoy snapshot carries as it opens, so
    // a home opened afresh, with no convoy path installed over it, snapshots a retired launch with
    // its end entry, and its replica reads how the launch ended.
    #[tokio::test]
    async fn a_freshly_opened_home_snapshots_a_retired_launch_with_its_end() {
        use flotilla_engine_convoys::{convoy_records::EngineConvoyRecords, convoys::Retirement};
        use flotilla_resources::{convoy_records::ConvoyRecords, ConvoyPhase, ConvoySpec, ConvoyStatus};
        let (dir_b, dir_a) = (tempfile::tempdir().expect("tempdir"), tempfile::tempdir().expect("tempdir"));
        let launch = {
            let composed = open(dir_b.path(), identity("home-b")).expect("open");
            let backend = flotilla_store_memory::engine_home_backend();
            let reads = flotilla_provisioning_testkit::EngineFixture::over_service(&composed.service).reads();
            let clock = Arc::new(flotilla_resources::SystemClock);
            let home = flotilla_engine_convoys_testkit::EngineConvoysHome::over(&composed.service, "flotilla", &backend, clock, reads);
            let status = ConvoyStatus { phase: ConvoyPhase::Failed, ..flotilla_engine_convoys_testkit::single_agent("coder") };
            let spec = ConvoySpec::builder().project_ref("flotilla".into()).workflow_ref("review".into()).build();
            let launch = home.launch(flotilla_engine_convoys_testkit::convoy("flotilla", "fix-it", spec, status)).await;
            assert_eq!(home.convoys().retire(&launch, flotilla_schema::Timestamp(9_000_000)), Ok(Retirement::Retired));
            launch
        };

        // Opened afresh, as a restart opens it: nothing is composed over it.
        let home_b = open(dir_b.path(), identity("home-b")).expect("reopen").service;
        let home_a = open(dir_a.path(), identity("home-a")).expect("open").service;
        let mut frames = Vec::new();
        home_b
            .serve_home("home-b", None, flotilla_schema::Timestamp(10_000_000), |frame| {
                frames.push(frame.to_vec());
                Ok(())
            })
            .expect("serve");
        let received = home_a
            .with_store(|store| {
                flotilla_schema_sqlite::stream::Follower::new(store)
                    .receive("home-b", |sink| frames.iter().try_for_each(|frame| sink(frame)))
            })
            .expect("follow");
        assert!(received.snapshot);
        let stalls = flotilla_engine_messages::awaiting::OpenAwaiting::open(&home_a).expect("open records");
        let records = EngineConvoyRecords::new("home-a", Arc::clone(&home_a), Arc::default(), stalls);
        let record = records.get(&launch, Some("home-b")).expect("read").expect("the replica");
        assert!(record.retired);
        assert!(record.ended.is_some(), "the replica reads how it ended: {record:?}");
    }
}
