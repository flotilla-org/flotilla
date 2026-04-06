//! Smoke-test for environment provisioning: create a Docker container,
//! run discovery inside it, and perform a git clone --reference checkout.
//!
//! Requires Docker and a local git repo with a remote.
//!
//! Usage:
//!   cargo run -p flotilla-core --example environment_checkout -- /path/to/repo [branch]
//!
//! Example:
//!   cargo run -p flotilla-core --example environment_checkout -- . main

use std::{path::Path, sync::Arc};

use flotilla_core::{
    config::ConfigStore,
    path_context::ExecutionEnvironmentPath,
    providers::{
        discovery::{EnvironmentAssertion, EnvironmentBag, FactoryRegistry},
        environment::{docker::DockerEnvironmentProvider, CreateOpts, EnvironmentProvider},
        ChannelLabel, CommandRunner, ProcessCommandRunner,
    },
};
use flotilla_protocol::{DaemonHostPath, EnvironmentId, EnvironmentSpec, ImageSource};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let repo_path = std::env::args().nth(1).unwrap_or_else(|| ".".to_string());
    let branch = std::env::args().nth(2).unwrap_or_else(|| "main".to_string());

    let repo_path = std::fs::canonicalize(&repo_path)?;
    println!("Repo:   {}", repo_path.display());
    println!("Branch: {branch}");

    let runner: Arc<dyn CommandRunner> = Arc::new(ProcessCommandRunner);
    let provider = DockerEnvironmentProvider::new(runner.clone());

    // 1. Resolve the reference repo (.git common dir)
    let git_common_dir = runner
        .run("git", &["rev-parse", "--git-common-dir"], &repo_path, &ChannelLabel::Noop)
        .await
        .map_err(|e| format!("not a git repo: {e}"))?;
    let reference_repo = DaemonHostPath::new(std::fs::canonicalize(repo_path.join(git_common_dir.trim()))?);
    println!("Ref:    {reference_repo}");

    // 2. Ensure image (using a minimal image with git)
    println!("\n--- Ensuring image ---");
    let spec = EnvironmentSpec { image: ImageSource::Registry("debian:bookworm-slim".to_string()), token_env_vars: vec![] };
    let image = provider.ensure_image(&spec, &repo_path).await?;
    println!("Image:  {image}");

    // 3. Create environment
    println!("\n--- Creating environment ---");
    let env_id = EnvironmentId::new("smoke-test");

    // We need git inside the container — install it after creation
    // Use a temp socket path (we don't need a real daemon socket for this test)
    let temp = tempfile::tempdir()?;
    let socket_path = DaemonHostPath::new(temp.path().join("fake.sock"));
    // Create a dummy socket file so the mount doesn't fail
    std::fs::write(socket_path.as_path(), "")?;

    let opts = CreateOpts {
        tokens: vec![],
        daemon_socket_path: socket_path,
        working_directory: None,
        provisioned_mounts: vec![flotilla_core::providers::environment::ProvisionedMount::new(
            reference_repo.as_path().to_path_buf(),
            "/ref/repo",
        )],
    };
    let handle = provider.create(env_id, &image, opts).await?;
    let container = handle.container_name().unwrap_or("unknown");
    println!("Container: {container}");
    println!("Status:    {:?}", handle.status().await?);

    // Install git inside the container (debian:bookworm-slim doesn't have it)
    println!("\n--- Installing git in container ---");
    let env_runner = handle.runner();
    env_runner
        .run("sh", &["-c", "apt-get update -qq && apt-get install -y -qq git >/dev/null 2>&1"], Path::new("/"), &ChannelLabel::Noop)
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

    let config_dir = tempfile::tempdir()?;
    let config = ConfigStore::with_base(config_dir.path());
    let env_repo_root = ExecutionEnvironmentPath::new("/workspace");
    let factory_registry = FactoryRegistry::default_all();
    let provider_registry = factory_registry.probe_all(&bag, &config, &env_repo_root, env_runner.clone()).await;

    let checkout_mgr = provider_registry.checkout_managers.preferred();
    println!("Checkout manager: {}", checkout_mgr.map(|_| "found").unwrap_or("NONE"));

    if let Some((desc, _)) = provider_registry.checkout_managers.preferred_with_desc() {
        println!("  backend: {}, impl: {}", desc.backend, desc.implementation);
    }

    // 5. Create checkout
    println!("\n--- Creating checkout for '{branch}' ---");
    match &checkout_mgr {
        Some(mgr) => match mgr.create_checkout(&env_repo_root, &branch, false).await {
            Ok((path, checkout)) => {
                println!("Checkout path:   {path}");
                println!("Checkout branch: {}", checkout.branch);

                // Verify files exist inside the container
                let ls_output = env_runner
                    .run("ls", &["-la"], path.as_path(), &ChannelLabel::Noop)
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
