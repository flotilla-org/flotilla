use std::{collections::BTreeSet, time::Duration};

use tender::{memory::MemoryTender, Availability, Error, Fingerprint, Grant, Namespace, PublicationId, PublishRequest, Session, Tender};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    time::timeout,
};

// Scenarios use only Tender and host controls. A transport adapter can run
// the same suite by providing a new rig without duplicating expectations.
trait HostControls {
    fn grant(&self, grant: Grant);
    fn allow_browse(&self, caller: Fingerprint);
    fn allow_connect(&self, caller: Fingerprint);
    fn trust_intermediary(&self, relay: Fingerprint);
    fn assign_replacement(&self, id: PublicationId, replacement: Fingerprint);
    fn revoke(&self, grantee: &Fingerprint, namespace: &Namespace);
    fn advance_to(&self, now: u64);
    fn restart(&self);
}

impl HostControls for MemoryTender {
    fn grant(&self, grant: Grant) {
        self.grant(grant);
    }
    fn allow_browse(&self, caller: Fingerprint) {
        self.allow_browse(caller);
    }
    fn allow_connect(&self, caller: Fingerprint) {
        self.allow_connect(caller);
    }
    fn trust_intermediary(&self, relay: Fingerprint) {
        self.trust_intermediary(relay);
    }
    fn assign_replacement(&self, id: PublicationId, replacement: Fingerprint) {
        self.assign_replacement(id, replacement);
    }
    fn revoke(&self, grantee: &Fingerprint, namespace: &Namespace) {
        self.revoke(grantee, namespace);
    }
    fn advance_to(&self, now: u64) {
        self.advance_to(now);
    }
    fn restart(&self) {
        self.restart();
    }
}

struct Rig {
    tender: Box<dyn Tender>,
    host: Box<dyn HostControls>,
    host_id: Fingerprint,
}

fn memory() -> Rig {
    let host_id = fp("host-key");
    let host = MemoryTender::new(host_id.clone());
    Rig { tender: Box::new(host.clone()), host: Box::new(host), host_id }
}

fn fp(value: &str) -> Fingerprint {
    Fingerprint(value.into())
}
fn ns(value: &str) -> Namespace {
    Namespace(value.into())
}
fn set(values: &[&str]) -> BTreeSet<Fingerprint> {
    values.iter().map(|value| fp(value)).collect()
}
fn session(caller: &str, host: &Fingerprint) -> Session {
    Session { caller: fp(caller), pinned_host: host.clone(), via: None }
}
fn request(namespace: &str, name: &str, audience: &[&str], reclaim: Option<PublicationId>) -> PublishRequest {
    PublishRequest { namespace: ns(namespace), name: name.into(), audience: set(audience), reclaim }
}
fn grant(rig: &Rig, grantee: &str, namespace: &str, audience: &[&str], expires_at: u64) {
    rig.host.grant(Grant { grantee: fp(grantee), namespace: ns(namespace), audience_ceiling: set(audience), expires_at });
}
fn reader(rig: &Rig, name: &str) -> Session {
    rig.host.allow_browse(fp(name));
    rig.host.allow_connect(fp(name));
    session(name, &rig.host_id)
}

async fn round_trip(
    rig: &Rig,
    client: &Session,
    id: PublicationId,
    incoming: &mut tokio::sync::mpsc::UnboundedReceiver<tender::ByteStream>,
) {
    let mut stream = rig.tender.connect(client, id).await.expect("connect");
    let mut service = incoming.recv().await.expect("service channel");
    stream.write_all(b"ordered bytes").await.expect("request write");
    stream.shutdown().await.expect("client half close");
    let mut request = Vec::new();
    service.read_to_end(&mut request).await.expect("service read");
    assert_eq!(request, b"ordered bytes");
    service.write_all(b"response").await.expect("response write");
    service.shutdown().await.expect("service half close");
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.expect("client read");
    assert_eq!(response, b"response");
}

async fn existing_service_on_ssh_host(rig: Rig) {
    let publisher = session("host-user", &rig.host_id);
    let client = reader(&rig, "laptop");
    grant(&rig, "host-user", "services", &["laptop"], 100);
    let mut published =
        rig.tender.publish(&publisher, request("services", "cleat", &["laptop"], None)).await.expect("publish existing endpoint");
    let mut watch = rig.tender.watch(&client).await.expect("watch");
    assert_eq!(watch.recv().await.expect("snapshot")[0].availability, Availability::Available);
    let exposure = rig.tender.expose_local(&client, published.lease.id).await.expect("expose");
    assert!(exposure.local_name.contains(&published.lease.id.0.to_string()));
    round_trip(&rig, &client, published.lease.id, &mut published.incoming).await;
    rig.tender.disconnect(&publisher, &published.lease).await.expect("disconnect");
    assert_eq!(watch.recv().await.expect("unavailable")[0].availability, Availability::Unavailable);
    assert!(matches!(rig.tender.open_exposure(&client, &exposure).await, Err(Error::Unavailable)));
    let mut reclaimed =
        rig.tender.publish(&publisher, request("services", "renamed-cleat", &["laptop"], Some(published.lease.id))).await.expect("reclaim");
    let visible = watch.recv().await.expect("new generation");
    assert_eq!(visible[0].generation, 2);
    assert_eq!(visible[0].name, "renamed-cleat");
    assert_eq!(exposure.publication, reclaimed.lease.id);
    round_trip(&rig, &client, exposure.publication, &mut reclaimed.incoming).await;
}

async fn scoped_container_publisher(rig: Rig) {
    let helper = session("container-key", &rig.host_id);
    let parent = reader(&rig, "parent");
    let outsider = reader(&rig, "outsider");
    grant(&rig, "container-key", "container/42", &["parent"], 10);
    let mut published =
        rig.tender.publish(&helper, request("container/42", "api", &["parent", "outsider"], None)).await.expect("scoped publish");
    assert_eq!(rig.tender.browse(&parent).await.expect("browse")[0].audience, set(&["parent"]));
    assert!(rig.tender.browse(&outsider).await.expect("browse").is_empty());
    assert!(matches!(rig.tender.connect(&outsider, published.lease.id).await, Err(Error::Denied)));
    assert!(matches!(rig.tender.browse(&helper).await, Err(Error::Denied)));
    assert!(matches!(rig.tender.connect(&helper, published.lease.id).await, Err(Error::Denied)));
    assert!(matches!(rig.tender.publish(&helper, request("parent", "escape", &["parent"], None)).await, Err(Error::Denied)));
    round_trip(&rig, &parent, published.lease.id, &mut published.incoming).await;
}

async fn roaming_via_pinned_intermediary(rig: Rig) {
    rig.host.trust_intermediary(fp("relay-key"));
    let mut publisher = session("roaming-key", &rig.host_id);
    publisher.via = Some(fp("relay-key"));
    let mut client = reader(&rig, "home-client");
    client.via = Some(fp("relay-key"));
    grant(&rig, "roaming-key", "mobile", &["home-client"], 100);
    let mut published =
        rig.tender.publish(&publisher, request("mobile", "notes", &["home-client"], None)).await.expect("publish via relay");
    round_trip(&rig, &client, published.lease.id, &mut published.incoming).await;
    let relay = session("relay-key", &rig.host_id);
    assert!(matches!(rig.tender.browse(&relay).await, Err(Error::Denied)));
    assert!(matches!(rig.tender.connect(&relay, published.lease.id).await, Err(Error::Denied)));
    let mut wrong_pin = client.clone();
    wrong_pin.pinned_host = fp("other-host");
    assert!(matches!(rig.tender.connect(&wrong_pin, published.lease.id).await, Err(Error::IdentityMismatch)));
    let mut wrong_relay = client;
    wrong_relay.via = Some(fp("impostor"));
    assert!(matches!(rig.tender.connect(&wrong_relay, published.lease.id).await, Err(Error::UntrustedIntermediary)));
}

async fn lifecycle_and_stale_teardown(rig: Rig) {
    let publisher = session("owner", &rig.host_id);
    let contender = session("contender", &rig.host_id);
    let client = reader(&rig, "consumer");
    grant(&rig, "owner", "n", &["consumer"], 100);
    grant(&rig, "contender", "n", &["consumer"], 100);
    let first = rig.tender.publish(&publisher, request("n", "service", &["consumer"], None)).await.expect("first");
    assert!(matches!(
        rig.tender.publish(&publisher, request("n", "service", &["consumer"], Some(first.lease.id))).await,
        Err(Error::LivePublisher)
    ));
    assert!(matches!(
        rig.tender.publish(&contender, request("n", "service", &["consumer"], Some(first.lease.id))).await,
        Err(Error::LivePublisher)
    ));
    rig.tender.disconnect(&publisher, &first.lease).await.expect("disconnect");
    let second = rig.tender.publish(&publisher, request("n", "service", &["consumer"], Some(first.lease.id))).await.expect("reclaim");
    assert!(matches!(rig.tender.withdraw(&publisher, &first.lease).await, Err(Error::StaleGeneration)));
    assert!(matches!(rig.tender.disconnect(&publisher, &first.lease).await, Err(Error::StaleGeneration)));
    assert_eq!(rig.tender.browse(&client).await.expect("browse")[0].availability, Availability::Available);
    rig.tender.withdraw(&publisher, &second.lease).await.expect("withdraw");
    assert_eq!(rig.tender.browse(&client).await.expect("browse")[0].availability, Availability::Withdrawn);
    assert!(matches!(
        rig.tender.publish(&publisher, request("n", "service", &["consumer"], Some(first.lease.id))).await,
        Err(Error::Withdrawn)
    ));
}

async fn grant_loss_and_expiry(rig: Rig) {
    let publisher = session("owner", &rig.host_id);
    let client = reader(&rig, "consumer");
    grant(&rig, "owner", "n", &["consumer"], 10);
    let mut first = rig.tender.publish(&publisher, request("n", "service", &["consumer"], None)).await.expect("publish");
    let mut connection = rig.tender.connect(&client, first.lease.id).await.expect("open");
    let mut service = first.incoming.recv().await.expect("incoming");
    rig.host.advance_to(10);
    assert!(matches!(rig.tender.connect(&client, first.lease.id).await, Err(Error::Withdrawn)));
    assert!(matches!(rig.tender.publish(&publisher, request("n", "new", &["consumer"], None)).await, Err(Error::GrantExpired)));
    connection.write_all(b"grandfathered").await.expect("existing stream survives expiry");
    let mut bytes = [0; 13];
    service.read_exact(&mut bytes).await.expect("service sees bytes");
    assert_eq!(&bytes, b"grandfathered");

    grant(&rig, "owner", "n", &["consumer"], 20);
    let mut second = rig.tender.publish(&publisher, request("n", "new", &["consumer"], None)).await.expect("republish");
    let mut connection = rig.tender.connect(&client, second.lease.id).await.expect("open");
    let _service = second.incoming.recv().await.expect("incoming");
    rig.host.revoke(&fp("owner"), &ns("n"));
    assert!(matches!(rig.tender.connect(&client, second.lease.id).await, Err(Error::Withdrawn)));
    assert!(matches!(rig.tender.publish(&publisher, request("n", "newer", &["consumer"], None)).await, Err(Error::Denied)));
    let mut byte = [0];
    assert_eq!(timeout(Duration::from_secs(1), connection.read(&mut byte)).await.expect("revocation closes promptly").expect("read"), 0);
}

async fn no_replay_or_rebind(rig: Rig) {
    let publisher = session("owner", &rig.host_id);
    let client = reader(&rig, "consumer");
    grant(&rig, "owner", "n", &["consumer"], 100);
    let mut first = rig.tender.publish(&publisher, request("n", "same-name", &["consumer"], None)).await.expect("publish");
    let exposure = rig.tender.expose_local(&client, first.lease.id).await.expect("expose");
    let mut old_stream = rig.tender.open_exposure(&client, &exposure).await.expect("open");
    let mut old_service = first.incoming.recv().await.expect("incoming");
    old_stream.write_all(b"before").await.expect("write");
    let mut bytes = [0; 6];
    old_service.read_exact(&mut bytes).await.expect("read");
    rig.tender.disconnect(&publisher, &first.lease).await.expect("disconnect");
    let mut byte = [0];
    assert_eq!(timeout(Duration::from_secs(1), old_stream.read(&mut byte)).await.expect("close").expect("read"), 0);
    let mut replacement =
        rig.tender.publish(&publisher, request("n", "same-name", &["consumer"], None)).await.expect("same display name, new id");
    assert_ne!(replacement.lease.id, exposure.publication);
    assert!(matches!(rig.tender.open_exposure(&client, &exposure).await, Err(Error::Unavailable)));
    let mut reclaimed =
        rig.tender.publish(&publisher, request("n", "renamed", &["consumer"], Some(first.lease.id))).await.expect("reclaim old identity");
    let mut new_stream = rig.tender.open_exposure(&client, &exposure).await.expect("reopen");
    let mut new_service = reclaimed.incoming.recv().await.expect("new channel");
    assert!(timeout(Duration::from_millis(30), new_service.read(&mut byte)).await.is_err(), "no bytes replayed");
    new_stream.write_all(b"fresh").await.expect("new write");
    let mut fresh = [0; 5];
    new_service.read_exact(&mut fresh).await.expect("new read");
    assert_eq!(&fresh, b"fresh");
    round_trip(&rig, &client, replacement.lease.id, &mut replacement.incoming).await;
}

async fn restart_and_replacement(rig: Rig) {
    let original = session("first-helper", &rig.host_id);
    let replacement = session("second-helper", &rig.host_id);
    let client = reader(&rig, "consumer");
    grant(&rig, "first-helper", "n", &["consumer"], 100);
    grant(&rig, "second-helper", "n", &["consumer"], 100);
    let first = rig.tender.publish(&original, request("n", "service", &["consumer"], None)).await.expect("publish");
    rig.host.restart();
    assert_eq!(rig.tender.browse(&client).await.expect("browse")[0].availability, Availability::Unavailable);
    assert!(matches!(
        rig.tender.publish(&replacement, request("n", "service", &["consumer"], Some(first.lease.id))).await,
        Err(Error::Denied)
    ));
    rig.host.assign_replacement(first.lease.id, fp("second-helper"));
    let second =
        rig.tender.publish(&replacement, request("n", "service", &["consumer"], Some(first.lease.id))).await.expect("explicit assignment");
    assert_eq!(second.lease.generation, 2);
    assert!(matches!(rig.tender.disconnect(&original, &first.lease).await, Err(Error::StaleGeneration)));
}

async fn publisher_receiver_closure(rig: Rig) {
    let publisher = session("owner", &rig.host_id);
    let client = reader(&rig, "consumer");
    grant(&rig, "owner", "n", &["consumer"], 100);
    let mut watch = rig.tender.watch(&client).await.expect("watch");
    assert!(watch.recv().await.expect("initial").is_empty());
    let published = rig.tender.publish(&publisher, request("n", "service", &["consumer"], None)).await.expect("publish");
    assert_eq!(watch.recv().await.expect("available")[0].availability, Availability::Available);
    let id = published.lease.id;
    drop(published);
    let snapshot = timeout(Duration::from_secs(1), watch.recv()).await.expect("closure update").expect("watch open");
    assert_eq!(snapshot[0].availability, Availability::Unavailable);
    assert!(matches!(rig.tender.connect(&client, id).await, Err(Error::Unavailable)));
}

async fn narrowed_grant_removes_live_audience(rig: Rig) {
    let publisher = session("owner", &rig.host_id);
    let kept = reader(&rig, "kept");
    let removed = reader(&rig, "removed");
    grant(&rig, "owner", "n", &["kept", "removed"], 100);
    let mut published = rig.tender.publish(&publisher, request("n", "service", &["kept", "removed"], None)).await.expect("publish");
    let mut watch = rig.tender.watch(&removed).await.expect("watch");
    assert_eq!(watch.recv().await.expect("initial").len(), 1);
    let mut old_stream = rig.tender.connect(&removed, published.lease.id).await.expect("old access");
    let _service = published.incoming.recv().await.expect("incoming");
    grant(&rig, "owner", "n", &["kept"], 100);
    assert!(watch.recv().await.expect("narrowed").is_empty());
    assert!(matches!(rig.tender.connect(&removed, published.lease.id).await, Err(Error::Denied)));
    assert!(matches!(rig.tender.connect(&removed, PublicationId(u64::MAX)).await, Err(Error::Denied)));
    let mut byte = [0];
    assert_eq!(timeout(Duration::from_secs(1), old_stream.read(&mut byte)).await.expect("removed stream closes").expect("read"), 0);
    round_trip(&rig, &kept, published.lease.id, &mut published.incoming).await;
}

async fn backpressure_and_cancel(rig: Rig) {
    let publisher = session("owner", &rig.host_id);
    let client = reader(&rig, "consumer");
    grant(&rig, "owner", "n", &["consumer"], 100);
    let mut published = rig.tender.publish(&publisher, request("n", "service", &["consumer"], None)).await.expect("publish");
    let mut connection = rig.tender.connect_with_deadline(&client, published.lease.id, Duration::from_secs(1)).await.expect("bounded open");
    let mut service = published.incoming.recv().await.expect("incoming");
    assert!(
        timeout(Duration::from_millis(30), connection.write_all(&vec![7; 1_000_000])).await.is_err(),
        "bounded stream must apply backpressure"
    );
    drop(connection); // Cooperative cancellation closes this one raw channel.
    let mut buf = [0; 256];
    let mut total = 0;
    loop {
        let n = timeout(Duration::from_secs(1), service.read(&mut buf)).await.expect("cancel closes").expect("read");
        if n == 0 {
            break;
        }
        total += n;
    }
    assert!(total < 1_000_000);
}

macro_rules! contract {
    ($name:ident) => {
        mod $name {
            #[tokio::test]
            async fn run() {
                super::$name(super::memory()).await;
            }
        }
    };
}

contract!(existing_service_on_ssh_host);
contract!(scoped_container_publisher);
contract!(roaming_via_pinned_intermediary);
contract!(lifecycle_and_stale_teardown);
contract!(grant_loss_and_expiry);
contract!(no_replay_or_rebind);
contract!(restart_and_replacement);
contract!(publisher_receiver_closure);
contract!(narrowed_grant_removes_live_audience);
contract!(backpressure_and_cancel);
