//! Host adoption is an environment lifecycle, with no backing to create or kill.
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};

use async_trait::async_trait;
use flotilla_protocol::{EnvironmentId, ImageId};

use super::{EnvironmentHandle, EnvironmentInput, EnvironmentProvider, PreparedEnvironment, ProvisionedEnvironment, ProvisionedMount};
use crate::environment::{PrepareOpts, ProvisionOpts};
use flotilla_provider_runtime::CommandRunner;

/// Legacy handles require an image id even for image-free adoption.
const HOST_DIRECT_IMAGE_SENTINEL: &str = "host-direct:no-image";

pub struct HostDirectEnvironmentProvider {
    runner: Arc<dyn CommandRunner>,
    environment: HashMap<String, String>,
    owner: Arc<()>,
    handles: Mutex<HashMap<EnvironmentId, Arc<HostHandle>>>,
}

impl HostDirectEnvironmentProvider {
    pub fn new(runner: Arc<dyn CommandRunner>, environment: HashMap<String, String>) -> Self {
        Self { runner, environment, owner: Arc::new(()), handles: Mutex::new(HashMap::new()) }
    }
}

#[async_trait]
impl EnvironmentProvider for HostDirectEnvironmentProvider {
    async fn detect_readiness(&self) -> super::ProviderReadiness {
        super::ProviderReadiness::Ready
    }
    fn kind(&self) -> super::ProviderKind {
        super::ProviderKind::new(super::EnvironmentClass::HostDirect, "host-direct")
    }
    fn capabilities(&self) -> super::ProviderCapabilities {
        Self::capability_declaration()
    }

    fn legacy_input(&self) -> Option<EnvironmentInput> {
        Some(EnvironmentInput::HostDirect)
    }

    fn detection(&self) -> Option<&dyn super::EnvironmentDetection> {
        Some(self)
    }

    async fn prepare(&self, spec: &flotilla_resources::EnvironmentSpec, _opts: &PrepareOpts) -> Result<PreparedEnvironment, String> {
        if Some(EnvironmentInput::of(spec)?) != self.legacy_input() {
            return Err("host-direct provider requires a host-direct spec".into());
        }
        Ok(PreparedEnvironment::new(&self.owner, ()))
    }

    async fn provision(
        &self,
        id: EnvironmentId,
        prepared: &PreparedEnvironment,
        opts: ProvisionOpts,
    ) -> Result<EnvironmentHandle, super::EnvironmentProvisionError> {
        prepared.get::<()>(&self.owner)?;
        if !opts.tokens.is_empty() || !opts.provisioned_mounts.is_empty() || !opts.tools.is_empty() || opts.cpu_limit.is_some() {
            return Err("host-direct adoption cannot install tokens, mounts, tools, or resource limits".into());
        }
        let mut handles = self.handles.lock().expect("host adoption lock");
        handles.retain(|_, handle| !handle.detached.load(Ordering::Acquire));
        Ok(Arc::clone(handles.entry(id.clone()).or_insert_with(|| {
            Arc::new(HostHandle {
                backing_id: id.canonical_string(),
                detached: AtomicBool::new(false),
                id,
                runner: Arc::clone(&self.runner),
                environment: self.environment.clone(),
                image: ImageId::new(HOST_DIRECT_IMAGE_SENTINEL),
            })
        })) as EnvironmentHandle)
    }

    async fn list(&self) -> Result<Vec<EnvironmentHandle>, String> {
        Ok(self
            .handles
            .lock()
            .expect("host adoption lock")
            .values()
            .filter(|handle| !handle.detached.load(Ordering::Acquire))
            .map(|handle| Arc::clone(handle) as EnvironmentHandle)
            .collect())
    }

    async fn destroy(&self, identity: &str) -> Result<(), String> {
        self.handles.lock().expect("host adoption lock").retain(|id, handle| {
            let matches = handle.backing_id == identity || id.as_str() == identity;
            if matches {
                handle.detached.store(true, Ordering::Release);
            }
            !matches
        });
        Ok(())
    }
}

struct HostHandle {
    backing_id: String,
    detached: AtomicBool,
    id: EnvironmentId,
    runner: Arc<dyn CommandRunner>,
    environment: HashMap<String, String>,
    image: ImageId,
}

#[async_trait]
impl ProvisionedEnvironment for HostHandle {
    /// A host backing is the host itself.
    fn provider_descriptor(&self) -> Option<flotilla_resources::EnvironmentProviderDescriptor> {
        None
    }
    fn capabilities(&self) -> super::ProviderCapabilities {
        HostDirectEnvironmentProvider::capability_declaration()
    }
    fn backing_id(&self) -> &str {
        &self.backing_id
    }

    fn id(&self) -> &EnvironmentId {
        &self.id
    }
    fn image(&self) -> &ImageId {
        &self.image
    }
    fn container_name(&self) -> Option<&str> {
        None
    }
    fn provisioned_mounts(&self) -> Vec<ProvisionedMount> {
        Vec::new()
    }
    async fn observe(&self) -> super::ProviderObservation {
        super::ProviderObservation::new(if self.detached.load(Ordering::Acquire) {
            super::ObservationState::Gone { cause: super::GoneCause::DestroyedByRequest }
        } else {
            super::ObservationState::Running
        })
    }

    async fn env_vars(&self) -> Result<HashMap<String, String>, String> {
        Ok(self.environment.clone())
    }
    fn runner(&self) -> Arc<dyn CommandRunner> {
        Arc::clone(&self.runner)
    }
    /// The host is the vessel: its agent works in process, through the host runner.
    fn agent(&self) -> Arc<dyn super::VesselAgent> {
        Arc::new(super::vessel_ops::RunnerVesselAgent(Arc::clone(&self.runner)))
    }
    async fn destroy(&self) -> Result<(), String> {
        self.detached.store(true, Ordering::Release);
        Ok(())
    }
}

#[async_trait]
impl super::EnvironmentDetection for HostDirectEnvironmentProvider {
    async fn detect(
        &self,
        image: Option<&str>,
        context: &flotilla_provider_runtime::execution_context::ObservationContext,
        binary: &str,
        args: &[&str],
        scratch: &std::path::Path,
    ) -> Result<flotilla_provider_runtime::CommandOutput, String> {
        if image.is_some() {
            return Err("host-direct detection does not accept an image".into());
        }
        super::detection::host_detection(self.runner.clone(), context, binary, args, scratch).await
    }
}

impl HostDirectEnvironmentProvider {
    fn capability_declaration() -> super::ProviderCapabilities {
        super::ProviderCapabilities {
            material_modes: vec![super::MaterialMode::Frozen, super::MaterialMode::Pushed, super::MaterialMode::Shared],
            checkout_strategies: vec![
                super::CheckoutStrategy::HostWorktree,
                super::CheckoutStrategy::HostClone,
                super::CheckoutStrategy::CloneInVessel,
            ],
            workspace_root: "/".into(),
            fallback_runner: true,
            receives_host_tools: false,
            per_vessel: false,
            egress_route: super::EgressRoute::Open,
            ..Default::default()
        }
    }
}
