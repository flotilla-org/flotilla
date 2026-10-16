use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use chrono::{DateTime, TimeZone, Utc};
use tokio::{io::AsyncWriteExt, net::TcpStream, sync::watch};

use super::{auto_merge, build_forge, start, AUTO_MERGE_TICK};
use crate::{
    model::{Clock, SystemClock},
    Config, FakeForge, ForgeSettings, NewPull, RepoRef,
};

const ADMIN_TOKEN: &str = "admin-test-token";
const LAB_TOKEN: &str = "lab-test-token";

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
}

/// A client that trusts only the test certificate authority, as a rehearsal
/// host trusts only the rehearsal CA for the fake.
fn client() -> reqwest::Client {
    // reqwest is built without a TLS provider; select the workspace's ring
    // provider, as flotilla-tls does for the daemon.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let ca = reqwest::Certificate::from_pem(&std::fs::read(fixture("ca.pem")).expect("CA fixture")).expect("CA PEM");
    reqwest::Client::builder().tls_certs_only([ca]).build().expect("client")
}

fn config() -> Config {
    Config {
        public_url: "https://localhost:3443".into(),
        listen: "127.0.0.1:0".parse().expect("addr"),
        admin_listen: "127.0.0.1:0".parse().expect("addr"),
        tls_cert: fixture("server.pem"),
        tls_key: fixture("server-key.pem"),
        admin_token: ADMIN_TOKEN.into(),
        auto_merge_after: None,
        seed: None,
        forges: [("lab".to_string(), ForgeSettings { token: LAB_TOKEN.into(), login: "lab-bot".into() })].into(),
    }
}

// Behaviour (#3374): the service answers the Forgejo API and the admin API
// over HTTPS, each on its own listener, with a certificate a client trusting
// only the rehearsal CA accepts. A connection whose TLS handshake fails is
// dropped alone: the listener keeps serving. Shutdown stops the listeners.
// Boundary: real sockets on 127.0.0.1 and a real TLS stack, which is what
// this test exists to cover.
#[tokio::test]
async fn serves_both_apis_over_https_and_survives_a_failed_handshake() {
    let config = config();
    let forge = build_forge(&config, Arc::new(SystemClock)).expect("forge");
    let running = start(&config, Arc::clone(&forge)).await.expect("start");
    let client = client();
    let api = format!("https://localhost:{}", running.api_addr().port());
    let admin = format!("https://localhost:{}", running.admin_addr().port());

    // A plaintext request fails its handshake; the listener stays up.
    let mut plain = TcpStream::connect(running.api_addr()).await.expect("connect");
    plain.write_all(b"GET /lab/api/v1/user HTTP/1.1\r\nhost: localhost\r\n\r\n").await.expect("write");
    drop(plain);

    let user = client
        .get(format!("{api}/lab/api/v1/user"))
        .header("authorization", format!("token {LAB_TOKEN}"))
        .header("accept", "application/json")
        .send()
        .await
        .expect("HTTPS request to the Forgejo API");
    assert_eq!(user.status(), reqwest::StatusCode::OK);
    let user: serde_json::Value = user.json().await.expect("JSON");
    assert_eq!(user["login"], "lab-bot");

    let issue = client
        .post(format!("{admin}/admin/v1/forges/lab/repos/flotilla-org/flotilla/issues"))
        .bearer_auth(ADMIN_TOKEN)
        .json(&serde_json::json!({"title": "Opened over HTTPS"}))
        .send()
        .await
        .expect("HTTPS request to the admin API");
    assert_eq!(issue.status(), reqwest::StatusCode::CREATED);
    assert_eq!(forge.item(&RepoRef::new("lab", "flotilla-org", "flotilla"), 1).expect("issue").title, "Opened over HTTPS");

    // The admin API is not served on the Forgejo listener, nor the reverse.
    let crossed = client.get(format!("{api}/admin/v1/state")).bearer_auth(ADMIN_TOKEN).send().await.expect("request");
    assert_eq!(crossed.status(), reqwest::StatusCode::NOT_FOUND);

    let addr = running.api_addr();
    tokio::time::timeout(Duration::from_secs(15), running.shutdown()).await.expect("shutdown completes");
    assert!(TcpStream::connect(addr).await.is_err(), "the listener is closed after shutdown");
}

// Behaviour: a missing or mismatched certificate fails startup with an error
// naming the file, rather than serving without TLS.
#[tokio::test]
async fn bad_tls_material_fails_startup() {
    let mut config = config();
    config.tls_key = fixture("ca.pem");
    let forge = build_forge(&config, Arc::new(SystemClock)).expect("forge");
    let error = start(&config, forge).await.err().expect("refused");
    assert!(error.to_string().contains("ca.pem"), "{error}");
    config.tls_cert = fixture("absent.pem");
    let forge = build_forge(&config, Arc::new(SystemClock)).expect("forge");
    let error = start(&config, forge).await.err().expect("refused");
    assert!(error.to_string().contains("absent.pem"), "{error}");
}

struct ManualClock(Mutex<DateTime<Utc>>);

impl ManualClock {
    fn advance(&self, by: Duration) {
        *self.0.lock().expect("clock") += chrono::Duration::from_std(by).expect("duration");
    }
}

impl Clock for ManualClock {
    fn now(&self) -> DateTime<Utc> {
        *self.0.lock().expect("clock")
    }
}

// Behaviour: with `auto_merge_after_secs`, the service merges an open,
// non-draft pull request once it has been open that long, checking every
// second, and stops when shutdown is requested. One second short of the
// delay it stays open; at the delay it merges on the next check.
#[tokio::test(start_paused = true)]
async fn auto_merge_merges_pulls_once_they_are_due() {
    let clock = Arc::new(ManualClock(Mutex::new(Utc.with_ymd_and_hms(2026, 10, 16, 12, 0, 0).single().expect("time"))));
    let forge = Arc::new(FakeForge::new("https://localhost", clock.clone()));
    forge.add_forge("lab", ForgeSettings { token: LAB_TOKEN.into(), login: "lab-bot".into() }).expect("forge");
    let repo = RepoRef::new("lab", "flotilla-org", "flotilla");
    let pull = forge
        .create_pull(
            &repo,
            NewPull { head: "feature".into(), base: "main".into(), title: "t".into(), body: String::new(), user: None, draft: false },
        )
        .expect("pull");
    let (stop, stopped) = watch::channel(false);
    let task = tokio::spawn(auto_merge(Arc::clone(&forge), Duration::from_secs(120), stopped));

    clock.advance(Duration::from_secs(119));
    tokio::time::sleep(AUTO_MERGE_TICK * 3).await;
    assert!(!forge.pull(&repo, pull.number).expect("pull").merged(), "not yet due");
    clock.advance(Duration::from_secs(1));
    tokio::time::sleep(AUTO_MERGE_TICK + Duration::from_millis(100)).await;
    assert!(forge.pull(&repo, pull.number).expect("pull").merged(), "merged on the next check once due");

    stop.send(true).expect("loop listening");
    tokio::time::timeout(Duration::from_secs(5), task).await.expect("loop stops").expect("loop did not panic");
}
