use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
};

use chrono::{DateTime, Duration, Utc};
use flotilla_protocol::{PrincipalRef, ResourceRef, SurfaceCharacter, SurfaceDeclaration};
use flotilla_resources::{
    apply_status_patch, InputMeta, Regard, RegardExpiryPolicy, RegardSource, RegardSpec, RegardStatusPatch, ResourceBackend, ResourceError,
};
use sha2::{Digest, Sha256};
use uuid::Uuid;

pub const DEFAULT_REGARD_DECAY_SECONDS: i64 = 300;
pub const DEFAULT_REGARD_REFRESH_SECONDS: u64 = 60;

pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}

#[derive(Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

#[derive(Debug, Clone)]
struct SurfaceState {
    declaration: SurfaceDeclaration,
    focused: HashSet<ResourceRef>,
}

#[derive(bon::Builder)]
pub struct RegardLifecycle {
    backend: ResourceBackend,
    clock: Arc<dyn Clock>,
    decay_window: Duration,
    #[builder(skip)]
    surfaces: Mutex<HashMap<Uuid, SurfaceState>>,
    #[builder(skip)]
    tracked_namespaces: Mutex<HashSet<String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SurfaceGestureOutcome {
    Handled,
    UnknownSurface,
}

impl RegardLifecycle {
    pub fn new(backend: ResourceBackend, clock: Arc<dyn Clock>, decay_window: Duration) -> Self {
        Self::builder().backend(backend).clock(clock).decay_window(decay_window).build()
    }

    pub fn with_system_clock(backend: ResourceBackend) -> Self {
        Self::new(backend, Arc::new(SystemClock), Duration::seconds(DEFAULT_REGARD_DECAY_SECONDS))
    }

    pub fn connect_surface(&self, surface_id: Uuid, declaration: SurfaceDeclaration) {
        self.surfaces
            .lock()
            .expect("regard surfaces lock poisoned")
            .insert(surface_id, SurfaceState { declaration, focused: HashSet::new() });
    }

    pub fn principal_for_surface(&self, surface_id: Uuid) -> Result<Option<PrincipalRef>, String> {
        Ok(self
            .surfaces
            .lock()
            .map_err(|_| "regard surfaces lock poisoned".to_string())?
            .get(&surface_id)
            .map(|surface| surface.declaration.principal_ref.clone()))
    }

    pub async fn emit_expressed(&self, principal: &PrincipalRef, target: &ResourceRef) -> Result<(), String> {
        self.record_regard(principal, target, RegardSource::Expressed).await
    }

    /// Emit an explicit gesture for a connected surface. Returns
    /// [`SurfaceGestureOutcome::UnknownSurface`] when the ID does not name a
    /// local surface, allowing forwarded and direct callers to supply their
    /// known principal.
    pub async fn emit_expressed_for_surface(&self, surface_id: Uuid, target: &ResourceRef) -> Result<SurfaceGestureOutcome, String> {
        let connected = self.surfaces.lock().map_err(|_| "regard surfaces lock poisoned".to_string())?.contains_key(&surface_id);
        if !connected {
            return Ok(SurfaceGestureOutcome::UnknownSurface);
        }
        self.observe_focus(surface_id, vec![target.clone()]).await?;
        Ok(SurfaceGestureOutcome::Handled)
    }

    pub async fn emit_implicit(&self, principal: &PrincipalRef, target: &ResourceRef, policy: &str) -> Result<(), String> {
        self.record_regard(principal, target, RegardSource::Implicit { policy: policy.to_string() }).await
    }

    pub async fn disconnect_surface(&self, surface_id: Uuid) -> Result<(), String> {
        let surface = self.surfaces.lock().map_err(|_| "regard surfaces lock poisoned".to_string())?.remove(&surface_id);
        let Some(surface) = surface else {
            return Ok(());
        };
        if surface.declaration.character == SurfaceCharacter::Ambient {
            return Ok(());
        }
        for target in surface.focused {
            self.record_regard(&surface.declaration.principal_ref, &target, RegardSource::Expressed).await?;
        }
        Ok(())
    }

    pub async fn observe_focus(&self, surface_id: Uuid, targets: Vec<ResourceRef>) -> Result<(), String> {
        let (declaration, previous, current) = {
            let mut surfaces = self.surfaces.lock().map_err(|_| "regard surfaces lock poisoned".to_string())?;
            let surface = surfaces.get_mut(&surface_id).ok_or_else(|| format!("surface {surface_id} is not connected"))?;
            if surface.declaration.character == SurfaceCharacter::Ambient {
                return Ok(());
            }
            let previous = surface.focused.clone();
            let current = targets.into_iter().collect::<HashSet<_>>();
            surface.focused = current.clone();
            (surface.declaration.clone(), previous, current)
        };

        // Refresh targets at both edges: current focus expresses regard, and
        // the departure edge starts a full decay window rather than expiring
        // immediately after a long uninterrupted focus spell.
        for target in previous.union(&current) {
            self.record_regard(&declaration.principal_ref, target, RegardSource::Expressed).await?;
        }
        Ok(())
    }

    /// Persist a heartbeat for every target currently focused on this daemon.
    /// Because Regard status is mesh-resident, heartbeats from every daemon
    /// make expiry conservative across the principal's whole surface set.
    pub async fn refresh_focused(&self) -> Result<(), String> {
        for (principal, target) in self.focused_targets()? {
            self.record_regard(&principal, &target, RegardSource::Expressed).await?;
        }
        Ok(())
    }

    pub async fn expire_due(&self, namespace: &str) -> Result<(), String> {
        let now = self.clock.now();
        let focused = self.focused_targets()?;
        let mut namespaces = {
            let mut tracked = self.tracked_namespaces.lock().map_err(|_| "regard namespaces lock poisoned".to_string())?;
            tracked.insert(namespace.to_string());
            tracked.iter().cloned().collect::<Vec<_>>()
        };
        namespaces.sort();

        for namespace in namespaces {
            let resolver = self.backend.using::<Regard>(&namespace);
            let regards = resolver.list().await.map_err(|error| error.to_string())?;
            for regard in regards.items {
                let RegardExpiryPolicy::Decaying { expires_after_seconds } = regard.spec.expiry else {
                    continue;
                };
                if focused.contains(&(regard.spec.principal_ref.clone(), regard.spec.target.clone())) {
                    continue;
                }
                let refreshed_at = regard.status.as_ref().and_then(|status| status.refreshed_at.or(status.created_at));
                let Some(refreshed_at) = refreshed_at else {
                    continue;
                };
                if now.signed_duration_since(refreshed_at) >= Duration::seconds(expires_after_seconds as i64) {
                    resolver.delete(&regard.metadata.name).await.map_err(|error| error.to_string())?;
                }
            }
        }
        Ok(())
    }

    fn focused_targets(&self) -> Result<HashSet<(PrincipalRef, ResourceRef)>, String> {
        Ok(self
            .surfaces
            .lock()
            .map_err(|_| "regard surfaces lock poisoned".to_string())?
            .values()
            .filter(|surface| surface.declaration.character == SurfaceCharacter::Focal)
            .flat_map(|surface| {
                surface.focused.iter().cloned().map(|target| (surface.declaration.principal_ref.clone(), target)).collect::<Vec<_>>()
            })
            .collect())
    }

    async fn record_regard(&self, principal: &PrincipalRef, target: &ResourceRef, source: RegardSource) -> Result<(), String> {
        self.tracked_namespaces.lock().map_err(|_| "regard namespaces lock poisoned".to_string())?.insert(target.namespace.clone());
        let resolver = self.backend.using::<Regard>(&target.namespace);
        let name = regard_name(principal, target);
        let spec = RegardSpec::builder()
            .principal_ref(principal.clone())
            .target(target.clone())
            .source(source.clone())
            .expiry(RegardExpiryPolicy::Decaying { expires_after_seconds: self.decay_window.num_seconds() as u64 })
            .build();
        let mut existing = match resolver.get(&name).await {
            Ok(existing) => Some(existing),
            Err(ResourceError::NotFound { .. }) => match resolver.create(&InputMeta::builder().name(name.clone()).build(), &spec).await {
                Ok(_) => None,
                Err(ResourceError::Conflict { .. }) => Some(resolver.get(&name).await.map_err(|error| error.to_string())?),
                Err(error) => return Err(error.to_string()),
            },
            Err(error) => return Err(error.to_string()),
        };
        for _ in 0..5 {
            let Some(current) = existing.take() else { break };
            if source != RegardSource::Expressed || matches!(current.spec.source, RegardSource::Expressed) {
                break;
            }
            let mut promoted = current.spec.clone();
            promoted.source = RegardSource::Expressed;
            let meta = InputMeta::builder()
                .name(current.metadata.name)
                .labels(current.metadata.labels)
                .annotations(current.metadata.annotations)
                .owner_references(current.metadata.owner_references)
                .finalizers(current.metadata.finalizers)
                .maybe_deletion_timestamp(current.metadata.deletion_timestamp)
                .build();
            match resolver.update(&meta, &current.metadata.resource_version, &promoted).await {
                Ok(_) => break,
                Err(ResourceError::Conflict { .. }) => {
                    existing = Some(resolver.get(&name).await.map_err(|error| error.to_string())?);
                }
                Err(error) => return Err(error.to_string()),
            }
        }
        if existing.is_some() {
            return Err(format!("regard {name} remained conflicted after retries"));
        }
        apply_status_patch(&resolver, &name, &RegardStatusPatch::Refresh { as_of: self.clock.now() })
            .await
            .map(|_| ())
            .map_err(|error| error.to_string())
    }
}

fn regard_name(principal: &PrincipalRef, target: &ResourceRef) -> String {
    let mut digest = Sha256::new();
    digest.update(principal.namespace.as_bytes());
    digest.update([0]);
    digest.update(principal.name.as_bytes());
    digest.update([0]);
    digest.update(serde_json::to_vec(target).expect("resource ref serialization"));
    let digest = digest.finalize();
    let suffix = digest[..8].iter().map(|byte| format!("{byte:02x}")).collect::<String>();
    format!("regard-{suffix}")
}
