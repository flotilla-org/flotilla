//! An in-memory drydock: the provider API over a table of environments and
//! exposures, with observation streams the test hands in.
use super::api::*;
use async_trait::async_trait;
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

pub(crate) struct MemoryApi {
    pub declaration: Mutex<ProviderDeclaration>,
    pub environments: Mutex<BTreeMap<String, Environment>>,
    pub inwards: Mutex<BTreeMap<(String, String), Inward>>,
    pub outwards: Mutex<BTreeMap<(String, String), Outward>>,
    /// Each observe call's cursor.
    pub cursors: Mutex<Vec<Option<String>>>,
    /// What each observe call returns; an empty queue answers a closed stream error.
    pub streams: tokio::sync::Mutex<tokio::sync::mpsc::Receiver<ObservationStream>>,
    /// Refuse each provision with this reason.
    pub refuse_provision: Mutex<Option<String>>,
    pub agent_socket: std::path::PathBuf,
    next: Mutex<u64>,
}

impl MemoryApi {
    /// An instance named `provider`, ready, and the sender its observe calls read.
    pub fn new(provider: &str, agent_socket: std::path::PathBuf) -> (Arc<Self>, tokio::sync::mpsc::Sender<ObservationStream>) {
        let (streams, receiver) = tokio::sync::mpsc::channel(4);
        let api = Self {
            declaration: Mutex::new(ProviderDeclaration {
                module: provider.into(),
                capabilities: Capabilities {
                    material_modes: vec![MaterialMode::Frozen, MaterialMode::Pushed, MaterialMode::Shared],
                    checkout_strategies: vec![CheckoutStrategy::HostWorktree, CheckoutStrategy::CloneInVessel],
                    workspace_root: "/workspace".into(),
                    fallback_runner: false,
                    host_filesystem: false,
                },
                readiness: Readiness::Ready,
            }),
            environments: Default::default(),
            inwards: Default::default(),
            outwards: Default::default(),
            cursors: Default::default(),
            streams: tokio::sync::Mutex::new(receiver),
            refuse_provision: Default::default(),
            agent_socket,
            next: Default::default(),
        };
        (Arc::new(api), streams)
    }

    fn environment(&self, provider: &str, id: &str) -> ApiResult<Environment> {
        self.environments
            .lock()
            .expect("environments")
            .get(id)
            .filter(|environment| environment.provider == provider)
            .cloned()
            .ok_or_else(|| ApiError::not_found(format!("environment {id}")))
    }
}

#[async_trait]
impl ProviderApi for MemoryApi {
    async fn hello(&self) -> ApiResult<Hello> {
        let module = self.declaration.lock().expect("declaration").module.clone();
        Ok(Hello {
            compatible: true,
            host_version: "memory-1".into(),
            loadable_modules: vec![LoadableModule { name: module, version: "module-1".into() }],
        })
    }
    async fn provider(&self, provider: &str) -> ApiResult<ProviderDeclaration> {
        let declaration = self.declaration.lock().expect("declaration").clone();
        if declaration.module != provider {
            return Err(ApiError::not_found(format!("provider {provider}")));
        }
        Ok(declaration)
    }
    async fn prepare(&self, provider: &str, request: &PrepareRequest) -> ApiResult<Prepared> {
        Ok(Prepared {
            provider: provider.into(),
            id: format!("prepared-{}", request.image_digest.len()),
            platform: request.platform.clone().unwrap_or_else(|| Platform::new("linux", "amd64")),
            image_digest: request.image_digest.clone(),
            tools_digest: request.tools_digest.clone(),
        })
    }
    async fn provision(&self, provider: &str, request: &ProvisionRequest) -> ApiResult<Provisioned> {
        if let Some(reason) = self.refuse_provision.lock().expect("refusal").clone() {
            return Err(ApiError::protocol(ProtocolErrorKind::Refused, "refused", reason));
        }
        let mut environments = self.environments.lock().expect("environments");
        if let Some(existing) = environments.values().find(|e| e.provider == provider && e.label.as_deref() == Some(&request.label)) {
            return Ok(Provisioned::Existing(existing.clone()));
        }
        let mut next = self.next.lock().expect("next");
        *next += 1;
        let environment = Environment {
            id: format!("backing-{next}"),
            label: Some(request.label.clone()),
            provider: provider.into(),
            state: Observation::Running,
            platform: Some(request.prepared.platform.clone()),
        };
        environments.insert(environment.id.clone(), environment.clone());
        Ok(Provisioned::Created(environment))
    }
    async fn list(&self, provider: &str) -> ApiResult<Vec<Environment>> {
        Ok(self.environments.lock().expect("environments").values().filter(|e| e.provider == provider).cloned().collect())
    }
    async fn inspect(&self, provider: &str, id: &str) -> ApiResult<Environment> {
        self.environment(provider, id)
    }
    async fn destroy(&self, _: &str, id: &str) -> ApiResult<()> {
        self.environments.lock().expect("environments").remove(id);
        Ok(())
    }
    async fn observe(&self, since: Option<&str>) -> ApiResult<ObservationStream> {
        self.cursors.lock().expect("cursors").push(since.map(str::to_owned));
        self.streams.lock().await.recv().await.ok_or_else(|| ApiError::Other("observation stream closed".into()))
    }
    async fn put_inward(&self, provider: &str, id: &str, name: &str, request: &InwardRequest) -> ApiResult<Inward> {
        self.environment(provider, id)?;
        // The agent's exposure reaches the stand-in agent's socket; any other is a path of its own.
        let local = match &request.target {
            InwardTarget::Unix(_) if name == "agent" => self.agent_socket.display().to_string(),
            _ => format!("/exposures/{id}/{name}.sock"),
        };
        let inward = Inward { target: request.target.clone(), local: Local::Unix(local), error: None };
        self.inwards.lock().expect("inwards").insert((id.into(), name.into()), inward.clone());
        Ok(inward)
    }
    async fn inward(&self, provider: &str, id: &str, name: &str) -> ApiResult<Inward> {
        self.environment(provider, id)?;
        self.inwards.lock().expect("inwards").get(&(id.into(), name.into())).cloned().ok_or_else(|| ApiError::not_found(name.to_owned()))
    }
    async fn put_outward(&self, provider: &str, id: &str, name: &str, request: &OutwardRequest) -> ApiResult<Outward> {
        self.environment(provider, id)?;
        let outward = Outward { guest: request.guest.clone(), destination: request.destination.clone(), ready: true, error: None };
        self.outwards.lock().expect("outwards").insert((id.into(), name.into()), outward.clone());
        Ok(outward)
    }
    async fn outward(&self, provider: &str, id: &str, name: &str) -> ApiResult<Outward> {
        self.environment(provider, id)?;
        self.outwards.lock().expect("outwards").get(&(id.into(), name.into())).cloned().ok_or_else(|| ApiError::not_found(name.to_owned()))
    }
    async fn reach_kind(&self, provider: &str, id: &str) -> ApiResult<ReachKind> {
        self.environment(provider, id)?;
        Ok(ReachKind::Ssh)
    }
    async fn relay(&self, provider: &str, id: &str) -> ApiResult<ByteStream> {
        self.environment(provider, id)?;
        Err(ApiError::protocol(ProtocolErrorKind::Refused, "refused", "this vessel's reach is not a relay"))
    }
}
