//! Stage a real contained Codex launch through the production adapter.
//! Usage: cargo run -p flotilla-agent --example stage_codex_crew -- /absolute/probe-root /absolute/codex [--restricted]
use std::{path::PathBuf, sync::Arc};

use flotilla_agent::adapter::{AgentAdapterRegistry, AgentLaunchRequest};
use flotilla_discovery_api::{EnvironmentAssertion, EnvironmentBag};
use flotilla_paths::path_context::ExecutionEnvironmentPath;
use flotilla_provider_runtime::ProcessCommandRunner;
use flotilla_resources::TerminalBrief;

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let root = PathBuf::from(args.next().expect("probe root"));
    assert!(root.is_absolute());
    let binary = args.next().expect("absolute Codex binary");
    let restricted = match args.next().as_deref() {
        None => false,
        Some("--restricted") => true,
        Some(other) => panic!("unknown option {other}; expected --restricted"),
    };
    assert!(args.next().is_none(), "unexpected trailing argument");
    let home = root.join("home");
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(workspace.join(".flotilla/briefs")).unwrap();
    let env = EnvironmentBag::new()
        .with(EnvironmentAssertion::env_var("CODEX_HOME", home.display().to_string()))
        .with(EnvironmentAssertion::binary("codex", binary));
    let registry = AgentAdapterRegistry::discover(&env, Arc::new(ProcessCommandRunner));
    let adapter = registry.get("codex").expect("Codex adapter");
    let environment =
        vec![("FLOTILLA_ENVIRONMENT_ID".into(), "codex-3823-probe".into()), ("CODEX_HOME".into(), home.display().to_string())];
    let brief = TerminalBrief {
        path: ".flotilla/briefs/coder.md".into(),
        content: "Run the shell command `printf hook-probe-3823` exactly once, then reply done.".into(),
        artifact_digest: None,
        copies: vec![],
    };
    adapter.prepare_with_environment(&ExecutionEnvironmentPath::new(&workspace), &brief, &environment).await.unwrap();
    let plan = adapter
        .launch(&AgentLaunchRequest {
            role: "coder".into(),
            model: None,
            brief,
            environment,
            fulfilment_grants: restricted.then(Default::default),
        })
        .unwrap();
    std::fs::write(
        root.join("launch.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "command": plan.command, "env": plan.env, "cwd": workspace,
        }))
        .unwrap(),
    )
    .unwrap();
    std::fs::write(root.join("command.sh"), plan.command).unwrap();
    println!("{}", root.display());
}
