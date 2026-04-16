use std::{
    collections::{BTreeMap, VecDeque},
    future::Future,
    marker::PhantomData,
    pin::Pin,
    time::Duration,
};

use chrono::{DateTime, Utc};
use futures::StreamExt;
use tokio::{sync::mpsc, task::JoinHandle};

use crate::{
    apply_status_patch,
    backend::{ResourceBackend, TypedResolver},
    checkout::CheckoutSpec,
    clone::CloneSpec,
    environment::EnvironmentSpec,
    error::ResourceError,
    presentation::PresentationSpec,
    resource::{InputMeta, Resource, ResourceObject},
    task_workspace::TaskWorkspaceSpec,
    terminal_session::TerminalSessionSpec,
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
    CreateEnvironment { meta: InputMeta, spec: EnvironmentSpec },
    CreateClone { meta: InputMeta, spec: CloneSpec },
    CreateCheckout { meta: InputMeta, spec: CheckoutSpec },
    CreateTerminalSession { meta: InputMeta, spec: TerminalSessionSpec },
    CreateTaskWorkspace { meta: InputMeta, spec: TaskWorkspaceSpec },
    CreatePresentation { meta: InputMeta, spec: PresentationSpec },
    DeletePresentation { name: String },
    DeleteTaskWorkspace { name: String },
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

            let mut watch = resolver.watch(WatchStart::FromVersion(listed.resource_version)).await?;
            while let Some(event) = watch.next().await {
                match event? {
                    WatchEvent::Added(object) | WatchEvent::Modified(object) | WatchEvent::Deleted(object) => {
                        Self::enqueue_from_object(self.label_key, &sender, &object).await?;
                    }
                }
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

            let mut watch = watched.watch(WatchStart::FromVersion(listed.resource_version)).await?;
            while let Some(event) = watch.next().await {
                match event? {
                    WatchEvent::Added(object) | WatchEvent::Modified(object) | WatchEvent::Deleted(object) => {
                        Self::enqueue_matching_primaries(self.label_key, &sender, &object, &primaries).await?;
                    }
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

    async fn apply_actuation(backend: &ResourceBackend, namespace: &str, actuation: Actuation) -> Result<(), ResourceError> {
        match actuation {
            Actuation::CreateEnvironment { meta, spec } => {
                let resolver = backend.using::<crate::Environment>(namespace);
                Self::create_if_missing(&resolver, meta, spec).await
            }
            Actuation::CreateClone { meta, spec } => {
                let resolver = backend.using::<crate::Clone>(namespace);
                Self::create_if_missing(&resolver, meta, spec).await
            }
            Actuation::CreateCheckout { meta, spec } => {
                let resolver = backend.using::<crate::Checkout>(namespace);
                Self::create_if_missing(&resolver, meta, spec).await
            }
            Actuation::CreateTerminalSession { meta, spec } => {
                let resolver = backend.using::<crate::TerminalSession>(namespace);
                Self::create_if_missing(&resolver, meta, spec).await
            }
            Actuation::CreateTaskWorkspace { meta, spec } => {
                let resolver = backend.using::<crate::TaskWorkspace>(namespace);
                Self::create_if_missing(&resolver, meta, spec).await
            }
            Actuation::CreatePresentation { meta, spec } => {
                let resolver = backend.using::<crate::Presentation>(namespace);
                Self::create_if_missing(&resolver, meta, spec).await
            }
            Actuation::DeletePresentation { name } => {
                let resolver = backend.using::<crate::Presentation>(namespace);
                match resolver.delete(&name).await {
                    Ok(()) | Err(ResourceError::NotFound { .. }) => Ok(()),
                    Err(err) => Err(err),
                }
            }
            Actuation::DeleteTaskWorkspace { name } => {
                let resolver = backend.using::<crate::TaskWorkspace>(namespace);
                match resolver.delete(&name).await {
                    Ok(()) | Err(ResourceError::NotFound { .. }) => Ok(()),
                    Err(err) => Err(err),
                }
            }
        }
    }

    async fn create_if_missing<T: Resource>(resolver: &TypedResolver<T>, meta: InputMeta, spec: T::Spec) -> Result<(), ResourceError> {
        match resolver.create(&meta, &spec).await {
            Ok(_) | Err(ResourceError::Conflict { .. }) => Ok(()),
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

                let mut watch = primary.watch(WatchStart::FromVersion(listed.resource_version)).await?;
                while let Some(event) = watch.next().await {
                    match event? {
                        WatchEvent::Added(object) | WatchEvent::Modified(object) | WatchEvent::Deleted(object) => {
                            sender
                                .send(object.metadata.name)
                                .await
                                .map_err(|_| ResourceError::other("controller queue closed while forwarding primary watch event"))?;
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

        loop {
            if let Some(name) = pending.pop_front() {
                let object = match primary.get(&name).await {
                    Ok(object) => object,
                    Err(ResourceError::NotFound { .. }) => continue,
                    Err(err) => return Err(err),
                };
                if let Some(finalizer_name) = reconciler.finalizer_name() {
                    if object.metadata.deletion_timestamp.is_none()
                        && object.metadata.finalizers.iter().all(|finalizer| finalizer != finalizer_name)
                    {
                        let meta = InputMeta::from(&object.metadata).with_added_finalizer(finalizer_name);
                        // A racing writer may win between get() and update(); rely on the resulting
                        // watch event to requeue the object and retry finalizer attachment.
                        primary.update(&meta, &object.metadata.resource_version, &object.spec).await?;
                        continue;
                    }
                    if object.metadata.deletion_timestamp.is_some()
                        && object.metadata.finalizers.iter().any(|finalizer| finalizer == finalizer_name)
                    {
                        reconciler.run_finalizer(&object).await?;
                        let meta = InputMeta::from(&object.metadata).without_finalizer(finalizer_name);
                        primary.update(&meta, &object.metadata.resource_version, &object.spec).await?;
                        continue;
                    }
                }
                if object.metadata.deletion_timestamp.is_some() {
                    continue;
                }
                let deps = reconciler.fetch_dependencies(&object).await?;
                let outcome = reconciler.reconcile(&object, &deps, Utc::now());
                for actuation in outcome.actuations {
                    Self::apply_actuation(&primary.backend, &primary.namespace, actuation).await?;
                }
                if let Some(patch) = outcome.patch {
                    apply_status_patch(&primary, &name, &patch).await?;
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
                            result?;
                            let _respawn = Self::spawn_primary_watch(
                                primary.clone(),
                                sender.clone(),
                                watch_exited_tx.clone(),
                                Some(Self::WATCH_RESTART_BACKOFF),
                            );
                        }
                        WatchExited::Secondary { index, result } => {
                            result?;
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
