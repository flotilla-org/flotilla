//! The account records contract (#3824 layer 1): one behaviour, run against the old store's
//! records and the engine's, so either can stand behind admission, the launch path, explain and
//! the account controllers.

use std::{collections::BTreeMap, sync::Arc, time::Duration};

use flotilla_provisioning::engine::account::EngineAccounts;
use flotilla_resources::{
    account_access::{
        binding_name, requester_address, AccessName, AccountBindingSpec, AccountChange, AccountProvisionSpec, AccountRecords, BindingState,
        CredentialName, Delivered, EpochEntry, GrantName, GrantScope, PendingBinding, ProvisionPlan, ProvisionWrite, Selection, UpdateMode,
    },
    Clock, SystemClock,
};
use flotilla_schema::{Records, Ref, State, Timestamp};
use flotilla_schema_sqlite::stream::Service;
use flotilla_store::{OldStoreAccounts, ResourceBackend};
use flotilla_store_memory::InMemoryBackend;
use futures::StreamExt;

const NS: &str = "flotilla";

fn access() -> AccessName {
    AccessName("claude-code:account".into())
}

fn at(micros: i64) -> Timestamp {
    Timestamp::from_micros(micros)
}

fn grants(names: &[&str]) -> BTreeMap<GrantName, GrantScope> {
    names.iter().map(|name| (GrantName((*name).into()), GrantScope { scope: "account".into() })).collect()
}

/// A requester's binding of `credential`, opened at `opened`.
fn pending(convoy: &str, role: &str, credential: &str, opened: i64) -> PendingBinding {
    let mut epochs = Records::new();
    epochs.append(1, EpochEntry { epoch: "1".into(), observed_at: at(opened) }).expect("epoch");
    PendingBinding {
        name: binding_name(convoy, role, &access()),
        spec: AccountBindingSpec {
            requester: requester_address(Some("demo"), convoy, role).expect("requester"),
            item: "claude-code".into(),
            requirement: access(),
            credential: CredentialName(credential.into()),
            account: Some("ada@example.com".into()),
            grants: grants(&["lab"]),
            candidates: [CredentialName(credential.into())].into(),
            exclusions: BTreeMap::new(),
            selection: Selection::Draw { index: 0 },
            epochs,
            opened_at: at(opened),
            state: State::new(BindingState::Open, at(opened)),
        },
    }
}

fn provision(binding: &str, home: &str, delivered: Option<&str>) -> AccountProvisionSpec {
    AccountProvisionSpec {
        binding: Ref::new(binding, home),
        place: "env-1".into(),
        environment_kind: "docker".into(),
        plan: ProvisionPlan {
            implementation: "claude-direct".into(),
            effective_access: "account".into(),
            mechanism: "oauth token".into(),
            update_mode: UpdateMode::Push,
        },
        home: "/homes/coder".into(),
        delivered: delivered.map(|generation| Delivered { epoch: "1".into(), generation: generation.into(), at: at(50) }),
    }
}

/// The contract. Each step states the behaviour it checks.
async fn contract(accounts: &dyn AccountRecords) {
    let changes = accounts.changes(NS).await.expect("changes");

    // Opening binds a requester; its binding is found by requester and requirement, open.
    accounts.open(NS, vec![pending("convoy-a", "coder", "cred-1", 10)]).await.expect("open");
    let first = accounts.requester_binding(NS, "convoy-a", "coder", &access()).await.expect("read").expect("bound");
    assert!(first.spec.state.value.is_open());
    assert_eq!(first.spec.credential.0, "cred-1");
    assert_eq!(accounts.binding(NS, &first.name).await.expect("by name"), Some(first.clone()));

    // A requester that holds an open binding keeps it: bindings are sticky (#3882).
    accounts.open(NS, vec![pending("convoy-a", "coder", "cred-2", 11)]).await.expect("open again");
    let kept = accounts.requester_binding(NS, "convoy-a", "coder", &access()).await.expect("read").expect("bound");
    assert_eq!(kept.spec.credential.0, "cred-1", "the open binding is kept");
    assert_eq!(accounts.convoy_bindings(NS, "convoy-a", true).await.expect("local").len(), 1);
    assert!(accounts.convoy_bindings(NS, "convoy-b", false).await.expect("other").is_empty());
    let (using, bound) = accounts.bound_convoys(NS, &["cred-1".into()]).await.expect("bound convoys");
    assert_eq!(
        (using.into_iter().collect::<Vec<_>>(), bound.into_iter().collect::<Vec<_>>()),
        (vec!["convoy-a".to_owned()], vec!["convoy-a".to_owned()])
    );

    // A regrant records the grants that now authorise the credential.
    accounts.regrant(NS, &kept, grants(&["fleet"])).await.expect("regrant");
    let regranted = accounts.requester_binding(NS, "convoy-a", "coder", &access()).await.expect("read").expect("bound");
    assert_eq!(regranted.spec.grants, grants(&["fleet"]));
    assert_ne!(regranted.version, kept.version, "a write moves the version");

    // A replacement login for the same account appends the next epoch.
    accounts.observe_epoch(NS, &regranted, EpochEntry { epoch: "2".into(), observed_at: at(20) }).await.expect("epoch");
    let epoched = accounts.requester_binding(NS, "convoy-a", "coder", &access()).await.expect("read").expect("bound");
    assert_eq!(epoched.spec.epochs.iter().map(|row| row.record.epoch.as_str()).collect::<Vec<_>>(), ["1", "2"]);

    // A provision is recorded once per place: without `replace` an existing record is kept, with
    // it the record is brought to the new spec.
    accounts.record_provision(NS, "prov-1", "convoy-a", &provision(&epoched.name, &epoched.home, None), false).await.expect("record");
    accounts.record_provision(NS, "prov-1", "convoy-a", &provision(&epoched.name, &epoched.home, Some("g1")), false).await.expect("kept");
    let recorded = accounts.provision(NS, "prov-1").await.expect("read").expect("recorded");
    assert_eq!(recorded.spec.delivered, None, "without replace the existing record stays");
    accounts.record_provision(NS, "prov-1", "convoy-a", &provision(&epoched.name, &epoched.home, Some("g1")), true).await.expect("replace");
    let delivered = accounts.provision(NS, "prov-1").await.expect("read").expect("recorded");
    assert_eq!(delivered.spec.delivered.as_ref().map(|delivered| delivered.generation.as_str()), Some("g1"));
    assert_eq!(accounts.binding_provisions(NS, std::slice::from_ref(&epoched.name), false).await.expect("by binding").len(), 1);
    assert_eq!(accounts.local_provisions(NS).await.expect("local").len(), 1);

    // A write over a stale read is superseded and changes nothing.
    let cleared = provision(&epoched.name, &epoched.home, None);
    assert_eq!(accounts.update_provision(NS, &recorded, &cleared).await.expect("stale"), ProvisionWrite::Superseded);
    assert_eq!(accounts.update_provision(NS, &delivered, &cleared).await.expect("current"), ProvisionWrite::Written);
    assert_eq!(accounts.provision(NS, "prov-1").await.expect("read").expect("recorded").spec.delivered, None);

    // A rebind closes the open binding for `rebind`, kept for attribution, and opens the next.
    accounts.rebind(NS, pending("convoy-a", "coder", "cred-2", 30), Some(at(30)), at(30)).await.expect("rebind");
    let rebound = accounts.requester_binding(NS, "convoy-a", "coder", &access()).await.expect("read").expect("bound");
    assert!(rebound.spec.state.value.is_open());
    assert_eq!(rebound.spec.credential.0, "cred-2");
    let all = accounts.convoy_bindings(NS, "convoy-a", false).await.expect("all");
    assert_eq!(all.len(), 2, "the closed binding is kept: {all:?}");
    let closed = all.iter().find(|binding| !binding.spec.state.value.is_open()).expect("closed");
    assert_eq!(closed.spec.state.value, BindingState::Closed { reason: "rebind".into(), delivered_until: Some(at(30)) });

    // Ending revokes the binding; the requester then reads its ended binding with the reason.
    accounts
        .end(NS, &rebound, BindingState::Revoked { reason: "no authorizing grant".into(), delivered_until: None }, at(40))
        .await
        .expect("revoke");
    let revoked = accounts.requester_binding(NS, "convoy-a", "coder", &access()).await.expect("read").expect("bound");
    assert_eq!(revoked.spec.state.value.reason(), Some("no authorizing grant"));
    assert_eq!(revoked.spec.credential.0, "cred-2", "the latest binding is the requester's");

    // Removing a provision removes it; removing an absent one is done.
    accounts.delete_provision(NS, "prov-1").await.expect("delete");
    accounts.delete_provision(NS, "prov-1").await.expect("absent");
    assert_eq!(accounts.provision(NS, "prov-1").await.expect("read"), None);

    // Every write reaches the change stream, a binding's with its convoy.
    let seen = tokio::time::timeout(
        Duration::from_secs(10),
        changes
            .take_while(|change| {
                let found = matches!(change, Ok(AccountChange::Binding { convoy: Some(convoy), .. }) if convoy == "convoy-a");
                std::future::ready(!found)
            })
            .count(),
    )
    .await;
    assert!(seen.is_ok(), "a binding change of convoy-a arrives");
}

#[tokio::test]
async fn the_old_store_keeps_the_account_records_contract() {
    let backend = ResourceBackend::new(InMemoryBackend::default());
    contract(&OldStoreAccounts::new(backend)).await;
}

#[tokio::test]
async fn the_engine_keeps_the_account_records_contract() {
    let directory = tempfile::tempdir().expect("temp dir");
    let mut kinds = flotilla_schema::KindRegistry::new();
    flotilla_provisioning::stored::account::register(&mut kinds).expect("register");
    let service = Arc::new(Service::open(&directory.path().join("decided.sqlite"), "alpha", kinds).expect("store"));
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    contract(&EngineAccounts::open(service, clock).expect("open")).await;
}

fn old_binding(
    name: &str,
    pending: PendingBinding,
    state: BindingState,
    home: &str,
) -> (flotilla_resources::ResourceObject<flotilla_resources::AccountBinding>, String) {
    let mut spec = pending.spec;
    spec.state = State::new(state, spec.opened_at);
    (object(name, spec), home.to_owned())
}

fn object<R: flotilla_resources::Resource<Status = ()>>(name: &str, spec: R::Spec) -> flotilla_resources::ResourceObject<R> {
    flotilla_resources::ResourceObject { metadata: flotilla_store_testkit::fixtures::object_meta(name, NS, "1"), spec, status: None }
}

// ADR 0047 (#3824 layer 1): every old binding, open or retired under a `closed-` name, imports
// under its requester and opening, and each provision is re-pointed at it, found by the node id
// it recorded even when its binding lists as this daemon's own; a provision naming a binding the
// listing lacks is refused, named.
#[test]
fn old_account_records_rehearse_onto_the_engine() {
    use flotilla_provisioning::engine::{account::rehearse_accounts, import::ImportReport};
    let open = pending("convoy-a", "coder", "cred-2", 30);
    let retired = pending("convoy-a", "coder", "cred-1", 10);
    let bindings = vec![
        old_binding(&open.name, open.clone(), BindingState::Open, "local"),
        old_binding(
            &format!("{}.closed-10", retired.name),
            retired,
            BindingState::Closed { reason: "rebind".into(), delivered_until: None },
            "local",
        ),
    ];
    let provisions = vec![
        (object("prov-1", provision(&open.name, "node-a", Some("g1"))), "local".to_owned()),
        (object("prov-2", provision("unlisted", "node-b", None)), "local".to_owned()),
    ];
    let mut report = ImportReport::default();
    rehearse_accounts(&bindings, &provisions, &mut report);
    assert_eq!(
        report.imported,
        BTreeMap::from([(("AccountBinding", "closed"), 1), (("AccountBinding", "open"), 1), (("AccountProvision", "delivered"), 1)])
    );
    assert_eq!(report.refused.len(), 1, "{:?}", report.refused);
    assert_eq!((report.refused[0].kind, report.refused[0].record.as_str()), ("AccountProvision", "prov-2"));
}
