use std::{env, path::PathBuf, sync::Arc, time::Duration};

use flotilla_controllers::reconcilers::{
    HopChainContext, PresentationPolicyRegistry, PresentationReconciler, ProviderPresentationRuntime, TaskWorkspaceReconciler,
};
use flotilla_core::{path_context::DaemonHostPath, providers::registry::ProviderRegistry, HostName};
use flotilla_resources::{
    controller::ControllerLoop, ensure_crd, ensure_namespace, Convoy, ConvoyReconciler, HttpBackend, Presentation, ResourceBackend,
    TaskWorkspace, WorkflowTemplate,
};
use tracing::info;

fn kubeconfig_path() -> PathBuf {
    if let Ok(path) = env::var("KUBECONFIG") {
        return PathBuf::from(path);
    }
    let home = env::var("HOME").expect("HOME must be set when KUBECONFIG is unset");
    PathBuf::from(home).join(".kube/config")
}

fn parse_namespace() -> String {
    let mut args = env::args().skip(1);
    let mut namespace = "flotilla".to_string();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--namespace" => {
                namespace = args.next().expect("--namespace requires a value");
            }
            other => panic!("unexpected argument: {other}"),
        }
    }
    namespace
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt().with_target(false).init();

    let namespace = parse_namespace();
    let backend = HttpBackend::from_kubeconfig(kubeconfig_path())?;
    ensure_namespace(&backend, &namespace).await?;
    ensure_crd(&backend, include_str!("../src/crds/workflow_template.crd.yaml")).await?;
    ensure_crd(&backend, include_str!("../src/crds/convoy.crd.yaml")).await?;
    ensure_crd(&backend, include_str!("../src/crds/task_workspace.crd.yaml")).await?;
    ensure_crd(&backend, include_str!("../src/crds/presentation.crd.yaml")).await?;

    let backend = ResourceBackend::Http(backend);
    let convoys = backend.clone().using::<Convoy>(&namespace);
    let templates = backend.clone().using::<WorkflowTemplate>(&namespace);
    let task_workspaces = backend.clone().using::<TaskWorkspace>(&namespace);
    let presentations = backend.clone().using::<Presentation>(&namespace);
    let empty_registry = Arc::new(ProviderRegistry::new());
    let policies = Arc::new(PresentationPolicyRegistry::with_defaults());
    let config_base = DaemonHostPath::new(env::current_dir()?);

    info!("starting task-workspace, presentation, and convoy controller loops");
    tokio::try_join!(
        async {
            ControllerLoop {
                primary: task_workspaces,
                secondaries: TaskWorkspaceReconciler::secondary_watches(),
                reconciler: TaskWorkspaceReconciler::new(backend.clone(), &namespace),
                resync_interval: Duration::from_secs(60),
                backend: backend.clone(),
            }
            .run()
            .await
        },
        async {
            ControllerLoop {
                primary: presentations,
                secondaries: PresentationReconciler::<ProviderPresentationRuntime>::secondary_watches(),
                reconciler: PresentationReconciler::new(
                    Arc::new(ProviderPresentationRuntime::new(Arc::clone(&empty_registry), Arc::clone(&policies))),
                    backend.clone(),
                    &namespace,
                    {
                        let empty_registry = Arc::clone(&empty_registry);
                        HopChainContext::new("local", HostName::local(), config_base, move |_env_ref| Ok(Arc::clone(&empty_registry)))
                    },
                    policies,
                ),
                resync_interval: Duration::from_secs(60),
                backend: backend.clone(),
            }
            .run()
            .await
        },
        async {
            let convoy_backend = backend.clone();
            ControllerLoop {
                primary: convoys,
                secondaries: ConvoyReconciler::secondary_watches(),
                reconciler: ConvoyReconciler::new(templates)
                    .with_task_workspaces(convoy_backend.clone().using::<TaskWorkspace>(&namespace))
                    .with_presentations(convoy_backend.clone().using::<Presentation>(&namespace)),
                resync_interval: Duration::from_secs(60),
                backend: convoy_backend,
            }
            .run()
            .await
        }
    )?;

    Ok(())
}
