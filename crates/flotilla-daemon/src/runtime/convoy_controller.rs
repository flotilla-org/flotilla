//! The Convoy controller (#4412): the convoy reconciler's passes run as their own policy on the
//! engine's shared [`Runner`], not on the old ControllerLoop.
//!
//! It never lists the kind and has no resync tick. A convoy is passed:
//!
//! - at the start, and at a rescan of this home's log or a wake asking for every convoy: each
//!   open launch homed here, read from the Convoy home's memory;
//! - at a commit of its own record here;
//! - at a commit, followed in the same log, of one of its vessels' launches or of a session
//!   launch held for one of its crews ([`Policy::touches`]); every open launch here when another
//!   home's records are read afresh;
//! - when a wake names it: its checkouts' changes, the evidence its promises are observed from,
//!   the capacity it waits on, an operator's reconcile-now;
//! - at the time its last pass asked to look again, and after a failed pass at a backoff that
//!   doubles from a second to a minute.
//!
//! Each pass's span names its cause: the first wake since its last pass began, a watch by its
//! type's name.
//!
//! A convoy is read by its key for each pass, and at most one pass of it runs at a time: a wake
//! during a pass passes it once more when it ends. At most [`PASSES`] passes run at once; the
//! rest wait their turn, in name order, so a burst of convoys does not crowd the store out from
//! under message delivery.

use std::{
    collections::{BTreeMap, BTreeSet},
    ops::ControlFlow,
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use flotilla_crew::convoy::{session_launch_convoy, ConvoyReconciler};
use flotilla_engine_convoys::convoys::Convoys;
use flotilla_engine_store::follow::Followed;
use flotilla_provisioning::engine::controller::{homed_here, Policy, Runner, Trigger};
use flotilla_resources::{lifecycle_reads::LifecycleReads, Convoy, ResourceError};
use flotilla_schema::{Identity, KindCode, Timestamp};
use flotilla_store::load_metrics::{attributed, CallerReads};
use flotilla_store::{
    controller::{Reconciler, SecondaryWatch, WorkQueueSender},
    ResourceBackend, TypedResolver,
};
use futures::{stream::BoxStream, StreamExt};
use tracing::{info_span, warn, Instrument};

/// What reaches the Convoy controller from outside the log.
pub(super) enum ConvoyWake {
    /// A watch, named here, named a convoy.
    Named(String, &'static str),
    /// A watch, named here, asked for every convoy.
    Every(&'static str),
}

/// How many convoy passes run at once.
const PASSES: usize = 4;

/// What one pass found: when to look again without a change, or the pass's error; none when the
/// convoy is no longer held.
type Pass = Result<Option<Option<Duration>>, ResourceError>;

pub(super) struct ConvoyPolicy {
    reconciler: Arc<ConvoyReconciler>,
    convoys: TypedResolver<Convoy>,
    home: Arc<Convoys>,
    /// What a followed session or vessel launch is read through.
    reads: Arc<dyn LifecycleReads>,
    /// The session launches' and vessel launches' kinds, followed beside the convoys.
    terminals: Vec<KindCode>,
    vessels: Vec<KindCode>,
    /// Each convoy's failed passes in a row.
    failures: BTreeMap<String, u32>,
    /// The convoys woken while a pass of them ran: passed once more when it ends.
    again: BTreeSet<String>,
    /// The convoys woken while [`PASSES`] passes ran: passed as passes end.
    waiting: BTreeSet<String>,
    /// How many passes run.
    running: usize,
    /// Why each convoy is to be passed: the first cause since its last pass began.
    causes: BTreeMap<String, &'static str>,
}

impl ConvoyPolicy {
    pub(super) fn new(
        reconciler: ConvoyReconciler,
        convoys: TypedResolver<Convoy>,
        home: Arc<Convoys>,
        reads: Arc<dyn LifecycleReads>,
    ) -> Result<Self, String> {
        let (terminals, vessels) = (reads.terminals()?.codes(), reads.vessels()?.codes());
        Ok(Self {
            reconciler: Arc::new(reconciler),
            convoys,
            home,
            reads,
            terminals,
            vessels,
            failures: BTreeMap::new(),
            again: BTreeSet::new(),
            waiting: BTreeSet::new(),
            running: 0,
            causes: BTreeMap::new(),
        })
    }

    /// The kinds the runner follows beside the convoys.
    pub(super) fn followed(&self) -> Vec<KindCode> {
        self.terminals.iter().chain(&self.vessels).copied().collect()
    }

    /// The convoy homed here that changed launch `identity`, a session's or a vessel's, is held for.
    fn holder(&self, identity: &Identity) -> Result<Option<String>, String> {
        let here = self.home.home();
        if self.terminals.contains(&identity.kind) {
            return session_launch_convoy(self.reads.as_ref(), &identity.name, &identity.home, here);
        }
        let vessel = self.reads.vessels()?.get(&identity.name, Some(&identity.home))?;
        Ok(vessel.filter(|record| record.convoy_home == here).map(|record| record.convoy))
    }

    fn pass(&mut self, runner: &mut Runner<Pass>, name: &str) {
        self.running += 1;
        let (reconciler, convoys, owned) = (Arc::clone(&self.reconciler), self.convoys.clone(), name.to_owned());
        let clock = Arc::clone(runner.clock());
        let cause = self.causes.remove(name).unwrap_or("unknown");
        let span = info_span!("engine_controller.pass", kind = "Convoy", name, cause);
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

impl ConvoyPolicy {
    /// Passes each of `names` now, once its running pass ends, or when a pass is free.
    fn wake(&mut self, runner: &mut Runner<Pass>, names: impl IntoIterator<Item = String>) {
        for name in names {
            if runner.flying(&name) {
                self.again.insert(name);
            } else if self.running >= PASSES {
                self.waiting.insert(name);
            } else {
                self.waiting.remove(&name);
                runner.unschedule(&name);
                self.pass(runner, &name);
            }
        }
    }
}

/// A runner trigger as a pass's cause.
fn trigger_cause(trigger: Trigger) -> &'static str {
    match trigger {
        Trigger::Held => "held",
        Trigger::Own => "own commit",
        Trigger::Dependency => "dependency",
        Trigger::Retry => "retry",
        Trigger::Start => "start",
        Trigger::Recheck => "recheck",
    }
}

/// A watch's type name without its paths: `CapacityWake<Vessel>`.
fn short(name: &'static str) -> &'static str {
    let mut out = String::new();
    let mut ident = String::new();
    let mut chars = name.chars().peekable();
    while let Some(c) = chars.next() {
        if c == ':' && chars.peek() == Some(&':') {
            chars.next();
            ident.clear();
        } else if c.is_alphanumeric() || c == '_' {
            ident.push(c);
        } else {
            out.push_str(&ident);
            ident.clear();
            out.push(c);
        }
    }
    out.push_str(&ident);
    // A handful of watches, named once each as the controller starts.
    Box::leak(out.into_boxed_str())
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
    type Name = String;

    fn kind(&self) -> &'static str {
        "Convoy"
    }

    fn live(&self) -> Result<Vec<String>, String> {
        Ok(self.home.open_launches())
    }

    fn own(&self, identity: &Identity, home: &str) -> Option<String> {
        homed_here(identity, home)
    }

    fn touches(&self, followed: &Followed) -> Vec<String> {
        match followed {
            Followed::Changed(identity) if self.terminals.contains(&identity.kind) || self.vessels.contains(&identity.kind) => {
                match self.holder(identity) {
                    Ok(convoy) => convoy.into_iter().collect(),
                    Err(error) => {
                        warn!(launch = %identity.name, %error, "a changed launch's convoy could not be read; every convoy is passed");
                        self.home.open_launches()
                    }
                }
            }
            Followed::Changed(_) => Vec::new(),
            // Another home's sessions and vessels were read afresh, unseen one by one; this
            // home's rescan already passes every open launch.
            Followed::Rescan(home) if home != self.home.home() => self.home.open_launches(),
            Followed::Rescan(_) => Vec::new(),
        }
    }

    async fn evaluate(&mut self, runner: &mut Runner<Pass>, touched: BTreeMap<String, Trigger>) {
        for (name, trigger) in &touched {
            self.causes.entry(name.clone()).or_insert(trigger_cause(*trigger));
        }
        self.wake(runner, touched.into_keys());
    }

    async fn landed(&mut self, runner: &mut Runner<Pass>, name: String, pass: Pass) {
        self.running -= 1;
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
            self.waiting.insert(name);
        }
        // The next waiting convoy not already in flight takes the freed pass.
        if let Some(next) = self.waiting.iter().find(|waiting| !runner.flying(waiting.as_str())).cloned() {
            self.wake(runner, [next]);
        }
    }

    async fn woken(&mut self, runner: &mut Runner<Pass>, wake: ConvoyWake) -> ControlFlow<()> {
        let (touched, cause) = match wake {
            ConvoyWake::Named(name, watch) => (vec![name], watch),
            ConvoyWake::Every(watch) => (self.home.open_launches(), watch),
        };
        for name in &touched {
            self.causes.entry(name.clone()).or_insert(cause);
        }
        self.wake(runner, touched);
        ControlFlow::Continue(())
    }
}

/// The wakes `watches` send, each named by its watch, each watch run again a second after it
/// ends.
pub(super) fn wakes(
    watches: Vec<Box<dyn SecondaryWatch<Primary = Convoy>>>,
    backend: ResourceBackend,
    namespace: String,
    reads: Arc<CallerReads>,
) -> BoxStream<'static, ConvoyWake> {
    let streams = watches.into_iter().map(|watch| {
        let source = short(watch.name());
        let (sender, names) = WorkQueueSender::channel();
        let (backend, namespace, restart) = (backend.clone(), namespace.clone(), sender.clone());
        // A watch's reads count for the controller it wakes.
        let running = Running(vec![tokio::spawn(attributed(Arc::clone(&reads), async move {
            loop {
                if let Err(error) = watch.clone_box().spawn(backend.clone(), namespace.clone(), restart.clone()).await {
                    warn!(%error, watch = source, "a convoy wake watch ended; it starts again");
                }
                // The changes it missed meanwhile ask for every convoy.
                tokio::time::sleep(Duration::from_secs(1)).await;
                restart.send_every();
            }
        }))]);
        let taker = sender.clone();
        // The watch runs while the stream is held.
        let named = futures::stream::unfold((names, running), |(mut names, running)| async move {
            names.recv().await.map(|name| (name, (names, running)))
        })
        .map(move |name| {
            taker.take(&name);
            ConvoyWake::Named(name, source)
        });
        let every = futures::stream::unfold(sender, move |sender| async move {
            sender.every_requested().await;
            Some((ConvoyWake::Every(source), sender))
        });
        futures::stream::select(named, every).boxed()
    });
    futures::stream::select_all(streams).boxed()
}

/// The wake watches' tasks, stopped with the stream that holds them.
struct Running(Vec<tokio::task::JoinHandle<()>>);

impl Drop for Running {
    fn drop(&mut self) {
        self.0.iter().for_each(tokio::task::JoinHandle::abort);
    }
}
