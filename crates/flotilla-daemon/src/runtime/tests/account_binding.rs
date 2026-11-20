use super::*;
use crate::testkits::engine::{admit_convoy, engine};
use flotilla_resources::{CredentialGrant, CredentialGrantSelector, CredentialGrantSpec};
use flotilla_store::controller::WorkQueueSender;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio_util::task::AbortOnDropHandle;

/// A daemon whose convoy launches are its engine's, with `credential-0` bound to `claude-max`.
async fn binding_daemon(temp: &Path) -> (Arc<InProcessDaemon>, String) {
    fs::write(temp.join("daemon.toml"), "machine_id = \"account-binding-test\"\n").expect("daemon config");
    let daemon = in_memory_daemon(Vec::new(), Arc::new(ConfigStore::with_base(temp))).await;
    designate_fleet(&daemon).await;
    engine(&daemon);
    let bound = admit(&daemon, "credential-0").await;
    bind_account(&daemon, &bound, "coder", "claude-max", "a@example.com").await;
    (daemon, bound)
}

async fn admit(daemon: &InProcessDaemon, name: &str) -> String {
    let spec = ConvoySpec::builder().project_ref(NAMESPACE.into()).workflow_ref("test".to_string()).build();
    admit_convoy(
        daemon,
        flotilla_engine_convoys_testkit::convoy(NAMESPACE, name, spec, flotilla_engine_convoys_testkit::single_agent("coder")),
    )
    .await
}

async fn grant(daemon: &InProcessDaemon, name: &str, credential: &str) {
    let spec = CredentialGrantSpec::builder()
        .selector(CredentialGrantSelector::builder().build())
        .credentials(BTreeSet::from([credential.to_string()]))
        .build();
    let grants = daemon.resource_backend().definitions::<CredentialGrant>(NAMESPACE);
    grants.create(&InputMeta::builder().name(name.into()).build(), &spec).await.expect("grant");
}

/// Grants `credential` under fresh names until a convoy is queued, and returns it.
async fn queued_by_grants(daemon: &InProcessDaemon, queued: &mut UnboundedReceiver<String>, prefix: &str, credential: &str) -> String {
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut n = 0;
        loop {
            grant(daemon, &format!("{prefix}-{n}"), credential).await;
            if let Ok(Some(name)) = tokio::time::timeout(Duration::from_millis(100), queued.recv()).await {
                return name;
            }
            n += 1;
        }
    })
    .await
    .expect("a grant queues a convoy")
}

/// Ends convoy launch `convoy` at `daemon`'s home, as an operator's abandon ends it.
async fn end_convoy(daemon: &InProcessDaemon, convoy: &str) {
    let view = daemon.resource_backend().using::<Convoy>(NAMESPACE).get(convoy).await.expect("convoy");
    let engine = daemon.convoy_engine_path();
    engine.abandon(&view, "operator", "ended").await.expect("convoy ended");
}

// Behaviour: a grant reaches the convoys whose bindings use its credential and the open convoys
// here still waiting for a binding, as they are when it comes: a convoy admitted since the
// watch's last grant is reached, and one ended since is not.
#[tokio::test]
async fn a_grant_reaches_the_open_unbound_convoys_as_they_are_when_it_comes() {
    let temp = tempfile::tempdir().expect("tempdir");
    let (daemon, bound) = binding_daemon(temp.path()).await;
    let (watches, _reconciler) = super::super::account_access::binding_controller(Arc::clone(&daemon), NAMESPACE.to_string());
    let (sender, mut queued) = WorkQueueSender::channel();
    let _watching: Vec<_> = watches
        .into_iter()
        .map(|watch| AbortOnDropHandle::new(tokio::spawn(watch.spawn(daemon.resource_backend(), NAMESPACE.to_string(), sender.clone()))))
        .collect();

    // The bound convoy's credential reaches it, and no other convoy is open.
    assert_eq!(queued_by_grants(&daemon, &mut queued, "warm", "claude-max").await, bound);

    let waiting = admit(&daemon, "credential-1").await;
    let ended = admit(&daemon, "credential-2").await;
    end_convoy(&daemon, &ended).await;
    // A queued name is not sent again until the controller takes it, which this test never does,
    // so each name is heard once at most.
    let heard = queued_by_grants(&daemon, &mut queued, "unused", "unused-credential").await;
    assert_eq!(heard, waiting, "the convoy admitted since is waiting for a binding");
    tokio::time::sleep(Duration::from_millis(300)).await;
    grant(&daemon, "unused-last", "unused-credential").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(queued.try_recv().is_err(), "the ended convoy is not reached");
}
