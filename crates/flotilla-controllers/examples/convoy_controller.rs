use std::{env, path::PathBuf, time::Duration};

use flotilla_controllers::reconcilers::VesselReconciler;
use flotilla_resources::{Checkout, Convoy, Vessel, WorkflowTemplate};
use flotilla_store::{controller::ControllerLoop, ensure_crd, ensure_namespace, ConvoyReconciler, HttpBackend, ResourceBackend};
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
    ensure_crd(&backend, include_str!("../../flotilla-resources/src/crds/workflow_template.crd.yaml")).await?;
    ensure_crd(&backend, include_str!("../../flotilla-resources/src/crds/convoy.crd.yaml")).await?;
    ensure_crd(&backend, include_str!("../../flotilla-resources/src/crds/vessel.crd.yaml")).await?;

    let backend = ResourceBackend::Http(backend);
    let convoys = backend.clone().using::<Convoy>(&namespace);
    let templates = backend.definitions::<WorkflowTemplate>(&namespace);
    let vessels = backend.clone().using::<Vessel>(&namespace);

    info!("starting vessel and convoy controller loops");
    tokio::try_join!(
        async {
            ControllerLoop {
                primary: vessels,
                secondaries: VesselReconciler::secondary_watches(),
                reconciler: VesselReconciler::new(backend.clone(), &namespace),
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
                    .with_vessels(convoy_backend.clone().using::<Vessel>(&namespace))
                    .with_checkouts(convoy_backend.clone().using::<Checkout>(&namespace)),
                resync_interval: Duration::from_secs(60),
                backend: convoy_backend,
            }
            .run()
            .await
        }
    )?;

    Ok(())
}
