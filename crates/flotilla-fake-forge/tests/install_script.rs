//! The operator install script, staged without root or systemd: what it
//! writes must start the service, reach it over TLS from a client that trusts
//! only the rehearsal CA, and print records flotilla accepts.
#![cfg(target_os = "linux")]

use std::{
    path::{Path, PathBuf},
    process::{Command, Output},
};

use flotilla_fake_forge::{serve, Config, SystemClock};
use serde_json::Value;

fn script() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scripts/rehearsal/install-fake-forge.sh")
}

fn stage(dir: &Path, extra: &[&str]) -> Output {
    let mut command = Command::new("bash");
    command.arg(script()).args(["--stage", dir.to_str().expect("UTF-8 path"), "--host", "localhost", "--ip", "127.0.0.1"]);
    command.args(["--bind", "127.0.0.1", "--port", "0", "--admin-port", "0"]);
    command.args(extra);
    command.output().expect("run the install script")
}

const FORGES: &[&str] = &["--forge", "github=github-rehearsal=github.com", "--forge", "lab=lab=forgejo.lab.flotilla.work,forgejo-manchego"];

fn read(path: impl AsRef<Path>) -> String {
    std::fs::read_to_string(path.as_ref()).unwrap_or_else(|error| panic!("read {}: {error}", path.as_ref().display()))
}

fn json(path: impl AsRef<Path>) -> Value {
    serde_json::from_str(&read(path)).expect("JSON document")
}

/// A client that trusts only the staged rehearsal CA, as a rehearsal host
/// does after installing it.
fn client(ca: &Path) -> reqwest::Client {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let ca = reqwest::Certificate::from_pem(read(ca).as_bytes()).expect("CA PEM");
    reqwest::Client::builder().tls_certs_only([ca]).build().expect("client")
}

// Behaviour (#3374): the script's output is a working installation. Its
// config loads, the seed applies, the service it describes serves each forge
// over HTTPS with a certificate the rehearsal CA vouches for, each forge
// accepts the token the script wrote for it, and the admin API accepts the
// admin token.
#[tokio::test]
async fn a_staged_install_serves_each_forge_over_tls() {
    let dir = tempfile::tempdir().expect("stage dir");
    let seed = dir.path().join("input-seed.json");
    std::fs::write(
        &seed,
        r#"{"forges": {"github": {"repositories": [{"owner": "flotilla-org", "name": "flotilla", "default_branch": "trunk"}]}}}"#,
    )
    .expect("seed");
    let stage_dir = dir.path().join("fake");
    let mut args = FORGES.to_vec();
    args.extend(["--seed", seed.to_str().expect("UTF-8"), "--auto-merge-after", "120", "--login", "rehearsal-crew"]);
    let output = stage(&stage_dir, &args);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));

    let config = Config::load(&stage_dir.join("config.toml")).expect("the staged config loads");
    assert_eq!(config.forges.keys().map(String::as_str).collect::<Vec<_>>(), ["github", "lab"]);
    assert_eq!(config.auto_merge_after, Some(std::time::Duration::from_secs(120)));
    for (mount, settings) in &config.forges {
        assert_eq!(settings.token, read(stage_dir.join("tokens").join(mount)).trim());
        assert_eq!(settings.login, "rehearsal-crew");
    }
    assert_ne!(config.forges["github"].token, config.forges["lab"].token, "each forge has its own token");

    let forge = serve::build_forge(&config, std::sync::Arc::new(SystemClock)).expect("forge with seed");
    let running = serve::start(&config, forge).await.expect("serve");
    let client = client(&stage_dir.join("rehearsal-ca.pem"));
    for (mount, settings) in &config.forges {
        let url = format!("https://localhost:{}/{mount}/api/v1/user", running.api_addr().port());
        let reply = client
            .get(&url)
            .header("accept", "application/json")
            .header("authorization", format!("token {}", settings.token))
            .send()
            .await
            .unwrap_or_else(|error| panic!("{url}: {error:?}"));
        assert_eq!(reply.status(), 200, "{mount}");
        assert_eq!(reply.json::<Value>().await.expect("user")["login"], "rehearsal-crew");
    }
    let state = client
        .get(format!("https://localhost:{}/admin/v1/state", running.admin_addr().port()))
        .header("authorization", format!("Bearer {}", read(stage_dir.join("admin-token")).trim()))
        .send()
        .await
        .expect("admin state");
    assert_eq!(state.status(), 200);
    let state: Value = state.json().await.expect("state");
    assert_eq!(state["github"]["repositories"]["flotilla-org/flotilla"]["default_branch"], "trunk", "the seed applied: {state}");
    running.shutdown().await;
}

// Behaviour: the records the script prints are ones flotilla accepts, as
// `flotilla resource validate` checks them, and they wire each forge to the
// fake. Each Forge record claims its hosts with the Forgejo kind and the
// fake's mount as `https_url`. Each CredentialSpec reads that forge's token
// from the client token directory. The daemon.toml lines map each forge id to
// its CredentialSpec.
#[test]
fn the_printed_records_are_valid_and_point_at_the_fake() {
    let dir = tempfile::tempdir().expect("stage dir");
    let output = stage(dir.path(), FORGES);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let apply = dir.path().join("apply");
    let daemon: toml::Value = toml::from_str(&read(apply.join("daemon.toml"))).expect("daemon.toml lines");
    for (mount, forge_id, hosts) in
        [("github", "github-rehearsal", vec!["github.com"]), ("lab", "lab", vec!["forgejo.lab.flotilla.work", "forgejo-manchego"])]
    {
        let forge = json(apply.join(format!("forge-{forge_id}.json")));
        flotilla_store::validate_resource_document(&forge).unwrap_or_else(|error| panic!("Forge {forge_id}: {error}"));
        assert_eq!(forge["spec"]["kind"], "forgejo");
        assert_eq!(forge["spec"]["https_url"], format!("https://localhost:0/{mount}"));
        assert_eq!(forge["spec"]["hosts"], serde_json::json!(hosts));

        let credential = json(apply.join(format!("credential-fake-forge-{forge_id}.json")));
        flotilla_store::validate_resource_document(&credential).unwrap_or_else(|error| panic!("CredentialSpec {forge_id}: {error}"));
        assert_eq!(credential["spec"]["consumer"]["forge_ref"], forge_id);
        assert_eq!(credential["spec"]["source"]["path"], format!("/etc/flotilla-rehearsal/tokens/{forge_id}"));
        assert_eq!(daemon["credentials"]["forgejo"][forge_id].as_str(), credential["metadata"]["name"].as_str());
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("flotilla resource apply -f forge-github-rehearsal.json"), "{stdout}");
    assert!(stdout.contains("[dummy_fleet.shared_forge]"), "{stdout}");
    assert!(stdout.contains("token_path = \"/etc/flotilla-rehearsal/tokens/<FORGE_ID>\""), "several forges leave the choice: {stdout}");
    // With one forge the dummy-crew stanza is filled in.
    let single = tempfile::tempdir().expect("stage dir");
    let output = stage(single.path(), &["--forge", "lab=lab=forgejo.lab.flotilla.work"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("service_url = \"https://localhost:0/lab\"\ntoken_path = \"/etc/flotilla-rehearsal/tokens/lab\""), "{stdout}");
}

// Behaviour: re-running keeps the CA, the forge tokens and the admin token,
// so hosts that already trust the CA and hold the tokens keep working, and
// reissues the server certificate.
#[test]
fn rerunning_keeps_the_ca_and_tokens() {
    let dir = tempfile::tempdir().expect("stage dir");
    assert!(stage(dir.path(), FORGES).status.success());
    let kept = ["rehearsal-ca.pem", "tokens/github", "tokens/lab", "admin-token"].map(|file| read(dir.path().join(file)));
    let certificate = read(dir.path().join("server.pem"));
    assert!(stage(dir.path(), FORGES).status.success());
    assert_eq!(kept, ["rehearsal-ca.pem", "tokens/github", "tokens/lab", "admin-token"].map(|file| read(dir.path().join(file))));
    assert_ne!(certificate, read(dir.path().join("server.pem")), "the server certificate is reissued");
}

// Behaviour: the script refuses input that would produce a broken rehearsal,
// before writing anything: the reserved `github` forge id (ADR 0066), a mount
// or forge id given twice, a forge without hosts, and a missing host name.
#[test]
fn the_script_refuses_broken_input() {
    for (case, args, expected) in [
        ("reserved id", vec!["--forge", "github=github=github.com"], "reserved for public GitHub"),
        ("repeated mount", vec!["--forge", "a=one=x.example", "--forge", "a=two=y.example"], "given twice"),
        ("repeated forge id", vec!["--forge", "a=one=x.example", "--forge", "b=one=y.example"], "given twice"),
        ("no hosts", vec!["--forge", "a=one="], "host names"),
        ("no forge", vec![], "at least one --forge"),
        ("quote in a forge host", vec!["--forge", "a=one=x.example\""], "host names"),
        ("bad ip", vec!["--forge", "a=one=x.example", "--ip", "10.0.0.1; rm"], "--ip"),
    ] {
        let dir = tempfile::tempdir().expect("stage dir");
        let staged = dir.path().join("fake");
        let output = stage(&staged, &args);
        assert!(!output.status.success(), "{case}");
        assert!(String::from_utf8_lossy(&output.stderr).contains(expected), "{case}: {}", String::from_utf8_lossy(&output.stderr));
        assert!(!staged.exists(), "{case}: nothing is written, not even the stage directory");
    }
    let dir = tempfile::tempdir().expect("stage dir");
    let staged = dir.path().join("fake");
    let output = Command::new("bash")
        .arg(script())
        .args(["--stage", staged.to_str().expect("UTF-8"), "--forge", "a=one=x.example"])
        .output()
        .expect("run");
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("--host is required"));
    let output = Command::new("bash")
        .arg(script())
        .args(["--stage", staged.to_str().expect("UTF-8"), "--host", "bad\"host", "--forge", "a=one=x.example"])
        .output()
        .expect("run");
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("must be a DNS name"));
    assert!(!staged.exists());
}
