//! This home's vessels run as the daemon runs them (#3108): the Vessel controller over the crew's
//! planner, told of its convoys' changes by their rechecks, and the crew seats on their wakes.

use std::sync::{Arc, OnceLock};

use flotilla_crew::reconcilers::vessel::{ConvoyVesselWake, CrewSeats, VesselPlanning};
use flotilla_provisioning::engine::{controller::Controller, vessel_actuator::VesselActuator, Lifecycles};
use flotilla_resources::{lifecycle_reads::LifecycleReads, SystemClock, Vessel};
use flotilla_schema_sqlite::stream::Service;
use flotilla_state_machines::machines::vessel::VesselKind;
use flotilla_store::{controller::ControllerLoop, ResourceBackend};
use flotilla_store_testkit::OldStoreAccounts;
use tokio::task::JoinHandle;

/// The tasks running this home's vessels, stopped when dropped.
pub struct RunningVessels {
    tasks: Vec<JoinHandle<()>>,
}

impl RunningVessels {
    /// The tasks, for a harness that stops its loops itself.
    pub fn into_tasks(mut self) -> Vec<JoinHandle<()>> {
        std::mem::take(&mut self.tasks)
    }
}

impl Drop for RunningVessels {
    fn drop(&mut self) {
        self.tasks.iter().for_each(JoinHandle::abort);
    }
}

/// Runs the vessels of `lifecycles`, on `service`'s store, with `planning`: their controller, its
/// rechecks from the vessels' convoys and their crews' bindings, and their crew seats over
/// `backend`'s vessel views in `namespace`.
pub fn run_vessels(
    backend: &ResourceBackend,
    namespace: &str,
    planning: VesselPlanning,
    lifecycles: Arc<dyn Lifecycles<VesselKind>>,
    reads: Arc<dyn LifecycleReads>,
    service: Arc<Service>,
) -> RunningVessels {
    let planning = Arc::new(planning);
    let wake =
        ConvoyVesselWake::new(backend, namespace, reads, Arc::new(OnceLock::new()), Arc::new(OldStoreAccounts::new(backend.clone())));
    let (rechecks, recheck_events) = tokio::sync::mpsc::channel(64);
    let controller =
        Controller::start(lifecycles, Arc::new(VesselActuator::new(Arc::clone(&planning) as _)), Arc::new(SystemClock), service)
            .expect("vessel controller");
    let mut secondaries = CrewSeats::secondary_watches();
    secondaries.push(Box::new(wake.clone()));
    let seats = ControllerLoop {
        primary: backend.clone().using::<Vessel>(namespace),
        secondaries,
        reconciler: CrewSeats::new(planning).with_rechecks(rechecks.clone()),
        backend: backend.clone(),
    };
    RunningVessels {
        tasks: vec![
            tokio::spawn(async move {
                // No probe observes a vessel: its holdings' commits and its rechecks look at it.
                let (_unobserved, backings) = tokio::sync::mpsc::channel(1);
                controller.run_with(backings, recheck_events).await
            }),
            tokio::spawn(wake.recheck(namespace.to_owned(), rechecks)),
            tokio::spawn(async move {
                let _ = seats.run().await;
            }),
        ],
    }
}
