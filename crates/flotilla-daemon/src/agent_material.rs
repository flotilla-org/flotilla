use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    sync::Arc,
};

use async_trait::async_trait;
use flotilla_core::providers::{
    discovery::EnvVars,
    environment::{ProvisionedMount, ProvisionedMountMode},
};
use flotilla_protocol::ResourceRef;
use flotilla_resources::{api_version, Environment, MaterialPoolSpec, MaterialPoolUnitSpec, Resource, ResourceBackend};
use tokio::fs;
use tracing::warn;

use crate::{
    material_pool::{MaterialLeaseOutcome, MaterialPoolManager},
    vessel_config::{agent_environment_fragment, Fragment},
};

const CODEX_ADAPTER_ID: &str = "codex";
const CODEX_POOL_REF: &str = "codex-login";
pub(crate) const CONTAINER_CODEX_HOME: &str = "/run/flotilla/codex";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AgentMaterialPreflight {
    pub(crate) command: String,
    pub(crate) args: Vec<String>,
    pub(crate) failure_context: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AgentMaterialDelivery {
    pub(crate) mount: ProvisionedMount,
    pub(crate) preflight: AgentMaterialPreflight,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AgentMaterialOutcome {
    NotRequired,
    Ready(AgentMaterialDelivery),
    Waiting { pool_ref: String, message: String },
}

#[async_trait]
trait AgentMaterialAdapter: Send + Sync {
    fn id(&self) -> &'static str;
    fn pool_ref(&self) -> &'static str;
    fn fragment(&self, environment: &BTreeMap<String, String>) -> Option<Fragment>;

    async fn prepare(&self, holder_ref: &ResourceRef, environment: &BTreeMap<String, String>) -> Result<AgentMaterialOutcome, String>;
}

pub(crate) struct AgentMaterialRegistry {
    namespace: String,
    pools: Arc<MaterialPoolManager>,
    adapters: BTreeMap<&'static str, Arc<dyn AgentMaterialAdapter>>,
}

impl AgentMaterialRegistry {
    pub(crate) fn new(backend: ResourceBackend, namespace: &str, env: Arc<dyn EnvVars>) -> Self {
        let pools = Arc::new(MaterialPoolManager::new(backend, namespace));
        let pool_dir = env
            .get("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/var/lib/flotilla"))
            .join(".config/flotilla/credentials/codex-pool");
        let codex: Arc<dyn AgentMaterialAdapter> =
            Arc::new(CodexMaterialAdapter::new(Arc::clone(&pools), pool_dir, cfg!(any(target_os = "linux", test))));
        Self { namespace: namespace.to_string(), pools, adapters: BTreeMap::from([(codex.id(), codex)]) }
    }

    pub(crate) async fn prepare(
        &self,
        environment_ref: &str,
        required_adapters: &BTreeSet<String>,
        environment: &BTreeMap<String, String>,
    ) -> Result<Vec<AgentMaterialDelivery>, AgentMaterialPrepareError> {
        let holder_ref = self.holder_ref(environment_ref);
        let mut deliveries = Vec::new();
        for adapter_id in required_adapters {
            let Some(adapter) = self.adapters.get(adapter_id.as_str()) else {
                continue;
            };
            match adapter.prepare(&holder_ref, environment).await {
                Ok(AgentMaterialOutcome::NotRequired) => {}
                Ok(AgentMaterialOutcome::Ready(delivery)) => deliveries.push(delivery),
                Ok(AgentMaterialOutcome::Waiting { pool_ref, message }) => {
                    return Err(AgentMaterialPrepareError::Waiting { pool_ref, message });
                }
                Err(message) => return Err(AgentMaterialPrepareError::Failed(message)),
            }
        }
        Ok(deliveries)
    }

    pub(crate) fn fragments(&self, required_adapters: &BTreeSet<String>, environment: &BTreeMap<String, String>) -> Vec<Fragment> {
        required_adapters
            .iter()
            .filter_map(|adapter_id| self.adapters.get(adapter_id.as_str()))
            .filter_map(|adapter| adapter.fragment(environment))
            .collect()
    }

    pub(crate) async fn release(&self, environment_ref: &str) -> Result<(), String> {
        self.pools.release_holder(&self.holder_ref(environment_ref)).await
    }

    pub(crate) async fn recover(&self, active_environment_refs: impl IntoIterator<Item = String>) -> Result<(), String> {
        let active = active_environment_refs.into_iter().map(|name| self.holder_ref(&name)).collect::<HashSet<_>>();
        let pool_refs = self.adapters.values().map(|adapter| adapter.pool_ref().to_string()).collect::<BTreeSet<_>>();
        self.pools.recover(&pool_refs, &active).await
    }

    fn holder_ref(&self, environment_ref: &str) -> ResourceRef {
        ResourceRef::new(api_version(Environment::API_PATHS), Environment::API_PATHS.kind, &self.namespace, environment_ref)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AgentMaterialPrepareError {
    Waiting { pool_ref: String, message: String },
    Failed(String),
}

struct CodexMaterialAdapter {
    pools: Arc<MaterialPoolManager>,
    pool_dir: PathBuf,
    supported: bool,
}

impl CodexMaterialAdapter {
    fn new(pools: Arc<MaterialPoolManager>, pool_dir: PathBuf, supported: bool) -> Self {
        Self { pools, pool_dir, supported }
    }

    async fn usable_units(&self) -> Result<BTreeMap<String, MaterialPoolUnitSpec>, String> {
        let mut entries = match fs::read_dir(&self.pool_dir).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
            Err(error) => return Err(format!("read Codex login pool {}: {error}", self.pool_dir.display())),
        };
        let mut numbered: BTreeMap<u64, PathBuf> = BTreeMap::new();
        while let Some(entry) =
            entries.next_entry().await.map_err(|error| format!("read Codex login pool {}: {error}", self.pool_dir.display()))?
        {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(number) = name.strip_prefix("slot-").and_then(|suffix| suffix.parse::<u64>().ok()) else {
                continue;
            };
            let path = entry.path();
            let auth_path = path.join("auth.json");
            let Ok(metadata) = fs::metadata(&auth_path).await else {
                continue;
            };
            if metadata.is_file() && metadata.len() > 0 && metadata.permissions().mode() & 0o777 == 0o600 {
                if let Some(kept_path) = numbered.get(&number) {
                    warn!(
                        slot_number = number,
                        kept_path = %kept_path.display(),
                        skipped_path = %path.display(),
                        "duplicate numeric Codex login slot; skipping directory"
                    );
                    continue;
                }
                numbered.insert(number, path);
            }
        }
        Ok(numbered
            .into_iter()
            .map(|(number, path)| (format!("slot-{number:020}"), MaterialPoolUnitSpec { directory: path.to_string_lossy().into_owned() }))
            .collect())
    }
}

#[async_trait]
impl AgentMaterialAdapter for CodexMaterialAdapter {
    fn id(&self) -> &'static str {
        CODEX_ADAPTER_ID
    }

    fn pool_ref(&self) -> &'static str {
        CODEX_POOL_REF
    }

    fn fragment(&self, environment: &BTreeMap<String, String>) -> Option<Fragment> {
        (!environment.contains_key("CODEX_HOME"))
            .then(|| agent_environment_fragment("CODEX_HOME", CONTAINER_CODEX_HOME, format!("agent-material/codex {CODEX_POOL_REF}")))
    }

    async fn prepare(&self, holder_ref: &ResourceRef, environment: &BTreeMap<String, String>) -> Result<AgentMaterialOutcome, String> {
        if environment.contains_key("CODEX_HOME") {
            return Ok(AgentMaterialOutcome::NotRequired);
        }
        if !self.supported {
            return Err("Codex login material delivery is supported only on Linux placement hosts".to_string());
        }

        let spec = MaterialPoolSpec { units: self.usable_units().await? };
        self.pools.reconcile_pool(CODEX_POOL_REF, &spec).await?;
        match self.pools.acquire(CODEX_POOL_REF, holder_ref).await? {
            MaterialLeaseOutcome::Leased { unit, .. } => Ok(AgentMaterialOutcome::Ready(AgentMaterialDelivery {
                mount: ProvisionedMount::new(PathBuf::from(&unit.directory), CONTAINER_CODEX_HOME, ProvisionedMountMode::Rw),
                preflight: AgentMaterialPreflight {
                    command: "codex".to_string(),
                    args: vec!["login".to_string(), "status".to_string()],
                    failure_context: "Codex login preflight failed".to_string(),
                },
            })),
            MaterialLeaseOutcome::Waiting { unit_count } => Ok(AgentMaterialOutcome::Waiting {
                pool_ref: CODEX_POOL_REF.to_string(),
                message: format!(
                    "waiting for codex login material; {unit_count} in pool, all leased; mint another unit to increase concurrency"
                ),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{io, path::Path, sync::Mutex};

    use flotilla_core::providers::discovery::test_support::TestEnvVars;
    use flotilla_protocol::NodeId;
    use flotilla_resources::InMemoryBackend;

    use super::*;

    #[derive(Clone)]
    struct LogCaptureWriter(Arc<Mutex<Vec<u8>>>);

    impl io::Write for LogCaptureWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().expect("log capture lock should be healthy").extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn write_named_slot(root: &Path, name: &str, number: u64) -> PathBuf {
        let slot = root.join(name);
        std::fs::create_dir_all(&slot).expect("create slot");
        let auth = slot.join("auth.json");
        std::fs::write(&auth, format!("{{\"slot\":{number}}}")).expect("write auth");
        std::fs::set_permissions(&auth, std::fs::Permissions::from_mode(0o600)).expect("protect auth");
        slot
    }

    fn write_slot(root: &Path, number: u64) -> PathBuf {
        write_named_slot(root, &format!("slot-{number}"), number)
    }

    fn registry(home: &Path) -> AgentMaterialRegistry {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("root-a"));
        AgentMaterialRegistry::new(backend, "flotilla", Arc::new(TestEnvVars::new([("HOME", home.to_string_lossy().into_owned())])))
    }

    #[tokio::test]
    async fn codex_adapter_specializes_a_generic_lease_as_codex_home() {
        let temp = tempfile::tempdir().expect("tempdir");
        let slot = write_slot(&temp.path().join(".config/flotilla/credentials/codex-pool"), 0);
        let registry = registry(temp.path());

        let deliveries =
            registry.prepare("env-a", &BTreeSet::from([CODEX_ADAPTER_ID.to_string()]), &BTreeMap::new()).await.expect("prepare");

        assert_eq!(deliveries.len(), 1);
        assert_eq!(deliveries[0].mount, ProvisionedMount::new(slot, CONTAINER_CODEX_HOME, ProvisionedMountMode::Rw));
        let composed = crate::vessel_config::compose(
            crate::vessel_config::TargetId::AgentEnvironment,
            registry.fragments(&BTreeSet::from([CODEX_ADAPTER_ID.to_string()]), &BTreeMap::new()),
        )
        .expect("compose Codex home");
        assert_eq!(composed.environment, vec![("CODEX_HOME".to_string(), CONTAINER_CODEX_HOME.to_string())]);
        assert!(composed.contents.contains("# fragment: agent-material/codex codex-login"));
    }

    #[tokio::test]
    async fn duplicate_numeric_codex_slots_keep_one_unit_and_warn_with_both_paths() {
        let temp = tempfile::tempdir().expect("tempdir");
        let pool_dir = temp.path().join("codex-pool");
        let slot_zero = write_named_slot(&pool_dir, "slot-0", 0);
        let slot_zero_padded = write_named_slot(&pool_dir, "slot-00", 0);
        let backend = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("root-a"));
        let adapter = CodexMaterialAdapter::new(Arc::new(MaterialPoolManager::new(backend, "flotilla")), pool_dir, true);
        let log_output = Arc::new(Mutex::new(Vec::new()));

        let units = {
            let writer = LogCaptureWriter(Arc::clone(&log_output));
            let subscriber = tracing_subscriber::fmt()
                .without_time()
                .with_ansi(false)
                .with_target(false)
                .with_max_level(tracing::Level::WARN)
                .with_writer(move || writer.clone())
                .finish();
            let _guard = tracing::subscriber::set_default(subscriber);
            adapter.usable_units().await.expect("discover usable units")
        };

        assert_eq!(units.len(), 1);
        assert_eq!(units.keys().map(String::as_str).collect::<Vec<_>>(), ["slot-00000000000000000000"]);
        let logs = String::from_utf8(log_output.lock().expect("log capture lock should be healthy").clone()).expect("logs should be utf-8");
        assert!(logs.contains("duplicate numeric Codex login slot; skipping directory"), "missing duplicate warning: {logs}");
        assert!(logs.contains(&slot_zero.to_string_lossy().into_owned()), "warning should name slot-0: {logs}");
        assert!(logs.contains(&slot_zero_padded.to_string_lossy().into_owned()), "warning should name slot-00: {logs}");
    }

    #[tokio::test]
    async fn codex_adapter_waits_on_exhaustion_and_reconciles_new_units() {
        let temp = tempfile::tempdir().expect("tempdir");
        let pool = temp.path().join(".config/flotilla/credentials/codex-pool");
        write_slot(&pool, 0);
        let registry = registry(temp.path());
        let required = BTreeSet::from([CODEX_ADAPTER_ID.to_string()]);

        registry.prepare("env-a", &required, &BTreeMap::new()).await.expect("first lease");
        assert!(matches!(
            registry.prepare("env-b", &required, &BTreeMap::new()).await,
            Err(AgentMaterialPrepareError::Waiting { pool_ref, .. }) if pool_ref == CODEX_POOL_REF
        ));

        let second = write_slot(&pool, 1);
        let delivery = registry.prepare("env-b", &required, &BTreeMap::new()).await.expect("lease new unit");
        assert_eq!(delivery[0].mount, ProvisionedMount::new(second, CONTAINER_CODEX_HOME, ProvisionedMountMode::Rw));
    }

    #[tokio::test]
    async fn codex_specific_validation_stays_in_the_adapter() {
        let temp = tempfile::tempdir().expect("tempdir");
        let pool = temp.path().join(".config/flotilla/credentials/codex-pool");
        let slot = write_slot(&pool, 0);
        std::fs::set_permissions(slot.join("auth.json"), std::fs::Permissions::from_mode(0o644)).expect("weaken auth");
        let registry = registry(temp.path());

        assert!(matches!(
            registry.prepare("env-a", &BTreeSet::from([CODEX_ADAPTER_ID.to_string()]), &BTreeMap::new()).await,
            Err(AgentMaterialPrepareError::Waiting { .. })
        ));
    }

    #[tokio::test]
    async fn codex_adapter_defers_to_an_existing_codex_home() {
        let temp = tempfile::tempdir().expect("tempdir");
        let registry = registry(temp.path());
        let required = BTreeSet::from([CODEX_ADAPTER_ID.to_string()]);

        assert!(registry
            .prepare("env-home", &required, &BTreeMap::from([("CODEX_HOME".to_string(), "/image/codex".to_string())]))
            .await
            .expect("existing home")
            .is_empty());
    }
}
