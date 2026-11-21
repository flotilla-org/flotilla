//! The Convoy controller (#4412): the convoy reconciler's passes run as their own policy on the
//! engine's shared [`Runner`], not on the old ControllerLoop.
//!
//! It never lists the kind and has no resync tick. A convoy is passed:
//!
//! - at the start, and at a rescan of this home's log or a wake asking for every convoy: each
//!   open launch homed here, read from the Convoy home's memory;
//! - at a commit of its own record here;
//! - when a wake names it: its vessels', checkouts' and sessions' changes, the evidence its
//!   promises are observed from, the capacity it waits on, an operator's reconcile-now;
//! - at the time its last pass asked to look again, and after a failed pass at a backoff that
//!   doubles from a second to a minute.
//!
//! A convoy is read by its key for each pass, and at most one pass of it runs at a time: a wake
//! during a pass passes it once more when it ends.

use std::{
    collections::{BTreeMap, BTreeSet},
    ops::ControlFlow,
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use flotilla_crew::convoy::ConvoyReconciler;
use flotilla_engine_convoys::convoys::Convoys;
use flotilla_provisioning::engine::controller::{Policy, Runner, Trigger};
use flotilla_resources::{Convoy, ResourceError};
use flotilla_schema::Timestamp;
use flotilla_store::{
    controller::{Reconciler, SecondaryWatch, WorkQueueSender},
    ResourceBackend, TypedResolver,
};
use futures::{stream::BoxStream, StreamExt};
use tracing::{info_span, warn, Instrument};

/// What reaches the Convoy controller from outside the log.
pub(super) enum ConvoyWake {
    /// A watch named a convoy.
    Named(String),
    /// A watch asked for every convoy.
    Every,
}

/// What one pass found: when to look again without a change, or the pass's error; none when the
/// convoy is no longer held.
type Pass = Result<Option<Option<Duration>>, ResourceError>;

pub(super) struct ConvoyPolicy {
    reconciler: Arc<ConvoyReconciler>,
    convoys: TypedResolver<Convoy>,
    home: Arc<Convoys>,
    /// Each convoy's failed passes in a row.
    failures: BTreeMap<String, u32>,
    /// The convoys woken while a pass of them ran: passed once more when it ends.
    again: BTreeSet<String>,
}

impl ConvoyPolicy {
    pub(super) fn new(reconciler: ConvoyReconciler, convoys: TypedResolver<Convoy>, home: Arc<Convoys>) -> Self {
        Self { reconciler: Arc::new(reconciler), convoys, home, failures: BTreeMap::new(), again: BTreeSet::new() }
    }

    fn pass(&self, runner: &mut Runner<Pass>, name: &str) {
        let (reconciler, convoys, owned) = (Arc::clone(&self.reconciler), self.convoys.clone(), name.to_owned());
        let clock = Arc::clone(runner.clock());
        let span = info_span!("engine_controller.pass", kind = "Convoy", name);
        runner.spawn(
            name,
            async move {
                let convoy = match convoys.get(&owned).await {
                    Ok(convoy) => convoy,
                    Err(ResourceError::NotFound { .. }) => return Ok(None),
                    Err(error) => return Err(error),
                };
                let prepared = reconciler.prepare(&convoy).await?;
                Ok(Some(reconciler.reconcile(&convoy, &prepared, clock.now()).requeue_after))
            }
            .instrument(span),
        );
    }
}

/// The wait before passing a convoy again after `failures` failed passes in a row.
fn backoff(failures: u32) -> Duration {
    Duration::from_secs(1).saturating_mul(1_u32 << failures.saturating_sub(1).min(6)).min(Duration::from_secs(60))
}

/// `after` from now on `runner`'s clock.
fn later(runner: &Runner<Pass>, after: Duration) -> Timestamp {
    Timestamp::from_datetime(&(runner.clock().now() + chrono::Duration::from_std(after).unwrap_or(chrono::Duration::MAX)))
}

#[async_trait]
impl Policy for ConvoyPolicy {
    type Done = Pass;
    type Wake = ConvoyWake;

    fn kind(&self) -> &'static str {
        "Convoy"
    }

    fn live(&self) -> Result<Vec<String>, String> {
        Ok(self.home.open_launches())
    }

    async fn evaluate(&mut self, runner: &mut Runner<Pass>, touched: BTreeMap<String, Trigger>) {
        for name in touched.into_keys() {
            if runner.flying(&name) {
                self.again.insert(name);
            } else {
                runner.unschedule(&name);
                self.pass(runner, &name);
            }
        }
    }

    async fn landed(&mut self, runner: &mut Runner<Pass>, name: String, pass: Pass) {
        match pass {
            Ok(None) => {
                self.failures.remove(&name);
                runner.forget(&name);
            }
            Ok(Some(after)) => {
                self.failures.remove(&name);
                if let Some(after) = after {
                    let at = later(runner, after);
                    runner.retry_at(&name, at);
                }
            }
            Err(error) => {
                let failures = self.failures.entry(name.clone()).or_default();
                *failures += 1;
                warn!(convoy = %name, failures = *failures, %error, "convoy pass failed");
                let at = later(runner, backoff(*failures));
                runner.retry_at(&name, at);
            }
        }
        if self.again.remove(&name) {
            self.evaluate(runner, BTreeMap::from([(name, Trigger::Own)])).await;
        }
    }

    async fn woken(&mut self, runner: &mut Runner<Pass>, wake: ConvoyWake) -> ControlFlow<()> {
        let touched = match wake {
            ConvoyWake::Named(name) => vec![name],
            ConvoyWake::Every => self.home.open_launches(),
        };
        self.evaluate(runner, touched.into_iter().map(|name| (name, Trigger::Own)).collect()).await;
        ControlFlow::Continue(())
    }
}

/// The wakes `watches` send, each watch run again a second after it ends.
pub(super) fn wakes(
    watches: Vec<Box<dyn SecondaryWatch<Primary = Convoy>>>,
    backend: ResourceBackend,
    namespace: String,
) -> BoxStream<'static, ConvoyWake> {
    let (sender, names) = WorkQueueSender::channel();
    let mut running = Running(Vec::new());
    for watch in watches {
        let (backend, namespace, sender) = (backend.clone(), namespace.clone(), sender.clone());
        running.0.push(tokio::spawn(async move {
            loop {
                if let Err(error) = watch.clone_box().spawn(backend.clone(), namespace.clone(), sender.clone()).await {
                    warn!(%error, "a convoy wake watch ended; it starts again");
                }
                // The changes it missed meanwhile ask for every convoy.
                tokio::time::sleep(Duration::from_secs(1)).await;
                sender.send_every();
            }
        }));
    }
    let taker = sender.clone();
    // The watches run while the stream is held.
    let named =
        futures::stream::unfold(
            (names, running),
            |(mut names, running)| async move { names.recv().await.map(|name| (name, (names, running))) },
        )
        .map(move |name| {
            taker.take(&name);
            ConvoyWake::Named(name)
        });
    let every = futures::stream::unfold(sender, |sender| async move {
        sender.every_requested().await;
        Some((ConvoyWake::Every, sender))
    });
    futures::stream::select(named, every).boxed()
}

/// The wake watches' tasks, stopped with the stream that holds them.
struct Running(Vec<tokio::task::JoinHandle<()>>);

impl Drop for Running {
    fn drop(&mut self) {
        self.0.iter().for_each(tokio::task::JoinHandle::abort);
    }
}
