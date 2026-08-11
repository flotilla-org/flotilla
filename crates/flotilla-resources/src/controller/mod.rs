use std::{
    collections::{BTreeMap, HashSet, VecDeque},
    future::Future,
    marker::PhantomData,
    pin::Pin,
    sync::Arc,
    time::Duration,
};

use chrono::{DateTime, Utc};
use futures::StreamExt;
use tokio::{
    sync::{mpsc, Mutex},
    task::JoinHandle,
    time::Instant,
};
use tracing::warn;

use crate::{
    apply_status_patch,
    backend::{ReplicaReadResolver, ResourceBackend, TypedResolver},
    checkout::CheckoutSpec,
    clone::CloneSpec,
    environment::EnvironmentSpec,
    error::ResourceError,
    labels::LifecycleAuthority,
    presentation::PresentationSpec,
    resource::{InputMeta, Resource, ResourceObject},
    terminal_session::TerminalSessionSpec,
    vessel::VesselSpec,
    watch::{WatchEvent, WatchStart},
};

pub type Event = String;

#[allow(async_fn_in_trait)]
pub trait Reconciler: Send + Sync + 'static {
    type Resource: Resource;
    type Dependencies;

    /// Gather or prepare the dependency state needed for `reconcile()`.
    ///
    /// Reconcilers may use this hook for idempotent preparation work when the
    /// dependency result depends on performing that step first.
    async fn fetch_dependencies(&self, obj: &ResourceObject<Self::Resource>) -> Result<Self::Dependencies, ResourceError>;

    fn reconcile(
        &self,
        obj: &ResourceObject<Self::Resource>,
        deps: &Self::Dependencies,
        now: DateTime<Utc>,
    ) -> ReconcileOutcome<Self::Resource>;

    async fn run_finalizer(&self, obj: &ResourceObject<Self::Resource>) -> Result<(), ResourceError>;

    fn finalizer_name(&self) -> Option<&'static str>;

    /// Opt an object reconciler into bounded retries for repeated identical
    /// errors. Controllers without a policy retain the ordinary resync retry
    /// behaviour.
    fn reconcile_error_policy(&self) -> Option<ReconcileErrorPolicy> {
        None
    }

    /// Persist a resource-specific degraded condition once the reconcile
    /// error budget is exhausted.
    fn reconcile_degraded_patch(
        &self,
        _obj: &ResourceObject<Self::Resource>,
        _failure: &ReconcileFailure,
    ) -> Option<<Self::Resource as Resource>::StatusPatch> {
        None
    }

    /// A persisted degraded condition survives controller restarts and parks
    /// the object until an explicit resource transition clears it.
    fn is_reconcile_degraded(&self, _obj: &ResourceObject<Self::Resource>) -> bool {
        false
    }

    /// Allow a lifecycle change to wake an otherwise parked degraded object.
    async fn degraded_object_needs_reconcile(&self, _obj: &ResourceObject<Self::Resource>) -> Result<bool, ResourceError> {
        Ok(false)
    }

    /// Map a finalizer failure to a status update.
    ///
    /// Other object-scoped errors remain isolated and are retried on a later
    /// event or resync without forcing the resource into a terminal state.
    fn finalizer_error_patch(
        &self,
        _obj: &ResourceObject<Self::Resource>,
        _error: &ResourceError,
    ) -> Option<<Self::Resource as Resource>::StatusPatch> {
        None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReconcileErrorPolicy {
    pub max_consecutive_failures: u32,
    pub initial_backoff: Duration,
    pub max_backoff: Duration,
}

impl ReconcileErrorPolicy {
    fn backoff(&self, consecutive_failures: u32) -> Duration {
        let exponent = consecutive_failures.saturating_sub(1).min(31);
        self.initial_backoff.saturating_mul(1_u32 << exponent).min(self.max_backoff)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconcileFailure {
    /// Stable error identity for one object and failure cause. Reconcilers
    /// opting into a budget should avoid errors containing volatile data.
    pub message: String,
    pub consecutive_failures: u32,
}

struct ObjectFailure {
    failure: ReconcileFailure,
    creation_timestamp: DateTime<Utc>,
    retry_at: Instant,
    terminal: bool,
}

pub struct ReconcileOutcome<T: Resource> {
    pub patch: Option<T::StatusPatch>,
    pub actuations: Vec<Actuation>,
    pub events: Vec<Event>,
    pub requeue_after: Option<Duration>,
}

impl<T: Resource> ReconcileOutcome<T> {
    pub fn new(patch: Option<T::StatusPatch>) -> Self {
        Self { patch, actuations: Vec::new(), events: Vec::new(), requeue_after: None }
    }

    pub fn with_actuations(patch: Option<T::StatusPatch>, actuations: Vec<Actuation>) -> Self {
        Self { patch, actuations, events: Vec::new(), requeue_after: None }
    }
}

#[derive(Debug, Clone)]
pub enum Actuation {
    CreateRepository { key: crate::RepositoryKey, spec: crate::RepositorySpec },
    CreateEnvironment { meta: InputMeta, spec: EnvironmentSpec },
    CreateClone { meta: InputMeta, spec: CloneSpec },
    RetryClone { name: String, failed_at: DateTime<Utc> },
    CreateCheckout { meta: InputMeta, spec: CheckoutSpec },
    CreateTerminalSession { meta: InputMeta, spec: TerminalSessionSpec },
    RestartTerminalSession { name: String },
    DeleteTerminalSession { name: String },
    CreateVessel { meta: InputMeta, spec: VesselSpec },
    CreatePresentation { meta: InputMeta, spec: PresentationSpec },
    DeletePresentation { name: String },
    DeleteVessel { name: String },
    DeleteCheckout { name: String },
}

pub trait SecondaryWatch: Send + Sync {
    type Primary: Resource;

    fn clone_box(&self) -> Box<dyn SecondaryWatch<Primary = Self::Primary>>;

    fn spawn(
        self: Box<Self>,
        backend: ResourceBackend,
        namespace: String,
        sender: mpsc::Sender<String>,
    ) -> Pin<Box<dyn Future<Output = Result<(), ResourceError>> + Send>>;
}

impl<P: Resource> Clone for Box<dyn SecondaryWatch<Primary = P>> {
    fn clone(&self) -> Self {
        self.clone_box()
    }
}

#[derive(Clone)]
pub struct LabelMappedWatch<W: Resource, P: Resource> {
    pub label_key: &'static str,
    pub _marker: PhantomData<(W, P)>,
}

impl<W: Resource, P: Resource> LabelMappedWatch<W, P> {
    async fn enqueue_from_object(
        label_key: &'static str,
        sender: &mpsc::Sender<String>,
        object: &ResourceObject<W>,
    ) -> Result<(), ResourceError> {
        if let Some(primary) = object.metadata.labels.get(label_key) {
            sender
                .send(primary.clone())
                .await
                .map_err(|_| ResourceError::other("controller queue closed while forwarding secondary event"))?;
        }
        Ok(())
    }
}

impl<W: Resource, P: Resource> SecondaryWatch for LabelMappedWatch<W, P> {
    type Primary = P;

    fn clone_box(&self) -> Box<dyn SecondaryWatch<Primary = Self::Primary>> {
        Box::new(Self { label_key: self.label_key, _marker: PhantomData })
    }

    fn spawn(
        self: Box<Self>,
        backend: ResourceBackend,
        namespace: String,
        sender: mpsc::Sender<String>,
    ) -> Pin<Box<dyn Future<Output = Result<(), ResourceError>> + Send>> {
        Box::pin(async move {
            let resolver = backend.using::<W>(&namespace);
            let listed = resolver.list().await?;
            for object in &listed.items {
                Self::enqueue_from_object(self.label_key, &sender, object).await?;
            }

            let mut watch = resolver.watch(WatchStart::resuming_from(&listed)).await?;
            while let Some(event) = watch.next().await {
                match event? {
                    WatchEvent::Added(object) | WatchEvent::Modified(object) | WatchEvent::Deleted(object) => {
                        Self::enqueue_from_object(self.label_key, &sender, &object).await?;
                    }
                    WatchEvent::DeletedByName(_) => {}
                }
            }
            Ok(())
        })
    }
}

#[derive(Clone)]
pub struct ResolverLabelMappedWatch<W: Resource, P: Resource> {
    pub label_key: &'static str,
    pub resolver: TypedResolver<W>,
    pub _marker: PhantomData<P>,
}

impl<W: Resource, P: Resource> SecondaryWatch for ResolverLabelMappedWatch<W, P> {
    type Primary = P;

    fn clone_box(&self) -> Box<dyn SecondaryWatch<Primary = Self::Primary>> {
        Box::new(Self { label_key: self.label_key, resolver: self.resolver.clone(), _marker: PhantomData })
    }

    fn spawn(
        self: Box<Self>,
        _backend: ResourceBackend,
        _namespace: String,
        sender: mpsc::Sender<String>,
    ) -> Pin<Box<dyn Future<Output = Result<(), ResourceError>> + Send>> {
        Box::pin(async move {
            let listed = self.resolver.list().await?;
            for object in &listed.items {
                LabelMappedWatch::<W, P>::enqueue_from_object(self.label_key, &sender, object).await?;
            }
            let mut watch = self.resolver.watch(WatchStart::resuming_from(&listed)).await?;
            while let Some(event) = watch.next().await {
                match event? {
                    WatchEvent::Added(object) | WatchEvent::Modified(object) | WatchEvent::Deleted(object) => {
                        LabelMappedWatch::<W, P>::enqueue_from_object(self.label_key, &sender, &object).await?;
                    }
                    WatchEvent::DeletedByName(_) => {}
                }
            }
            Ok(())
        })
    }
}

#[derive(Clone)]
pub struct ReplicaLabelMappedWatch<W: Resource, P: Resource> {
    pub label_key: &'static str,
    pub resolver: ReplicaReadResolver<W>,
    pub _marker: PhantomData<P>,
}

impl<W: Resource, P: Resource> SecondaryWatch for ReplicaLabelMappedWatch<W, P> {
    type Primary = P;

    fn clone_box(&self) -> Box<dyn SecondaryWatch<Primary = Self::Primary>> {
        Box::new(Self { label_key: self.label_key, resolver: self.resolver.clone(), _marker: PhantomData })
    }

    fn spawn(
        self: Box<Self>,
        _backend: ResourceBackend,
        _namespace: String,
        sender: mpsc::Sender<String>,
    ) -> Pin<Box<dyn Future<Output = Result<(), ResourceError>> + Send>> {
        Box::pin(async move {
            let mut watch = self.resolver.watch().await?;
            let listed = self.resolver.list().await?;
            for source in &listed.items {
                LabelMappedWatch::<W, P>::enqueue_from_object(self.label_key, &sender, &source.object).await?;
            }
            while let Some(event) = watch.next().await {
                let source = match event? {
                    crate::ReadWatchEvent::Added(source)
                    | crate::ReadWatchEvent::Modified(source)
                    | crate::ReadWatchEvent::Deleted(source) => source,
                    crate::ReadWatchEvent::DeletedByName { .. } => continue,
                };
                LabelMappedWatch::<W, P>::enqueue_from_object(self.label_key, &sender, &source.object).await?;
            }
            Ok(())
        })
    }
}

/// Watch federated convoys and enqueue every checkout they currently claim.
#[derive(Clone)]
pub struct ReplicaConvoyCheckoutWatch {
    pub resolver: ReplicaReadResolver<crate::Convoy>,
}

impl ReplicaConvoyCheckoutWatch {
    async fn enqueue_checkouts(sender: &mpsc::Sender<String>, convoy: &ResourceObject<crate::Convoy>) -> Result<(), ResourceError> {
        for checkout_name in crate::expected_checkout_refs(convoy).unwrap_or_default() {
            sender
                .send(checkout_name)
                .await
                .map_err(|_| ResourceError::other("controller queue closed while forwarding federated convoy checkout event"))?;
        }
        Ok(())
    }
}

impl SecondaryWatch for ReplicaConvoyCheckoutWatch {
    type Primary = crate::Checkout;

    fn clone_box(&self) -> Box<dyn SecondaryWatch<Primary = Self::Primary>> {
        Box::new(self.clone())
    }

    fn spawn(
        self: Box<Self>,
        _backend: ResourceBackend,
        _namespace: String,
        sender: mpsc::Sender<String>,
    ) -> Pin<Box<dyn Future<Output = Result<(), ResourceError>> + Send>> {
        Box::pin(async move {
            let mut watch = self.resolver.watch().await?;
            let listed = self.resolver.list().await?;
            for source in &listed.items {
                Self::enqueue_checkouts(&sender, &source.object).await?;
            }
            while let Some(event) = watch.next().await {
                let source = match event? {
                    crate::ReadWatchEvent::Added(source)
                    | crate::ReadWatchEvent::Modified(source)
                    | crate::ReadWatchEvent::Deleted(source) => source,
                    crate::ReadWatchEvent::DeletedByName { .. } => continue,
                };
                Self::enqueue_checkouts(&sender, &source.object).await?;
            }
            Ok(())
        })
    }
}

#[derive(Clone)]
pub struct LabelJoinWatch<W: Resource, P: Resource> {
    pub label_key: &'static str,
    pub _marker: PhantomData<(W, P)>,
}

impl<W: Resource, P: Resource> LabelJoinWatch<W, P> {
    async fn enqueue_matching_primaries(
        label_key: &'static str,
        sender: &mpsc::Sender<String>,
        watched: &ResourceObject<W>,
        primaries: &TypedResolver<P>,
    ) -> Result<(), ResourceError> {
        let Some(value) = watched.metadata.labels.get(label_key) else {
            return Ok(());
        };
        let selector = BTreeMap::from([(label_key.to_string(), value.clone())]);
        let listed = primaries.list_matching_labels(&selector).await?;
        for object in listed.items {
            sender
                .send(object.metadata.name)
                .await
                .map_err(|_| ResourceError::other("controller queue closed while forwarding joined secondary event"))?;
        }
        Ok(())
    }
}

impl<W: Resource, P: Resource> SecondaryWatch for LabelJoinWatch<W, P> {
    type Primary = P;

    fn clone_box(&self) -> Box<dyn SecondaryWatch<Primary = Self::Primary>> {
        Box::new(Self { label_key: self.label_key, _marker: PhantomData })
    }

    fn spawn(
        self: Box<Self>,
        backend: ResourceBackend,
        namespace: String,
        sender: mpsc::Sender<String>,
    ) -> Pin<Box<dyn Future<Output = Result<(), ResourceError>> + Send>> {
        Box::pin(async move {
            let watched = backend.clone().using::<W>(&namespace);
            let primaries = backend.using::<P>(&namespace);
            let listed = watched.list().await?;
            for object in &listed.items {
                Self::enqueue_matching_primaries(self.label_key, &sender, object, &primaries).await?;
            }

            let mut watch = watched.watch(WatchStart::resuming_from(&listed)).await?;
            while let Some(event) = watch.next().await {
                match event? {
                    WatchEvent::Added(object) | WatchEvent::Modified(object) | WatchEvent::Deleted(object) => {
                        Self::enqueue_matching_primaries(self.label_key, &sender, &object, &primaries).await?;
                    }
                    WatchEvent::DeletedByName(_) => {}
                }
            }
            Ok(())
        })
    }
}

enum WatchExited {
    Primary(Result<(), ResourceError>),
    Secondary { index: usize, result: Result<(), ResourceError> },
}

pub struct ControllerLoop<R: Reconciler> {
    pub primary: TypedResolver<R::Resource>,
    pub secondaries: Vec<Box<dyn SecondaryWatch<Primary = R::Resource>>>,
    pub reconciler: R,
    pub resync_interval: Duration,
    pub backend: ResourceBackend,
}

impl<R: Reconciler> ControllerLoop<R> {
    const WATCH_RESTART_BACKOFF: Duration = Duration::from_millis(100);

    async fn write_tolerating_not_found<T>(write: impl Future<Output = Result<T, ResourceError>>) -> Result<(), ResourceError> {
        match write.await {
            Ok(_) | Err(ResourceError::NotFound { .. }) => Ok(()),
            Err(err) => Err(err),
        }
    }

    async fn apply_actuation(backend: &ResourceBackend, namespace: &str, actuation: Actuation) -> Result<(), ResourceError> {
        match actuation {
            Actuation::CreateRepository { key, spec } => {
                let resolver = backend.using::<crate::Repository>(namespace);
                crate::ensure_repository(&resolver, &key, &spec).await.map(|_| ())
            }
            Actuation::CreateEnvironment { meta, spec } => {
                let resolver = backend.using::<crate::Environment>(namespace);
                Self::create_if_missing(&resolver, meta, spec).await
            }
            Actuation::CreateClone { meta, spec } => {
                let resolver = backend.using::<crate::Clone>(namespace);
                Self::create_if_missing(&resolver, meta, spec).await
            }
            Actuation::RetryClone { name, failed_at } => {
                let resolver = backend.using::<crate::Clone>(namespace);
                let clone = match resolver.get(&name).await {
                    Ok(clone) => clone,
                    Err(ResourceError::NotFound { .. }) => return Ok(()),
                    Err(error) => return Err(error),
                };
                let current_failed_at =
                    clone.status.as_ref().and_then(|status| status.failed_at).unwrap_or(clone.metadata.creation_timestamp);
                if clone.status.as_ref().map(|status| status.phase) != Some(crate::ClonePhase::Failed) || current_failed_at != failed_at {
                    return Ok(());
                }
                crate::apply_status_patch(&resolver, &name, &crate::CloneStatusPatch::MarkCloning).await.map(|_| ())
            }
            Actuation::CreateCheckout { meta, spec } => {
                let resolver = backend.using::<crate::Checkout>(namespace);
                Self::create_if_missing(&resolver, meta, spec).await
            }
            Actuation::CreateTerminalSession { meta, spec } => {
                let resolver = backend.using::<crate::TerminalSession>(namespace);
                Self::create_if_missing(&resolver, meta, spec).await
            }
            Actuation::RestartTerminalSession { name } => {
                let resolver = backend.using::<crate::TerminalSession>(namespace);
                crate::apply_status_patch(&resolver, &name, &crate::TerminalSessionStatusPatch::MarkStarting).await.map(|_| ())
            }
            Actuation::DeleteTerminalSession { name } => {
                let resolver = backend.using::<crate::TerminalSession>(namespace);
                Self::delete_if_lifecycle_owned(&resolver, &name).await
            }
            Actuation::CreateVessel { meta, spec } => {
                let resolver = backend.using::<crate::Vessel>(namespace);
                Self::create_if_missing(&resolver, meta, spec).await
            }
            Actuation::CreatePresentation { meta, spec } => {
                let resolver = backend.using::<crate::Presentation>(namespace);
                Self::create_if_missing(&resolver, meta, spec).await
            }
            Actuation::DeletePresentation { name } => {
                let resolver = backend.using::<crate::Presentation>(namespace);
                Self::delete_if_lifecycle_owned(&resolver, &name).await
            }
            Actuation::DeleteVessel { name } => {
                let resolver = backend.using::<crate::Vessel>(namespace);
                Self::delete_if_lifecycle_owned(&resolver, &name).await
            }
            Actuation::DeleteCheckout { name } => {
                let resolver = backend.using::<crate::Checkout>(namespace);
                Self::delete_if_lifecycle_owned(&resolver, &name).await
            }
        }
    }

    async fn create_if_missing<T: Resource>(resolver: &TypedResolver<T>, mut meta: InputMeta, spec: T::Spec) -> Result<(), ResourceError> {
        meta.set_lifecycle_authority(LifecycleAuthority::Managed);
        match resolver.create(&meta, &spec).await {
            Ok(_) | Err(ResourceError::Conflict { .. }) => Ok(()),
            Err(err) => Err(err),
        }
    }

    async fn delete_if_lifecycle_owned<T: Resource>(resolver: &TypedResolver<T>, name: &str) -> Result<(), ResourceError> {
        let object = match resolver.get(name).await {
            Ok(object) => object,
            Err(ResourceError::NotFound { .. }) => return Ok(()),
            Err(err) => return Err(err),
        };
        if !is_lifecycle_owned(object.metadata.lifecycle_authority()?) {
            return Ok(());
        }
        match resolver.delete(name).await {
            Ok(()) | Err(ResourceError::NotFound { .. }) => Ok(()),
            Err(err) => Err(err),
        }
    }

    fn spawn_primary_watch(
        primary: TypedResolver<R::Resource>,
        sender: mpsc::Sender<String>,
        watch_exited: mpsc::UnboundedSender<WatchExited>,
        restart_backoff: Option<Duration>,
    ) -> JoinHandle<()> {
        tokio::spawn(async move {
            if let Some(backoff) = restart_backoff {
                tokio::time::sleep(backoff).await;
            }
            let result = async {
                let listed = primary.list().await?;
                for object in &listed.items {
                    sender
                        .send(object.metadata.name.clone())
                        .await
                        .map_err(|_| ResourceError::other("controller queue closed while forwarding initial primary list"))?;
                }

                let mut watch = primary.watch(WatchStart::resuming_from(&listed)).await?;
                while let Some(event) = watch.next().await {
                    match event? {
                        WatchEvent::Added(object) | WatchEvent::Modified(object) | WatchEvent::Deleted(object) => {
                            sender
                                .send(object.metadata.name)
                                .await
                                .map_err(|_| ResourceError::other("controller queue closed while forwarding primary watch event"))?;
                        }
                        WatchEvent::DeletedByName(tombstone) => {
                            sender
                                .send(tombstone.name)
                                .await
                                .map_err(|_| ResourceError::other("controller queue closed while forwarding primary tombstone"))?;
                        }
                    }
                }
                Ok(())
            }
            .await;

            let _ = watch_exited.send(WatchExited::Primary(result));
        })
    }

    fn spawn_secondary_watch(
        index: usize,
        watch: Box<dyn SecondaryWatch<Primary = R::Resource>>,
        backend: ResourceBackend,
        namespace: String,
        sender: mpsc::Sender<String>,
        watch_exited: mpsc::UnboundedSender<WatchExited>,
        restart_backoff: Option<Duration>,
    ) -> JoinHandle<()> {
        tokio::spawn(async move {
            if let Some(backoff) = restart_backoff {
                tokio::time::sleep(backoff).await;
            }
            let result = watch.spawn(backend, namespace, sender).await;
            let _ = watch_exited.send(WatchExited::Secondary { index, result });
        })
    }

    async fn resync_all(primary: &TypedResolver<R::Resource>, sender: &mpsc::Sender<String>) -> Result<(), ResourceError> {
        let listed = primary.list().await?;
        for object in listed.items {
            sender
                .send(object.metadata.name)
                .await
                .map_err(|_| ResourceError::other("controller queue closed while forwarding resync item"))?;
        }
        Ok(())
    }

    fn accept_restartable_watch_exit(result: Result<(), ResourceError>) -> Result<(), ResourceError> {
        match result {
            Ok(()) | Err(ResourceError::WatchExpired { .. }) => Ok(()),
            Err(err) => Err(err),
        }
    }

    pub async fn run(self) -> Result<(), ResourceError>
    where
        <R::Resource as Resource>::Status: Default,
    {
        let ControllerLoop { primary, secondaries, reconciler, resync_interval, backend } = self;
        let (sender, mut receiver) = mpsc::channel::<String>(128);
        let (watch_exited_tx, mut watch_exited_rx) = mpsc::unbounded_channel();
        let _primary_watch = Self::spawn_primary_watch(primary.clone(), sender.clone(), watch_exited_tx.clone(), None);
        let secondary_templates = secondaries;
        let _secondary_watches: Vec<JoinHandle<()>> = secondary_templates
            .iter()
            .enumerate()
            .map(|(index, watch)| {
                Self::spawn_secondary_watch(
                    index,
                    watch.clone(),
                    backend.clone(),
                    primary.namespace.clone(),
                    sender.clone(),
                    watch_exited_tx.clone(),
                    None,
                )
            })
            .collect();
        let mut resync = tokio::time::interval(resync_interval);
        resync.tick().await;
        let mut pending: VecDeque<String> = VecDeque::new();
        let scheduled_requeues = Arc::new(Mutex::new(HashSet::new()));
        let mut object_failures = BTreeMap::<String, ObjectFailure>::new();

        loop {
            if let Some(name) = pending.pop_front() {
                let object = match primary.get(&name).await {
                    Ok(object) => object,
                    Err(ResourceError::NotFound { .. }) => {
                        object_failures.remove(&name);
                        continue;
                    }
                    Err(err) => {
                        warn!(
                            resource_kind = R::Resource::API_PATHS.kind,
                            resource = %name,
                            %err,
                            "controller failed to read object; leaving it for a later resync",
                        );
                        continue;
                    }
                };
                let mut attempted_reconcile = false;
                let result = async {
                    let lifecycle_owned = is_lifecycle_owned(object.metadata.lifecycle_authority()?);
                    if let Some(finalizer_name) = reconciler.finalizer_name() {
                        if object.metadata.deletion_timestamp.is_none()
                            && object.metadata.finalizers.iter().all(|finalizer| finalizer != finalizer_name)
                            && lifecycle_owned
                        {
                            let meta = InputMeta::from(&object.metadata).with_added_finalizer(finalizer_name);
                            Self::write_tolerating_not_found(primary.update(&meta, &object.metadata.resource_version, &object.spec))
                                .await?;
                            return Ok(());
                        }
                        if object.metadata.deletion_timestamp.is_some()
                            && object.metadata.finalizers.iter().any(|finalizer| finalizer == finalizer_name)
                        {
                            if lifecycle_owned {
                                if let Err(err) = reconciler.run_finalizer(&object).await {
                                    if let Some(patch) = reconciler.finalizer_error_patch(&object, &err) {
                                        if let Err(patch_error) =
                                            Self::write_tolerating_not_found(apply_status_patch(&primary, &name, &patch)).await
                                        {
                                            warn!(
                                                resource_kind = R::Resource::API_PATHS.kind,
                                                resource = %name,
                                                %patch_error,
                                                "controller failed to record object finalizer error",
                                            );
                                        }
                                    }
                                    return Err(err);
                                }
                            }
                            let meta = InputMeta::from(&object.metadata).without_finalizer(finalizer_name);
                            Self::write_tolerating_not_found(primary.update(&meta, &object.metadata.resource_version, &object.spec))
                                .await?;
                            return Ok(());
                        }
                    }
                    if object.metadata.deletion_timestamp.is_some() {
                        return Ok(());
                    }
                    if !lifecycle_owned {
                        return Ok(());
                    }
                    let degraded_needs_reconcile =
                        reconciler.is_reconcile_degraded(&object) && reconciler.degraded_object_needs_reconcile(&object).await?;
                    if reconciler.is_reconcile_degraded(&object) && !degraded_needs_reconcile {
                        return Ok(());
                    }
                    if object_failures.get(&name).is_some_and(|failure| failure.creation_timestamp != object.metadata.creation_timestamp) {
                        object_failures.remove(&name);
                    }
                    if let Some(failure) = object_failures.get(&name) {
                        if !degraded_needs_reconcile && (failure.terminal || Instant::now() < failure.retry_at) {
                            return Ok(());
                        }
                    }
                    attempted_reconcile = true;
                    let deps = reconciler.fetch_dependencies(&object).await?;
                    let outcome = reconciler.reconcile(&object, &deps, Utc::now());
                    for actuation in outcome.actuations {
                        Self::apply_actuation(&primary.backend, &primary.namespace, actuation).await?;
                    }
                    if let Some(patch) = outcome.patch {
                        Self::write_tolerating_not_found(apply_status_patch(&primary, &name, &patch)).await?;
                    }
                    if let Some(delay) = outcome.requeue_after {
                        let mut scheduled = scheduled_requeues.lock().await;
                        if scheduled.insert(name.clone()) {
                            let scheduled_requeues = Arc::clone(&scheduled_requeues);
                            let sender = sender.clone();
                            let name = name.clone();
                            tokio::spawn(async move {
                                tokio::time::sleep(delay).await;
                                scheduled_requeues.lock().await.remove(&name);
                                let _ = sender.send(name).await;
                            });
                        }
                    }
                    Ok(())
                }
                .await;
                match result {
                    Ok(()) if attempted_reconcile => {
                        object_failures.remove(&name);
                    }
                    Ok(()) => {}
                    Err(err) => {
                        let mut terminal = false;
                        let mut retry_after = None;
                        if attempted_reconcile {
                            if let Some(policy) = reconciler.reconcile_error_policy() {
                                let message = err.to_string();
                                let previous = object_failures.get(&name);
                                let consecutive_failures = previous
                                    .filter(|previous| previous.failure.message == message)
                                    .map_or(1, |previous| previous.failure.consecutive_failures.saturating_add(1));
                                let failure = ReconcileFailure { message, consecutive_failures };
                                terminal = consecutive_failures >= policy.max_consecutive_failures;
                                let delay = policy.backoff(consecutive_failures);
                                if terminal {
                                    if let Some(patch) = reconciler.reconcile_degraded_patch(&object, &failure) {
                                        match apply_status_patch(&primary, &name, &patch).await {
                                            Ok(_) => {}
                                            Err(ResourceError::NotFound { .. }) => {
                                                object_failures.remove(&name);
                                                continue;
                                            }
                                            Err(patch_error) => {
                                                terminal = false;
                                                retry_after = Some(delay);
                                                warn!(
                                                    resource_kind = R::Resource::API_PATHS.kind,
                                                    resource = %name,
                                                    %patch_error,
                                                    "controller failed to record degraded reconcile condition; retrying",
                                                );
                                            }
                                        }
                                    } else {
                                        terminal = false;
                                        retry_after = Some(delay);
                                    }
                                } else {
                                    retry_after = Some(delay);
                                }
                                object_failures.insert(name.clone(), ObjectFailure {
                                    failure,
                                    creation_timestamp: object.metadata.creation_timestamp,
                                    retry_at: Instant::now() + delay,
                                    terminal,
                                });
                            }
                        }
                        warn!(
                            resource_kind = R::Resource::API_PATHS.kind,
                            resource = %name,
                            %err,
                            terminal,
                            "controller failed to reconcile object; other objects will continue",
                        );
                        if let Some(delay) = retry_after {
                            let mut scheduled = scheduled_requeues.lock().await;
                            if scheduled.insert(name.clone()) {
                                let scheduled_requeues = Arc::clone(&scheduled_requeues);
                                let sender = sender.clone();
                                let name = name.clone();
                                tokio::spawn(async move {
                                    tokio::time::sleep(delay).await;
                                    scheduled_requeues.lock().await.remove(&name);
                                    let _ = sender.send(name).await;
                                });
                            }
                        }
                    }
                }
                continue;
            }

            tokio::select! {
                maybe_name = receiver.recv() => {
                    let Some(name) = maybe_name else {
                        return Ok(());
                    };
                    while let Ok(next) = receiver.try_recv() {
                        if next != name {
                            pending.push_back(next);
                        }
                    }
                    pending.push_front(name);
                }
                _ = resync.tick() => {
                    Self::resync_all(&primary, &sender).await?;
                }
                Some(exited) = watch_exited_rx.recv() => {
                    match exited {
                        WatchExited::Primary(result) => {
                            Self::accept_restartable_watch_exit(result)?;
                            let _respawn = Self::spawn_primary_watch(
                                primary.clone(),
                                sender.clone(),
                                watch_exited_tx.clone(),
                                Some(Self::WATCH_RESTART_BACKOFF),
                            );
                        }
                        WatchExited::Secondary { index, result } => {
                            Self::accept_restartable_watch_exit(result)?;
                            // Relisting a secondary can enqueue objects that still exist, but
                            // cannot reveal a deletion missed while its watch was expired.
                            Self::resync_all(&primary, &sender).await?;
                            let _respawn = Self::spawn_secondary_watch(
                                index,
                                secondary_templates[index].clone(),
                                backend.clone(),
                                primary.namespace.clone(),
                                sender.clone(),
                                watch_exited_tx.clone(),
                                Some(Self::WATCH_RESTART_BACKOFF),
                            );
                        }
                    }
                }
            }
        }
    }
}

fn is_lifecycle_owned(authority: Option<LifecycleAuthority>) -> bool {
    !matches!(authority, Some(LifecycleAuthority::Observed | LifecycleAuthority::Adopted))
}

/// List every resource matching `selector` and delete only lifecycle-owned children.
///
/// `Observed` and `Adopted` children may be referenced by a managed parent but their
/// lifecycle belongs outside the parent reconciler, so cascading teardown must not
/// delete them.
pub async fn delete_lifecycle_owned_matching<T: Resource>(
    resolver: &TypedResolver<T>,
    selector: &BTreeMap<String, String>,
) -> Result<(), ResourceError> {
    let listed = resolver.list_matching_labels(selector).await?;
    for object in listed.items {
        if !is_lifecycle_owned(object.metadata.lifecycle_authority()?) {
            continue;
        }
        match resolver.delete(&object.metadata.name).await {
            Ok(()) | Err(ResourceError::NotFound { .. }) => {}
            Err(err) => return Err(err),
        }
    }
    Ok(())
}
