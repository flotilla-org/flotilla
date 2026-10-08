//! Host adoption is an environment lifecycle, with no backing to create or kill.
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use flotilla_protocol::{EnvironmentId, EnvironmentStatus, ImageId};

use super::{EnvironmentHandle, EnvironmentKind, EnvironmentProvider, PreparedEnvironment, ProvisionedEnvironment, ProvisionedMount};
use crate::providers::environment::{PrepareOpts, ProvisionOpts};
use crate::providers::CommandRunner;

/// Legacy handles require an image id even for image-free adoption.
const HOST_DIRECT_IMAGE_SENTINEL: &str = "host-direct:no-image";

pub struct HostDirectEnvironmentProvider {
    runner: Arc<dyn CommandRunner>,
    environment: HashMap<String, String>,
    owner: Arc<()>,
    handles: Mutex<HashMap<EnvironmentId, EnvironmentHandle>>,
}

impl HostDirectEnvironmentProvider {
    pub fn new(runner: Arc<dyn CommandRunner>, environment: HashMap<String, String>) -> Self {
        Self { runner, environment, owner: Arc::new(()), handles: Mutex::new(HashMap::new()) }
    }
}

#[async_trait]
impl EnvironmentProvider for HostDirectEnvironmentProvider {
    fn kind(&self) -> EnvironmentKind {
        EnvironmentKind::HostDirect
    }

    async fn prepare(&self, spec: &flotilla_resources::EnvironmentSpec, _opts: &PrepareOpts) -> Result<PreparedEnvironment, String> {
        if EnvironmentKind::of(spec)? != self.kind() {
            return Err("host-direct provider requires a host-direct spec".into());
        }
        Ok(PreparedEnvironment::new(&self.owner, ()))
    }

    async fn provision(&self, id: EnvironmentId, prepared: &PreparedEnvironment, opts: ProvisionOpts) -> Result<EnvironmentHandle, String> {
        prepared.get::<()>(&self.owner)?;
        if !opts.tokens.is_empty()
            || !opts.provisioned_mounts.is_empty()
            || !opts.tools.is_empty()
            || opts.cpu_limit.is_some()
            || opts.memory_policy != Default::default()
        {
            return Err("host-direct adoption cannot install tokens, mounts, tools, or resource limits".into());
        }
        let mut handles = self.handles.lock().expect("host adoption lock");
        Ok(Arc::clone(handles.entry(id.clone()).or_insert_with(|| {
            Arc::new(HostHandle {
                id,
                runner: Arc::clone(&self.runner),
                environment: self.environment.clone(),
                image: ImageId::new(HOST_DIRECT_IMAGE_SENTINEL),
            })
        })))
    }

    async fn list(&self) -> Result<Vec<EnvironmentHandle>, String> {
        Ok(self.handles.lock().expect("host adoption lock").values().cloned().collect())
    }

    async fn list_backings(&self) -> Result<Vec<super::EnvironmentBacking>, String> {
        Ok(Vec::new())
    }

    async fn destroy(&self, identity: &str) -> Result<(), String> {
        self.handles.lock().expect("host adoption lock").retain(|id, _| id.as_str() != identity);
        Ok(())
    }
}

struct HostHandle {
    id: EnvironmentId,
    runner: Arc<dyn CommandRunner>,
    environment: HashMap<String, String>,
    image: ImageId,
}

#[async_trait]
impl ProvisionedEnvironment for HostHandle {
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
    async fn status(&self) -> Result<EnvironmentStatus, String> {
        Ok(EnvironmentStatus::Running)
    }
    async fn env_vars(&self) -> Result<HashMap<String, String>, String> {
        Ok(self.environment.clone())
    }
    fn runner(&self) -> Arc<dyn CommandRunner> {
        Arc::clone(&self.runner)
    }
    async fn destroy(&self) -> Result<(), String> {
        Ok(())
    }
}
