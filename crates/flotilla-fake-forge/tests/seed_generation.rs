//! Generating the seed from the copied stores' Repository records.

use std::{collections::BTreeMap, path::PathBuf, process::Command};

use flotilla_fake_forge::seed::{seed_from_repository_records, Seed};
use hegel::generators as gs;
use serde_json::{json, Value};

/// The service binary Cargo builds for this package's integration tests.
fn binary() -> PathBuf {
    std::env::var_os("CARGO_BIN_EXE_flotilla-fake-forge").map(PathBuf::from).expect("Cargo names the binary for integration tests")
}

fn mounts() -> BTreeMap<String, String> {
    [("https://github.com".to_string(), "github".to_string())].into()
}

fn corpus() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../flotilla-store-testkit/tests/fixtures/stored-records/r355/Repository.json")
}

fn repository(service: &str, full_name: &str, default_branch: Option<&str>) -> Value {
    let mut record = json!({
        "apiVersion": "flotilla.work/v1",
        "kind": "Repository",
        "metadata": {"name": format!("repo-{full_name}")},
        "spec": {"identity": {"kind": "remote", "canonical_remote": format!("{service}/{full_name}")},
                 "forge": {"service_url": service, "repository": full_name}},
    });
    if let Some(branch) = default_branch {
        record["status"] = json!({"default_branch": branch});
    }
    record
}

// Behaviour (#3374): the seed comes from the copied stores' real Repository
// records. Over the stored-record corpus every github.com repository appears
// once, however many replicas name it, with the default branch its status
// reports. Records naming a forge with no mount are reported as skipped.
#[test]
fn the_stored_corpus_seeds_each_github_repository_once() {
    let records: Value = serde_json::from_str(&std::fs::read_to_string(corpus()).expect("corpus")).expect("JSON");
    let objects = records.as_array().expect("array of records");
    let generated = seed_from_repository_records(std::slice::from_ref(&records), &mounts()).expect("seed");
    let github = &generated.seed.forges["github"].repositories;
    let expected: std::collections::BTreeSet<String> = objects
        .iter()
        .filter(|record| record["spec"]["forge"]["service_url"] == "https://github.com")
        .map(|record| record["spec"]["forge"]["repository"].as_str().expect("repository").to_ascii_lowercase())
        .collect();
    let seeded: Vec<String> =
        github.iter().map(|repository| format!("{}/{}", repository.owner, repository.name).to_ascii_lowercase()).collect();
    assert_eq!(seeded.iter().cloned().collect::<std::collections::BTreeSet<_>>(), expected);
    assert_eq!(seeded.len(), expected.len(), "one entry per repository");
    assert!(objects.len() > seeded.len(), "the corpus has replicas, so this exercises merging");
    for repository in github {
        let reported = objects
            .iter()
            .filter(|record| {
                record["spec"]["forge"]["repository"]
                    .as_str()
                    .is_some_and(|name| name.eq_ignore_ascii_case(&format!("{}/{}", repository.owner, repository.name)))
            })
            .find_map(|record| record["status"]["default_branch"].as_str());
        assert_eq!(repository.default_branch, reported.unwrap_or("main"));
    }
    let unmapped = objects.iter().filter(|record| record["spec"]["forge"]["service_url"] != "https://github.com").count();
    assert_eq!(generated.skipped.len(), unmapped);
}

// Behaviour: merging is by mount and case-insensitive name, as Forgejo
// compares names. Every record whose forge has a mount is represented exactly
// once, under that mount, with the first default branch any copy reports
// (`main` when none does), and every other record is skipped. Input order
// across hosts' listings does not change which repositories are seeded.
// The generator draws 1..=12 records over two mapped forges and one unmapped,
// three names in varying case, with and without default branches, split
// across one or two input listings in envelope or array form.
#[hegel::test]
fn generated_listings_seed_each_mapped_repository_once(tc: hegel::TestCase) {
    let services = ["https://github.com", "https://forgejo.lab.example/", "https://other.example"];
    let names = ["Team/App", "team/app", "team/lib", "Other/Tool"];
    let branches = [None, Some("main"), Some("trunk")];
    let count = tc.draw(gs::integers::<usize>().min_value(1).max_value(12));
    let records: Vec<Value> = (0..count)
        .map(|_| {
            repository(
                services[tc.draw(gs::integers::<usize>().min_value(0).max_value(2))],
                names[tc.draw(gs::integers::<usize>().min_value(0).max_value(3))],
                branches[tc.draw(gs::integers::<usize>().min_value(0).max_value(2))],
            )
        })
        .collect();
    let split = tc.draw(gs::integers::<usize>().min_value(0).max_value(count));
    let envelope = tc.draw(gs::booleans());
    let wrap = |records: &[Value]| {
        if envelope {
            json!({"records": records.iter().map(|object| json!({"type": "object", "object": object})).collect::<Vec<_>>()})
        } else {
            Value::Array(records.to_vec())
        }
    };
    let inputs = [wrap(&records[..split]), wrap(&records[split..])];
    let mounts: BTreeMap<String, String> =
        [("https://github.com".into(), "github".into()), ("https://forgejo.lab.example".into(), "lab".into())].into();
    let generated = seed_from_repository_records(&inputs, &mounts).expect("seed");

    let mount_of = |service: &str| match service {
        "https://github.com" => Some("github"),
        "https://forgejo.lab.example/" => Some("lab"),
        _ => None,
    };
    let mut expected: BTreeMap<(String, String), String> = BTreeMap::new();
    for record in &records {
        let Some(mount) = mount_of(record["spec"]["forge"]["service_url"].as_str().expect("service")) else { continue };
        let name = record["spec"]["forge"]["repository"].as_str().expect("name").to_ascii_lowercase();
        let branch = record["status"]["default_branch"].as_str().map(str::to_string);
        let entry = expected.entry((mount.to_string(), name)).or_default();
        if entry.is_empty() {
            *entry = branch.unwrap_or_default();
        }
    }
    let actual: BTreeMap<(String, String), String> = generated
        .seed
        .forges
        .iter()
        .flat_map(|(mount, forge)| {
            forge.repositories.iter().map(move |repository| {
                (
                    (mount.clone(), format!("{}/{}", repository.owner, repository.name).to_ascii_lowercase()),
                    repository.default_branch.clone(),
                )
            })
        })
        .collect();
    let expected: BTreeMap<_, _> =
        expected.into_iter().map(|(key, branch)| (key, if branch.is_empty() { "main".to_string() } else { branch })).collect();
    assert_eq!(actual, expected);
    let seeded_count: usize = generated.seed.forges.values().map(|forge| forge.repositories.len()).sum();
    assert_eq!(seeded_count, actual.len(), "no repository twice under one mount");
    let unmapped = records.iter().filter(|record| record["spec"]["forge"]["service_url"] == "https://other.example").count();
    assert_eq!(generated.skipped.len(), unmapped);
}

// Behaviour: input that is not a listing of Repository records is refused
// rather than producing an empty seed.
#[test]
fn other_inputs_are_refused() {
    assert!(seed_from_repository_records(&[json!({"items": []})], &mounts()).is_err());
    assert!(seed_from_repository_records(&[json!("text")], &mounts()).is_err());
    let convoy = json!([{"kind": "Convoy", "metadata": {"name": "c"}, "spec": {}}]);
    assert!(seed_from_repository_records(&[convoy], &mounts()).expect_err("not repositories").contains("Convoy"));
}

// Behaviour (glue): the `seed` subcommand writes the seed the service loads,
// and reports skipped records on standard error.
#[test]
fn the_seed_command_writes_a_loadable_seed() {
    let dir = tempfile::tempdir().expect("dir");
    let listing = dir.path().join("kiwi.json");
    let records = json!({"records": [{"type": "object", "object": repository("https://github.com", "flotilla-org/flotilla", Some("main"))},
                                     {"type": "object", "object": repository("https://elsewhere.example", "a/b", None)}]});
    std::fs::write(&listing, records.to_string()).expect("listing");
    let output_path = dir.path().join("seed.json");
    let output = Command::new(binary())
        .args(["seed", "--repositories", listing.to_str().expect("UTF-8"), "--forge", "https://github.com=github"])
        .args(["--output", output_path.to_str().expect("UTF-8")])
        .output()
        .expect("run seed");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert!(String::from_utf8_lossy(&output.stderr).contains("skipped Repository repo-a/b"));
    let seed = Seed::load(&output_path).expect("the service loads it");
    assert_eq!(seed.forges["github"].repositories.len(), 1);
    let refused = Command::new(binary())
        .args(["seed", "--repositories", listing.to_str().expect("UTF-8"), "--forge", "github"])
        .output()
        .expect("run seed");
    assert!(!refused.status.success(), "a mapping without SERVICE_URL=MOUNT is refused");
}
