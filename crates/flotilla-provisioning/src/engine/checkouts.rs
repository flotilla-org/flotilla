//! Checkouts on the engine (#3108): the request a checkout spec makes, the spec a stored request
//! shows its readers, and the [`CheckoutRecords`] port over the decided store.
//!
//! Each launch of a checkout is a resource of its own (`<checkout>#<n>`). A request while the
//! latest launch has not ended is served by that launch; a request once it has ended starts the
//! next launch. The Checkout controller writes every lifecycle edge after the request, and the
//! launch's facts with its Realise. Its integration is a plain record of the same name, written by
//! its home's inspection only when an inspected value changes.
//!
//! A launch held for a convoy is created under a change whose one item is that engine Convoy at
//! the version its requester read (#4024 ruling 1). The Checkout controller judges the holder
//! ended from the first entry after that version that ends the Convoy (Delivered or Abandoned),
//! read from the log rather than from the Convoy's current state.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use flotilla_engine_store::plain::{PendingChange, PlainLog};
use flotilla_resources::{
    checkout_records::{
        launched, CheckoutChange, CheckoutHolder, CheckoutRecord, CheckoutRecords, Inspection, Integration, Landed, HOLDER_ENDED,
        HOLDER_NOT_OPEN,
    },
    host_direct_environment_name, CheckoutSpec, CheckoutWorktreeSpec, FreshCloneCheckoutSpec, ObservedCheckoutSpec, RepositoryKey,
    HOST_DIRECT_NAME_PREFIX,
};
use flotilla_schema::{ChangeItem, Identity, Kind, NewChange, Ref, Timestamp};
use flotilla_schema_sqlite::{Read, StoreHandle};
use flotilla_state_machines::{
    machines::convoy::{Convoy as EngineConvoy, ConvoyLabel},
    Resource, StateValue,
};

use super::{
    launch_log::StagedFacts,
    reads::{indexed, keyed, latest, requested_at, view, ReadError, StoreReads, View},
    EngineError, Lifecycles,
};
use crate::stored::{
    checkout::{Checkout, CheckoutFacts, CheckoutIntegration, CheckoutKind, CheckoutRequest, LandedEvidence, Source},
    Holder,
};

/// The writer recorded for a home's inspection writes.
const INSPECTION: &str = "checkout-inspection";

/// The branches a repository's main checkout is on.
fn is_main_branch(branch: &str) -> bool {
    matches!(branch, "main" | "master" | "trunk")
}

/// The stored holder of `holder`.
pub fn holder_of(holder: &CheckoutHolder) -> Holder {
    match holder {
        CheckoutHolder::Convoy(convoy) => Holder::convoy(convoy),
    }
}

/// The engine Convoy `name` a checkout held for it is requested under: the one that is open, with
/// the version read. A convoy not held here, or one that has ended, holds no new checkout.
fn open_convoy(read: &mut Read<'_>, name: &str) -> Result<Result<(Identity, u64), String>, ReadError> {
    let code = <Resource<EngineConvoy> as Kind>::CODE;
    let mut ended = false;
    for id in read.resolve_named(code, name)? {
        let Some(loaded) = read.load::<Resource<EngineConvoy>>(id)? else { continue };
        if loaded.value.state().label() == ConvoyLabel::Open {
            return Ok(Ok((Identity { kind: code, name: loaded.name, home: loaded.home }, loaded.version)));
        }
        ended = true;
    }
    Ok(Err(if ended { format!("convoy {name} {HOLDER_ENDED}") } else { format!("convoy {name} {HOLDER_NOT_OPEN}") }))
}

/// The holder a stored `holder` names, if it is a checkout's.
pub(crate) fn checkout_holder(holder: &Holder) -> Option<CheckoutHolder> {
    let (kind, name) = holder.0.split_once(':')?;
    match kind {
        "convoy" => Some(CheckoutHolder::Convoy(name.to_owned())),
        _ => None,
    }
}

/// The request `spec` makes at `home` for a launch of checkout `checkout`, held for `holder`. An
/// observed spec is a checkout adopted where it is found, in its host's direct environment.
pub fn request_of(checkout: &str, spec: &CheckoutSpec, holder: Option<&CheckoutHolder>, home: &str) -> Result<CheckoutRequest, String> {
    fn local<K>(name: &str, home: &str) -> Ref<K> {
        Ref::new(name.to_owned(), home)
    }
    let holder = holder.map(holder_of);
    let request = match spec {
        CheckoutSpec::Worktree(spec) => CheckoutRequest {
            launch_of: checkout.to_owned(),
            launch: 0,
            repository: spec.repo_ref.to_string(),
            environment: local(&spec.env_ref, home),
            branch: spec.r#ref.clone(),
            base: spec.base_ref.clone(),
            path: spec.target_path.clone(),
            source: Source::Worktree,
            clone: Some(local(&spec.clone_ref, home)),
            holder,
        },
        CheckoutSpec::FreshClone(spec) => CheckoutRequest {
            launch_of: checkout.to_owned(),
            launch: 0,
            repository: spec.repo_ref.to_string(),
            environment: local(&spec.env_ref, home),
            branch: spec.r#ref.clone(),
            base: spec.base_ref.clone(),
            path: spec.target_path.clone(),
            source: Source::FreshClone { url: spec.url.clone() },
            clone: spec.source_clone_ref.as_deref().map(|name| local(name, home)),
            holder,
        },
        CheckoutSpec::Observed(spec) => CheckoutRequest {
            launch_of: checkout.to_owned(),
            launch: 0,
            repository: spec.repo_ref.to_string(),
            environment: local(&host_direct_environment_name(&spec.host_ref), home),
            branch: spec.r#ref.clone(),
            base: None,
            path: spec.path.clone(),
            source: Source::Adopted,
            clone: None,
            holder,
        },
    };
    request.validate()?;
    Ok(request)
}

/// The spec `request` shows its readers.
pub fn spec_of(request: &CheckoutRequest) -> CheckoutSpec {
    let repo_ref = RepositoryKey(request.repository.clone());
    match &request.source {
        Source::Worktree => CheckoutSpec::Worktree(CheckoutWorktreeSpec {
            repo_ref,
            env_ref: request.environment.name.clone(),
            r#ref: request.branch.clone(),
            base_ref: request.base.clone(),
            target_path: request.path.clone(),
            clone_ref: request.clone.as_ref().map(|clone| clone.name.clone()).unwrap_or_default(),
        }),
        Source::FreshClone { url } => CheckoutSpec::FreshClone(FreshCloneCheckoutSpec {
            source_clone_ref: request.clone.as_ref().map(|clone| clone.name.clone()),
            repo_ref,
            env_ref: request.environment.name.clone(),
            r#ref: request.branch.clone(),
            base_ref: request.base.clone(),
            target_path: request.path.clone(),
            url: url.clone(),
        }),
        Source::Adopted => CheckoutSpec::Observed(ObservedCheckoutSpec {
            r#ref: request.branch.clone(),
            path: request.path.clone(),
            repo_ref,
            host_ref: request.environment.name.strip_prefix(HOST_DIRECT_NAME_PREFIX).unwrap_or(&request.environment.name).to_owned(),
            is_main: is_main_branch(&request.branch),
        }),
    }
}

fn time(at: Timestamp) -> Option<DateTime<Utc>> {
    at.to_datetime()
}

/// The stored inspection `inspection` makes, failing since `failing_since`.
pub(crate) fn stored_integration(inspection: Inspection, failing_since: Option<Timestamp>) -> CheckoutIntegration {
    let details = |details: Vec<String>| (!details.is_empty()).then_some(details);
    CheckoutIntegration {
        failing_since,
        head_revision: inspection.head_revision,
        clean: inspection.clean,
        pushed: inspection.pushed,
        landed: inspection.landed,
        clean_details: details(inspection.clean_details),
        pushed_details: details(inspection.pushed_details),
        landed_details: details(inspection.landed_details),
        change_request: inspection.change_request,
        landed_evidence: inspection.landed_evidence.map(|landed| LandedEvidence {
            change_request: landed.change_request,
            merged_at: landed.merged_at.as_ref().map(Timestamp::from_datetime),
            target_ref: landed.target_ref,
            head_in_merged_head: landed.head_in_merged_head,
        }),
        remote_digest: inspection.remote_digest,
    }
}

/// The inspection a stored `integration` holds.
fn inspection_of(integration: &CheckoutIntegration) -> Inspection {
    Inspection {
        head_revision: integration.head_revision.clone(),
        clean: integration.clean,
        pushed: integration.pushed,
        landed: integration.landed,
        clean_details: integration.clean_details.clone().unwrap_or_default(),
        pushed_details: integration.pushed_details.clone().unwrap_or_default(),
        landed_details: integration.landed_details.clone().unwrap_or_default(),
        change_request: integration.change_request,
        landed_evidence: integration.landed_evidence.as_ref().map(|landed| Landed {
            change_request: landed.change_request,
            merged_at: landed.merged_at.and_then(time),
            target_ref: landed.target_ref.clone(),
            head_in_merged_head: landed.head_in_merged_head,
        }),
        remote_digest: integration.remote_digest.clone(),
    }
}

/// The [`CheckoutRecords`] port over one decided store: this home's checkouts through its
/// Checkout home, and every home's through keyed and indexed reads.
pub struct EngineCheckouts {
    lifecycles: Arc<dyn Lifecycles<CheckoutKind>>,
    reads: StoreReads,
    log: PlainLog,
    /// What each launch is created under, staged here for the launch log that creates it.
    inputs: StagedFacts<Vec<ChangeItem>>,
    homes: Arc<dyn Fn() -> Vec<String> + Send + Sync>,
    changes: tokio::sync::broadcast::Sender<CheckoutChange>,
}

/// How many changes a slow follower may fall behind by; one that lags rereads everything.
const CHANGES: usize = 256;

/// How often a request that loses a race to another requester of the same checkout reads again.
const REQUEST_ATTEMPTS: usize = 3;

impl EngineCheckouts {
    /// The port over `lifecycles`, whose launch log creates each launch under the items staged
    /// in `inputs` ([`super::launch_log::LaunchLog::with_inputs`]), reading the homes `homes` names.
    pub fn new(
        lifecycles: Arc<dyn Lifecycles<CheckoutKind>>,
        store: Arc<StoreHandle>,
        inputs: StagedFacts<Vec<ChangeItem>>,
        homes: impl Fn() -> Vec<String> + Send + Sync + 'static,
    ) -> Self {
        Self {
            lifecycles,
            log: PlainLog::new(Arc::clone(&store)),
            reads: StoreReads::new(store),
            inputs,
            homes: Arc::new(homes),
            changes: tokio::sync::broadcast::channel(CHANGES).0,
        }
    }

    /// Every launch `home` holds.
    fn scan(read: &mut Read<'_>, home: &str) -> Result<Vec<View<CheckoutKind>>, ReadError> {
        let mut views = Vec::new();
        let mut after = None;
        loop {
            let page = read.scan::<Resource<Checkout>>(home, after, 64)?;
            for id in page.ids {
                views.extend(view::<CheckoutKind>(read, id)?);
            }
            after = page.next;
            if after.is_none() {
                return Ok(views);
            }
        }
    }

    /// Follows the checkouts' records in `service`'s log, at every home, telling
    /// [`CheckoutRecords::changes`] of each. It runs at the store's commits.
    pub fn follow(&self, service: Arc<flotilla_schema_sqlite::stream::Service>) -> Result<tokio::task::JoinHandle<()>, String> {
        let mut watch =
            super::watch::EngineWatch::start(service, [<Resource<Checkout> as Kind>::CODE, CheckoutFacts::CODE, CheckoutIntegration::CODE])
                .map_err(|error| error.to_string())?;
        let changes = self.changes.clone();
        Ok(tokio::spawn(async move {
            loop {
                let followed = watch.next(|_, followed| Ok(followed.into_iter().map(checkout_change).collect::<Vec<_>>())).await;
                match followed {
                    // No follower is no reason to stop: one may subscribe later.
                    Ok(followed) => followed.into_iter().for_each(|change| drop(changes.send(change))),
                    Err(error) => tracing::warn!(%error, "the checkouts' changes could not be read from the log; retrying"),
                }
            }
        }))
    }

    fn now() -> Timestamp {
        Timestamp::from_datetime(&Utc::now())
    }

    fn change() -> PendingChange {
        PendingChange::new(NewChange::new(INSPECTION, Self::now()))
    }

    /// `view` as a reader sees it, with its launch's facts and its home's inspection.
    fn record(&self, read: &mut Read<'_>, view: View<CheckoutKind>) -> Result<CheckoutRecord, ReadError> {
        let degraded = match read.resolve(CheckoutFacts::CODE, &view.name, &view.home)? {
            Some(id) => read.load::<CheckoutFacts>(id)?.and_then(|loaded| loaded.value.degraded),
            None => None,
        };
        let integration = match read.resolve(CheckoutIntegration::CODE, &view.name, &view.home)? {
            Some(id) => match read.load::<CheckoutIntegration>(id)? {
                Some(loaded) => Some(self.integration(read, &view.home, &loaded.value)?),
                None => None,
            },
            None => None,
        };
        let requested_at = requested_at::<Resource<Checkout>>(read, &view.name, &view.home)?.and_then(time).unwrap_or(DateTime::UNIX_EPOCH);
        Ok(CheckoutRecord {
            requested_at,
            reading: view.judgement.reading(),
            spec: spec_of(&view.request),
            holder: view.request.holder.as_ref().and_then(checkout_holder),
            name: view.name,
            home: view.home,
            degraded,
            integration,
        })
    }

    /// `integration` homed at `home`, with when its values were last confirmed: since its home's
    /// inspections started failing, else when its home was last heard, which for this home is now.
    fn integration(&self, read: &mut Read<'_>, home: &str, integration: &CheckoutIntegration) -> Result<Integration, ReadError> {
        let confirmed = match integration.failing_since {
            Some(since) => since,
            None if home == self.home() => Self::now(),
            None => read.watermark(home)?.applied_through,
        };
        Ok(Integration {
            inspection: inspection_of(integration),
            confirmed_at: time(confirmed).unwrap_or(DateTime::UNIX_EPOCH),
            failing_since: integration.failing_since.and_then(time),
        })
    }

    fn records(
        &self,
        what: &str,
        views: impl FnOnce(&mut Read<'_>) -> Result<Vec<View<CheckoutKind>>, ReadError>,
    ) -> Result<Vec<CheckoutRecord>, String> {
        self.reads
            .read(|read| views(read)?.into_iter().map(|view| self.record(read, view)).collect())
            .map_err(|error| format!("engine checkouts {what}: {error}"))
    }

    /// One try at the launch that serves `request` for `checkout`; a launch it creates is created
    /// under `items`.
    fn serve(&self, checkout: &str, request: &CheckoutRequest, items: &[ChangeItem]) -> Result<String, EngineError> {
        // The items are staged under the checkout's name, which the create carries until its log
        // numbers the launch.
        let created = |create: &dyn Fn() -> Result<String, EngineError>| {
            if !items.is_empty() {
                self.inputs.stage(checkout, items.to_vec());
            }
            let created = create();
            // Taken by the create that ran; a create that did not run leaves nothing behind.
            self.inputs.take(checkout);
            created
        };
        match self.lifecycles.newest_launch(checkout) {
            Some(held) if held.state().live() && CheckoutRequest { launch: request.launch, ..held.spec().clone() } != *request => {
                Err(EngineError::Failed(format!("checkout {checkout} is held for another request ({})", held.name())))
            }
            Some(held) if held.state().live() => Ok(held.name().to_owned()),
            Some(held) => created(&|| self.lifecycles.replace_launch(checkout, held.name(), request.clone(), Self::now())),
            None => created(&|| self.lifecycles.request(checkout, request.clone(), Self::now()).map(|launch| launch.name().to_owned())),
        }
    }

    /// What a launch held for `holder` is created under: a convoy's open engine Convoy at the
    /// version read now. A launch held for no one carries no item.
    fn holder_items(&self, holder: Option<&CheckoutHolder>) -> Result<Vec<ChangeItem>, String> {
        let Some(CheckoutHolder::Convoy(convoy)) = holder else { return Ok(Vec::new()) };
        let (record, version) =
            self.reads.read(|read| open_convoy(read, convoy)).map_err(|error| format!("read convoy {convoy}: {error}"))??;
        Ok(vec![ChangeItem::new(record, Some(version.to_string()))])
    }

    /// This home's held inspection of `launch`, at its version.
    fn held_integration(&self, launch: &str) -> Result<Option<(u64, CheckoutIntegration)>, String> {
        if self.lifecycles.get(launch).is_none() {
            return Err(format!("checkout {launch} is not held at {}", self.home()));
        }
        let held = self.log.get::<CheckoutIntegration>(launch, self.home()).map_err(|error| error.to_string())?;
        Ok(held.map(|held| (held.version, held.spec)))
    }
}

impl CheckoutRecords for EngineCheckouts {
    fn cursor(&self) -> Option<Box<dyn flotilla_resources::lifecycle_reads::ChangeCursor<CheckoutChange>>> {
        let cursor = super::log_cursor::LogCursor::start(
            self.reads.clone(),
            Arc::clone(&self.homes),
            [<Resource<Checkout> as Kind>::CODE, CheckoutFacts::CODE, CheckoutIntegration::CODE],
            checkout_change,
        );
        match cursor {
            Ok(cursor) => Some(Box::new(cursor)),
            Err(error) => {
                tracing::warn!(%error, "the log could not be followed from here; readers read afresh");
                None
            }
        }
    }

    fn home(&self) -> &str {
        self.lifecycles.home()
    }

    fn changes(&self) -> tokio::sync::broadcast::Receiver<CheckoutChange> {
        self.changes.subscribe()
    }

    fn get(&self, name: &str, home: Option<&str>) -> Result<Option<CheckoutRecord>, String> {
        let home = home.unwrap_or(self.home()).to_owned();
        self.reads
            .read(|read| {
                let view = if launched(name) != name {
                    keyed::<CheckoutKind>(read, name, &home)?
                } else {
                    latest::<CheckoutKind>(read, name, &home)?
                };
                view.map(|view| self.record(read, view)).transpose()
            })
            .map_err(|error| format!("engine Checkout/{name}: {error}"))
    }

    fn held(&self, holder: &CheckoutHolder) -> Result<Vec<CheckoutRecord>, String> {
        let stored = holder_of(holder);
        self.records(&format!("held for {}", stored.0), |read| indexed(read, &CheckoutRequest::by_holder(&stored)))
    }

    fn of_repository(&self, repository: &RepositoryKey) -> Result<Vec<CheckoutRecord>, String> {
        self.records(&format!("of repository {repository}"), |read| indexed(read, &CheckoutRequest::by_repository(&repository.to_string())))
    }

    fn in_environment(&self, environment: &str, home: Option<&str>) -> Result<Vec<CheckoutRecord>, String> {
        let environment = Ref::new(environment.to_owned(), home.unwrap_or(self.home()));
        self.records(&format!("in environment {}", environment.name), |read| indexed(read, &CheckoutRequest::by_environment(&environment)))
    }

    fn here(&self) -> Result<Vec<CheckoutRecord>, String> {
        let home = self.home().to_owned();
        self.records(&format!("at {home}"), |read| Self::scan(read, &home))
    }

    fn every(&self) -> Result<Vec<CheckoutRecord>, String> {
        let homes = (self.homes)();
        self.records("at every home", |read| {
            let mut views = Vec::new();
            for home in &homes {
                views.extend(Self::scan(read, home)?);
            }
            Ok(views)
        })
    }

    fn request(&self, name: &str, spec: &CheckoutSpec, holder: Option<&CheckoutHolder>) -> Result<String, String> {
        if launched(name) != name {
            return Err(format!("checkout {name} is a launch's name, not a checkout's"));
        }
        let request = request_of(name, spec, holder, self.home())?;
        let items = self.holder_items(holder).map_err(|error| format!("request checkout {name}: {error}"))?;
        let mut last = None;
        for _ in 0..REQUEST_ATTEMPTS {
            match self.serve(name, &request, &items) {
                Ok(launch) => return Ok(launch),
                // Another requester launched it first: read again.
                Err(error @ (EngineError::Refused(_) | EngineError::Unknown | EngineError::Raced { .. })) => last = Some(error),
                Err(error) => return Err(format!("request checkout {name}: {error}")),
            }
        }
        Err(format!("request checkout {name}: {}", last.map(|error| error.to_string()).unwrap_or_default()))
    }

    fn inspected(&self, launch: &str, inspection: Inspection) -> Result<bool, String> {
        let written = match self.held_integration(launch)? {
            Some((version, held)) => {
                let next = stored_integration(inspection.latched(&inspection_of(&held)), None);
                if next == held {
                    return Ok(false);
                }
                self.log.edit(launch, version, &next, &Self::change())
            }
            None => self.log.create(launch, &stored_integration(inspection, None), &Self::change()),
        };
        written.map(|_| true).map_err(|error| format!("write the inspection of checkout {launch}: {error}"))
    }

    fn inspection_failed(&self, launch: &str) -> Result<(), String> {
        let written = match self.held_integration(launch)? {
            Some((_, held)) if held.failing_since.is_some() => return Ok(()),
            Some((version, held)) => {
                self.log.edit(launch, version, &CheckoutIntegration { failing_since: Some(Self::now()), ..held }, &Self::change())
            }
            // Nothing was ever confirmed: the record says only since when it has failed.
            None => self.log.create(launch, &stored_integration(Inspection::default(), Some(Self::now())), &Self::change()),
        };
        written.map(|_| ()).map_err(|error| format!("record the failed inspection of checkout {launch}: {error}"))
    }
}

/// The checkouts the next batch of followed log entries changed, at any home.
struct Changed {
    requests: Vec<CheckoutRequest>,
    /// The follower fell behind and rescanned: which checkouts changed is unknown, so every
    /// primary is woken.
    rescanned: bool,
}

async fn changed_requests(watch: &mut super::watch::EngineWatch) -> Result<Changed, String> {
    watch
        .next(|read, followed| {
            let mut changed = Changed { requests: Vec::new(), rescanned: false };
            for followed in followed {
                let identity = match followed {
                    flotilla_engine_store::follow::Followed::Changed(identity) => identity,
                    flotilla_engine_store::follow::Followed::Rescan(_) => {
                        changed.rescanned = true;
                        continue;
                    }
                };
                let Some(id) = read.resolve(<Resource<Checkout> as Kind>::CODE, &identity.name, &identity.home)? else { continue };
                if let Some(loaded) = read.load::<Resource<Checkout>>(id)? {
                    changed.requests.push(loaded.value.spec().clone());
                }
            }
            Ok(changed)
        })
        .await
        .map_err(|error| error.to_string())
}

fn start_watch(
    service: &Arc<flotilla_schema_sqlite::stream::Service>,
) -> Result<super::watch::EngineWatch, flotilla_resources::ResourceError> {
    super::watch::EngineWatch::start(Arc::clone(service), [<Resource<Checkout> as Kind>::CODE])
        .map_err(|error| flotilla_resources::ResourceError::other(error.to_string()))
}

/// Wakes a Repository's reconciler when a checkout of it changes: its checkouts by host are among
/// the repository's observations (#3108).
#[derive(Clone)]
pub struct CheckoutRepositoryWake {
    service: Arc<flotilla_schema_sqlite::stream::Service>,
}

impl CheckoutRepositoryWake {
    pub fn new(service: Arc<flotilla_schema_sqlite::stream::Service>) -> Self {
        Self { service }
    }
}

impl flotilla_store::controller::SecondaryWatch for CheckoutRepositoryWake {
    type Primary = flotilla_resources::Repository;

    fn clone_box(&self) -> Box<dyn flotilla_store::controller::SecondaryWatch<Primary = Self::Primary>> {
        Box::new(self.clone())
    }

    fn spawn(
        self: Box<Self>,
        _backend: flotilla_store::ResourceBackend,
        _namespace: String,
        sender: flotilla_store::controller::WorkQueueSender,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), flotilla_resources::ResourceError>> + Send>> {
        Box::pin(async move {
            let mut watch = start_watch(&self.service)?;
            loop {
                match changed_requests(&mut watch).await {
                    Ok(changed) => {
                        if changed.rescanned {
                            sender.send_every();
                        }
                        for request in changed.requests {
                            if sender.send(request.repository).await.is_err() {
                                return Ok(());
                            }
                        }
                    }
                    Err(error) => tracing::warn!(%error, "the checkout repository wake could not read the log; retrying"),
                }
            }
        })
    }
}

/// The old convoys whose engine launch is among `holders`: each generation by the launch it became.
async fn holder_convoys(
    backend: &flotilla_store::ResourceBackend,
    namespace: &str,
    holders: &std::collections::BTreeSet<String>,
) -> Result<std::collections::BTreeSet<String>, flotilla_resources::ResourceError> {
    Ok(backend
        .including_replicas::<flotilla_resources::Convoy>(namespace)
        .list()
        .await?
        .items
        .into_iter()
        .filter(|convoy| {
            flotilla_resources::convoy::engine::checkout_holder(&convoy.object)
                .is_some_and(|CheckoutHolder::Convoy(launch)| holders.contains(&launch))
        })
        .map(|convoy| convoy.object.metadata.name)
        .collect())
}

/// The convoy holders of the checkouts the next batch of followed log entries changed, and
/// whether the follower rescanned.
async fn changed_holders(watch: &mut super::watch::EngineWatch) -> Result<(std::collections::BTreeSet<String>, bool), String> {
    let changed = changed_requests(watch).await?;
    let holders = changed
        .requests
        .iter()
        .filter_map(|request| request.holder.as_ref().and_then(checkout_holder))
        .map(|CheckoutHolder::Convoy(convoy)| convoy)
        .collect();
    Ok((holders, changed.rescanned))
}

/// Wakes the convoy a checkout is held for when it changes: a convoy reads its checkouts'
/// inspections for the change requests its branches produce and for its landing (#3108).
#[derive(Clone)]
pub struct CheckoutConvoyWake {
    service: Arc<flotilla_schema_sqlite::stream::Service>,
}

impl CheckoutConvoyWake {
    pub fn new(service: Arc<flotilla_schema_sqlite::stream::Service>) -> Self {
        Self { service }
    }
}

impl flotilla_store::controller::SecondaryWatch for CheckoutConvoyWake {
    type Primary = flotilla_resources::Convoy;

    fn clone_box(&self) -> Box<dyn flotilla_store::controller::SecondaryWatch<Primary = Self::Primary>> {
        Box::new(self.clone())
    }

    fn spawn(
        self: Box<Self>,
        backend: flotilla_store::ResourceBackend,
        namespace: String,
        sender: flotilla_store::controller::WorkQueueSender,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), flotilla_resources::ResourceError>> + Send>> {
        Box::pin(async move {
            let mut watch = start_watch(&self.service)?;
            loop {
                let holders = match changed_holders(&mut watch).await {
                    Ok((holders, rescanned)) => {
                        if rescanned {
                            sender.send_every();
                        }
                        if holders.is_empty() {
                            continue;
                        }
                        holders
                    }
                    Err(error) => {
                        tracing::warn!(%error, "the checkout convoy wake could not read the log; retrying");
                        continue;
                    }
                };
                match holder_convoys(&backend, &namespace, &holders).await {
                    Ok(convoys) => {
                        for convoy in convoys {
                            if sender.send(convoy).await.is_err() {
                                return Ok(());
                            }
                        }
                    }
                    Err(error) => tracing::warn!(%error, "the checkout convoy wake could not read the convoys"),
                }
            }
        })
    }
}

/// What a followed write of the kinds tells readers.
fn checkout_change(followed: flotilla_engine_store::follow::Followed) -> CheckoutChange {
    match followed {
        flotilla_engine_store::follow::Followed::Changed(identity) => CheckoutChange::Changed(identity.name),
        flotilla_engine_store::follow::Followed::Rescan(_) => CheckoutChange::Rescanned,
    }
}

#[cfg(test)]
mod tests {
    use flotilla_resources::RepositoryKey;

    use super::*;

    fn observed(branch: &str) -> CheckoutSpec {
        CheckoutSpec::Observed(ObservedCheckoutSpec {
            r#ref: branch.into(),
            path: "/work/repo".into(),
            repo_ref: RepositoryKey("github.com/o/r".into()),
            host_ref: "kiwi".into(),
            is_main: is_main_branch(branch),
        })
    }

    #[test]
    fn an_adopted_checkout_is_its_hosts_and_derives_is_main() {
        for branch in ["main", "feature"] {
            let request = request_of(branch, &observed(branch), Some(&CheckoutHolder::Convoy("c".into())), "kiwi").expect("request");
            assert_eq!(request.source, Source::Adopted);
            assert_eq!(request.environment.name, "host-direct-kiwi");
            assert_eq!(request.holder, Some(Holder::convoy("c")));
            assert_eq!(spec_of(&request), observed(branch));
        }
    }

    #[test]
    fn a_holder_reads_back_as_it_was_given() {
        let holder = CheckoutHolder::Convoy("a:b".into());
        assert_eq!(checkout_holder(&holder_of(&holder)), Some(holder));
        assert_eq!(checkout_holder(&Holder::vessel("v")), None);
    }
}
