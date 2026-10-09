//! Smoke-test for environment provisioning: create a Docker container,
//! run discovery inside it, and perform a git clone --reference checkout.
//!
//! Requires Docker and a local git repo with a remote.
//!
//! Usage:
//!   cargo run -p flotilla-core --example environment_checkout -- /path/to/repo branch registry/repo@sha256:digest
//!
//! Example:
//!   cargo run -p flotilla-core --example environment_checkout -- . main registry/repo@sha256:digest

use std::{path::Path, sync::Arc};

use flotilla_core::{
    config::ConfigStore,
    discovery_api::{EnvironmentAssertion, EnvironmentBag},
    providers::{
        discovery::{detectors::default_host_detectors, run_host_detectors, FactoryRegistry, ProcessEnvVars},
        environment::{CreateOpts, EnvironmentKind},
        ChannelLabel, CommandRunner, ProcessCommandRunner,
    },
    vcs::RepositoryRead,
};
use flotilla_paths::path_context::ExecutionEnvironmentPath;
use flotilla_protocol::{DaemonHostPath, EnvironmentId, EnvironmentSpec, ImageSource};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let repo_path = std::env::args().nth(1).unwrap_or_else(|| ".".to_string());
    let branch = std::env::args().nth(2).unwrap_or_else(|| "main".to_string());

    let repo_path = std::fs::canonicalize(&repo_path)?;
    println!("Repo:   {}", repo_path.display());
    println!("Branch: {branch}");

    let runner: Arc<dyn CommandRunner> = Arc::new(ProcessCommandRunner);
    let config_dir = tempfile::tempdir()?;
    let config = ConfigStore::with_base(config_dir.path());
    let factory_registry = FactoryRegistry::default_all();
    let host_bag = run_host_detectors(&default_host_detectors(), &*runner, &ProcessEnvVars).await;
    let host_registry = factory_registry.probe_all(&host_bag, &config, &ExecutionEnvironmentPath::new(&repo_path), runner.clone()).await;
    let host_vcs = host_registry.vcs.preferred().ok_or("no VCS provider discovered for reference repo")?;

    let (_, provider) =
        host_registry.environment_providers.select(EnvironmentKind::Docker, None).ok_or("Docker provider unavailable or ambiguous")?;
    let image = std::env::args().nth(3).ok_or("provide a pinned registry image as the third argument")?;

    // 1. Resolve the reference repo (.git common dir)
    let git_common_dir =
        host_vcs.read_repository(&repo_path, RepositoryRead::SharedMetadataDir).await.map_err(|e| format!("not a git repo: {e}"))?;
    let reference_repo = DaemonHostPath::new(std::fs::canonicalize(repo_path.join(git_common_dir.trim()))?);
    println!("Ref:    {reference_repo}");

    // 2. Ensure image (using a minimal image with git)
    println!("\n--- Ensuring image ---");
    let spec = EnvironmentSpec { image: ImageSource::Registry(image), token_env_vars: vec![] };
    let resource_spec = flotilla_core::providers::environment::legacy_environment_spec(&spec)?;
    let prepared = provider.prepare(&resource_spec, &Default::default()).await?;

    // 3. Create environment
    println!("\n--- Creating environment ---");
    let env_id = EnvironmentId::new("smoke-test");

    let opts = CreateOpts {
        tokens: vec![],
        working_directory: None,
        image_pull_policy: Default::default(),
        prepared_auth: Default::default(),
        tools: Vec::new(),
        provisioned_mounts: vec![flotilla_core::providers::environment::ProvisionedMount::new(
            reference_repo.as_path().to_path_buf(),
            "/ref/repo",
            flotilla_core::providers::environment::ProvisionedMountMode::Ro,
        )],
        cpu_limit: None,
        memory_policy: Default::default(),
    };
    let handle = provider.provision(env_id, &prepared, opts.into()).await?;
    let container = handle.container_name().unwrap_or("unknown");
    println!("Container: {container}");
    println!("Status:    {:?}", handle.status().await?);

    // Install git inside the container (debian:bookworm-slim doesn't have it)
    println!("\n--- Installing git in container ---");
    let env_runner = handle.runner();
    env_runner
        .run("sh", &["-c", "apt-get update -qq && apt-get install -y -qq git >/dev/null 2>&1"], Path::new("/"), &ChannelLabel::Default)
        .await?;
    println!("git installed");

    // 4. Discovery inside the container
    println!("\n--- Running discovery ---");
    let raw_vars = handle.env_vars().await?;
    let mut bag = EnvironmentBag::new();
    for (key, value) in &raw_vars {
        bag = bag.with(EnvironmentAssertion::env_var(key, value));
    }
    println!("Env vars: {} entries", raw_vars.len());
    println!("FLOTILLA_ENVIRONMENT_ID = {:?}", raw_vars.get("FLOTILLA_ENVIRONMENT_ID"));

    let env_repo_root = ExecutionEnvironmentPath::new("/workspace");
    let provider_registry = factory_registry.probe_all(&bag, &config, &env_repo_root, env_runner.clone()).await;

    let checkout_mgr = provider_registry.vcs.preferred();
    println!("Checkout manager: {}", checkout_mgr.map(|_| "found").unwrap_or("NONE"));

    if let Some((desc, _)) = provider_registry.vcs.preferred_with_desc() {
        println!("  backend: {}, impl: {}", desc.backend, desc.implementation);
    }

    // 5. Create checkout
    println!("\n--- Creating checkout for '{branch}' ---");
    match &checkout_mgr {
        Some(mgr) => match mgr.create_checkout(&branch, false).await {
            Ok((path, checkout)) => {
                println!("Checkout path:   {path}");
                println!("Checkout branch: {}", checkout.branch);

                // Verify files exist inside the container
                let ls_output = env_runner
                    .run("ls", &["-la"], path.as_path(), &ChannelLabel::Default)
                    .await
                    .unwrap_or_else(|e| format!("(ls failed: {e})"));
                println!("\n--- Contents of {path} ---");
                for line in ls_output.lines().take(15) {
                    println!("  {line}");
                }
            }
            Err(e) => println!("Checkout failed: {e}"),
        },
        None => println!("No checkout manager discovered — cannot create checkout"),
    }

    // 6. Cleanup
    println!("\n--- Destroying environment ---");
    handle.destroy().await?;
    println!("Done.");

    Ok(())
}
