use std::{fs, path::Path, sync::Arc, time::Duration};

use super::{Config, DEFAULT_LOGIN};
use crate::{
    model::{FakeForge, SystemClock},
    seed::{Seed, SeedForge, SeedRepository},
    ForgeSettings,
};

const VALID: &str = r#"
public_url = "https://fake-forge.rehearsal:3443/"
listen = "0.0.0.0:3443"
admin_listen = "0.0.0.0:3444"
tls_cert = "tls/server.pem"
tls_key = "/etc/flotilla-fake-forge/server-key.pem"
admin_token_file = "admin-token"
auto_merge_after_secs = 120
seed = "seed.json"

[forges.github]
token_file = "tokens/github"
login = "rehearsal-crew"

[forges.lab]
token_file = "/abs/tokens/lab"
"#;

/// A config directory holding the token files `VALID` names, with the lab
/// token at an absolute path inside the directory.
fn write_tokens(dir: &Path) -> String {
    fs::create_dir_all(dir.join("tokens")).expect("tokens dir");
    fs::write(dir.join("admin-token"), "admin-secret\n").expect("admin token");
    fs::write(dir.join("tokens/github"), "  github-secret \n").expect("github token");
    fs::write(dir.join("tokens/lab"), "lab-secret").expect("lab token");
    VALID.replace("/abs/tokens/lab", &dir.join("tokens/lab").display().to_string())
}

// Behaviour: a valid configuration loads with relative paths resolved against
// the config file's directory, absolute paths kept, tokens read from their
// files and trimmed, the login defaulted to `rehearsal`, and `public_url`
// normalised to an origin without a trailing slash.
#[test]
fn a_valid_config_loads_with_paths_resolved_and_tokens_read() {
    let dir = tempfile::tempdir().expect("tempdir");
    let text = write_tokens(dir.path());
    let path = dir.path().join("config.toml");
    fs::write(&path, text).expect("config");
    let config = Config::load(&path).expect("valid config");
    assert_eq!(config.public_url, "https://fake-forge.rehearsal:3443");
    assert_eq!(config.listen.to_string(), "0.0.0.0:3443");
    assert_eq!(config.tls_cert, dir.path().join("tls/server.pem"));
    assert_eq!(config.tls_key, Path::new("/etc/flotilla-fake-forge/server-key.pem"));
    assert_eq!(config.seed.as_deref(), Some(dir.path().join("seed.json").as_path()));
    assert_eq!(config.admin_token, "admin-secret");
    assert_eq!(config.auto_merge_after, Some(Duration::from_secs(120)));
    assert_eq!(config.forges["github"], ForgeSettings { token: "github-secret".into(), login: "rehearsal-crew".into() });
    assert_eq!(config.forges["lab"], ForgeSettings { token: "lab-secret".into(), login: DEFAULT_LOGIN.into() });
}

// Behaviour: configuration mistakes are refused with an error naming the
// problem: unknown fields at the top level and in a forge table, an empty or
// missing token file, a public URL that is not an https origin, a missing
// forge table, a zero auto-merge delay, one address for both listeners, and
// a forge sharing the admin token.
#[test]
fn config_mistakes_are_refused_with_clear_errors() {
    let dir = tempfile::tempdir().expect("tempdir");
    let valid = write_tokens(dir.path());
    fs::write(dir.path().join("empty-token"), " \n\t").expect("empty token");
    let cases: Vec<(&str, String, &str)> = vec![
        ("unknown top-level field", format!("verbose = true\n{valid}"), "unknown field `verbose`"),
        ("unknown forge field", valid.replace("login = \"rehearsal-crew\"", "password = \"x\""), "unknown field `password`"),
        ("empty token", valid.replace("tokens/github", "empty-token"), "is empty"),
        ("missing token file", valid.replace("tokens/github", "tokens/absent"), "cannot read forge `github` token file"),
        ("empty admin token", valid.replace("\"admin-token\"", "\"empty-token\""), "admin token file"),
        ("http public URL", valid.replace("https://fake-forge", "http://fake-forge"), "must be https"),
        ("public URL with a path", valid.replace(":3443/\"", ":3443/github\""), "without a path"),
        ("no forges", valid[..valid.find("[forges.github]").expect("forges")].to_string(), "at least one"),
        ("zero auto-merge", valid.replace("= 120", "= 0"), "at least 1"),
        ("shared listener", valid.replace("0.0.0.0:3444", "0.0.0.0:3443"), "its own listener"),
        ("forge shares admin token", valid.replace("tokens/github", "admin-token"), "shares the admin token"),
        ("forges share a token", valid.replace("tokens/lab", "tokens/github"), "share a token"),
        ("bad listen address", valid.replace("0.0.0.0:3443\"\nadmin", "localhost\"\nadmin"), "listen"),
    ];
    for (case, text, expected) in cases {
        let error = Config::parse(&text, dir.path()).expect_err(case);
        assert!(error.to_string().contains(expected), "{case}: {error}");
    }
    let without_optional = valid.replace("auto_merge_after_secs = 120\n", "").replace("seed = \"seed.json\"\n", "");
    let config = Config::parse(&without_optional, dir.path()).expect("optional fields may be omitted");
    assert_eq!((config.auto_merge_after, config.seed), (None, None));
}

fn forge() -> FakeForge {
    let forge = FakeForge::new("https://fake-forge.test", Arc::new(SystemClock));
    forge.add_forge("github", ForgeSettings { token: "t".into(), login: "rehearsal".into() }).expect("forge");
    forge
}

// Behaviour: a seed creates each listed repository with its default branch
// (defaulting to `main`), and round-trips through its JSON form, which
// rehearsal tooling generates.
#[test]
fn a_seed_creates_repositories_with_their_default_branches() {
    let seed = Seed::parse(
        r#"{"forges": {"github": {"repositories": [
            {"owner": "flotilla-org", "name": "flotilla", "default_branch": "trunk"},
            {"owner": "flotilla-org", "name": "cleat"}
        ]}}}"#,
    )
    .expect("valid seed");
    let forge = forge();
    assert_eq!(seed.apply(&forge).expect("applies"), 2);
    let snapshot = forge.snapshot();
    let repositories = &snapshot["github"].repositories;
    assert_eq!(repositories["flotilla-org/flotilla"].default_branch, "trunk");
    assert_eq!(repositories["flotilla-org/cleat"].default_branch, "main");
    let written = serde_json::to_string(&seed).expect("serialize");
    assert_eq!(Seed::parse(&written).expect("round trip"), seed);
}

// Behaviour: seed mistakes are refused: unknown fields, and a forge the
// config does not declare, which is refused before any repository is created
// so a seed for another fleet cannot half-apply.
#[test]
fn seed_mistakes_are_refused() {
    for (case, text) in [
        ("unknown top-level field", r#"{"forges": {}, "version": 1}"#),
        ("unknown repository field", r#"{"forges": {"github": {"repositories": [{"owner": "a", "name": "b", "private": true}]}}}"#),
        ("missing name", r#"{"forges": {"github": {"repositories": [{"owner": "a"}]}}}"#),
    ] {
        assert!(Seed::parse(text).is_err(), "{case}");
    }
    let repository = |name: &str| SeedRepository { owner: "flotilla-org".into(), name: name.into(), default_branch: "main".into() };
    let seed = Seed {
        forges: [
            ("github".to_string(), SeedForge { repositories: vec![repository("flotilla")] }),
            ("gitlab".to_string(), SeedForge { repositories: vec![repository("other")] }),
        ]
        .into(),
    };
    let forge = forge();
    let error = seed.apply(&forge).expect_err("unknown forge");
    assert!(error.to_string().contains("seed names forge `gitlab`"), "{error}");
    assert!(forge.snapshot()["github"].repositories.is_empty(), "nothing was seeded");
}
