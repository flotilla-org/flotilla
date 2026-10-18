use std::sync::Arc;

use flotilla_config::ConfigStore;
use flotilla_protocol::HostName;

use crate::{in_process::InProcessDaemon, testkits::discovery::fake_discovery};

// Before the first kind migrates, probing the optional store never opens a database or
// consumes the installation slot. Explicit installation also rejects an empty registry.
#[tokio::test]
async fn absent_decided_store_has_no_lazy_open_side_effects() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"decided-store-test\"\n").expect("daemon config");
    let config = Arc::new(ConfigStore::with_base(temp.path()));
    let path = config.state_dir().as_path().join("decided.sqlite");
    let daemon = InProcessDaemon::new(
        Vec::new(),
        config,
        fake_discovery(false),
        HostName::new("local-host"),
        Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
    )
    .await;
    for _ in 0..2 {
        assert!(daemon.decided_store().is_err());
        assert!(!path.exists(), "request must not lazily open the store");
    }
    let empty = Arc::new(
        flotilla_schema_sqlite::stream::Service::open(&temp.path().join("empty.sqlite"), "local-host", Default::default())
            .expect("empty service"),
    );
    assert!(daemon.install_decided_store(empty).is_err());
    assert!(daemon.decided_store().is_err());
}
