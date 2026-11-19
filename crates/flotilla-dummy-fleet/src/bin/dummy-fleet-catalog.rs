//! Write the dummy fleet's catalog as resource documents, one JSON file per
//! document, for `flotilla resource apply --file` on a daemon running with
//! `[dummy_fleet]`, ending with the Fleet project's standing governor. Prints
//! the files in apply order, one path per line.
//!
//! ```sh
//! dummy-fleet-catalog --host-ref NAME --out-dir DIR \
//!   [--long-runner-turns 5] [--forge-service-url https://forge.dummy.invalid]
//! ```
//!
//! `--host-ref` is the local Host resource's name. `--long-runner-turns` and
//! `--forge-service-url` default to the `[dummy_fleet]` defaults; pass the
//! daemon's values when its `daemon.toml` sets them.

use std::{path::PathBuf, process::ExitCode};

use flotilla_config::dummy_fleet::{DummyAgentConfig, DEFAULT_DUMMY_FORGE_SERVICE_URL};
use flotilla_dummy_fleet::catalog;

const USAGE: &str = "usage: dummy-fleet-catalog --host-ref NAME --out-dir DIR [--long-runner-turns N] [--forge-service-url URL]";

fn run() -> Result<(), String> {
    let (mut host_ref, mut out_dir) = (None, None);
    let mut turns = DummyAgentConfig::default().long_runner_turns;
    let mut forge_service_url = DEFAULT_DUMMY_FORGE_SERVICE_URL.to_string();
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        if matches!(flag.as_str(), "-h" | "--help") {
            println!("{USAGE}");
            return Ok(());
        }
        let value = args.next().ok_or_else(|| format!("{flag} needs a value\n{USAGE}"))?;
        match flag.as_str() {
            "--host-ref" => host_ref = Some(value),
            "--out-dir" => out_dir = Some(PathBuf::from(value)),
            "--long-runner-turns" => turns = value.parse().map_err(|error| format!("--long-runner-turns {value}: {error}"))?,
            "--forge-service-url" => forge_service_url = value,
            _ => return Err(format!("unknown argument {flag}\n{USAGE}")),
        }
    }
    let host_ref = host_ref.filter(|name| !name.is_empty()).ok_or_else(|| format!("--host-ref is required\n{USAGE}"))?;
    let out_dir = out_dir.ok_or_else(|| format!("--out-dir is required\n{USAGE}"))?;
    let mut documents = catalog::documents(&forge_service_url, &host_ref, turns)?;
    documents.extend(catalog::standing_governor_documents(&forge_service_url)?);
    for path in catalog::write_documents(&out_dir, &documents)? {
        println!("{}", path.display());
    }
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("dummy-fleet-catalog: {error}");
            ExitCode::FAILURE
        }
    }
}
