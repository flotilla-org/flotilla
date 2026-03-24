use std::{
    collections::HashMap,
    path::Path,
    sync::{Arc, Mutex},
};

use flotilla_protocol::{AgentHarness, AgentStatus, AttachableId};
use serde::{Deserialize, Serialize};

use crate::{config::flotilla_config_dir, path_context::DaemonHostPath};

/// Persisted state for a single agent instance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentEntry {
    pub harness: AgentHarness,
    pub status: AgentStatus,
    pub model: Option<String>,
    pub session_title: Option<String>,
    /// The agent's native session ID (e.g., Claude's `session_id`).
    pub session_id: Option<String>,
    pub last_event_epoch_secs: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRegistry {
    /// Primary index: attachable ID → agent state.
    #[serde(default)]
    pub agents: HashMap<AttachableId, AgentEntry>,
    /// Secondary index: native session ID → attachable ID.
    #[serde(default)]
    pub session_index: HashMap<String, AttachableId>,
}

pub trait AgentStateStoreApi: Send + Sync {
    fn registry(&self) -> &AgentRegistry;
    fn upsert(&mut self, attachable_id: AttachableId, entry: AgentEntry);
    fn remove(&mut self, attachable_id: &AttachableId);
    fn lookup_by_session_id(&self, session_id: &str) -> Option<&AttachableId>;
    fn get(&self, attachable_id: &AttachableId) -> Option<&AgentEntry>;
    fn list_agents(&self) -> Vec<(AttachableId, AgentEntry)>;
    fn save(&self) -> Result<(), String>;
}

pub type SharedAgentStateStore = Arc<Mutex<Box<dyn AgentStateStoreApi>>>;

pub fn shared_agent_state_store<S>(store: S) -> SharedAgentStateStore
where
    S: AgentStateStoreApi + 'static,
{
    Arc::new(Mutex::new(Box::new(store)))
}

pub fn shared_file_backed_agent_state_store(base: &DaemonHostPath) -> SharedAgentStateStore {
    shared_agent_state_store(AgentStateStore::with_base(base))
}

pub fn shared_in_memory_agent_state_store() -> SharedAgentStateStore {
    shared_agent_state_store(InMemoryAgentStateStore::new())
}

// ---------- shared state logic ----------

#[derive(Debug, Clone, Default)]
struct AgentStoreState {
    registry: AgentRegistry,
}

impl AgentStoreState {
    fn from_registry(registry: AgentRegistry) -> Self {
        Self { registry }
    }

    fn registry(&self) -> &AgentRegistry {
        &self.registry
    }

    fn upsert(&mut self, attachable_id: AttachableId, entry: AgentEntry) {
        // Clean up stale session_index entry if session_id changed
        if let Some(existing) = self.registry.agents.get(&attachable_id) {
            if let Some(ref old_sid) = existing.session_id {
                if entry.session_id.as_ref() != Some(old_sid) {
                    self.registry.session_index.remove(old_sid);
                }
            }
        }
        if let Some(ref session_id) = entry.session_id {
            self.registry.session_index.insert(session_id.clone(), attachable_id.clone());
        }
        self.registry.agents.insert(attachable_id, entry);
    }

    fn remove(&mut self, attachable_id: &AttachableId) {
        if let Some(entry) = self.registry.agents.remove(attachable_id) {
            if let Some(ref session_id) = entry.session_id {
                self.registry.session_index.remove(session_id);
            }
        }
    }

    fn lookup_by_session_id(&self, session_id: &str) -> Option<&AttachableId> {
        self.registry.session_index.get(session_id)
    }

    fn get(&self, attachable_id: &AttachableId) -> Option<&AgentEntry> {
        self.registry.agents.get(attachable_id)
    }

    fn list_agents(&self) -> Vec<(AttachableId, AgentEntry)> {
        self.registry.agents.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
    }
}

// ---------- file-backed implementation ----------

pub struct AgentStateStore {
    path: DaemonHostPath,
    state: AgentStoreState,
}

impl AgentStateStore {
    pub fn new() -> Self {
        Self::with_path(flotilla_config_dir().join("agents").join("state.json"))
    }

    pub fn with_base(base: &DaemonHostPath) -> Self {
        Self::with_path(base.join("agents").join("state.json"))
    }

    pub fn with_path(path: DaemonHostPath) -> Self {
        let registry = match Self::load_registry(path.as_path()) {
            Ok(registry) => registry,
            Err(err) => {
                tracing::warn!(%path, err = %err, "failed to load agent state, starting empty");
                AgentRegistry::default()
            }
        };
        Self { path, state: AgentStoreState::from_registry(registry) }
    }

    fn load_registry(path: &Path) -> Result<AgentRegistry, String> {
        let contents = match std::fs::read_to_string(path) {
            Ok(contents) => contents,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(AgentRegistry::default()),
            Err(e) => return Err(format!("failed to read agent state: {e}")),
        };
        serde_json::from_str(&contents).map_err(|e| format!("failed to parse agent state: {e}"))
    }
}

impl Default for AgentStateStore {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentStateStoreApi for AgentStateStore {
    fn registry(&self) -> &AgentRegistry {
        self.state.registry()
    }

    fn upsert(&mut self, attachable_id: AttachableId, entry: AgentEntry) {
        self.state.upsert(attachable_id, entry);
    }

    fn remove(&mut self, attachable_id: &AttachableId) {
        self.state.remove(attachable_id);
    }

    fn lookup_by_session_id(&self, session_id: &str) -> Option<&AttachableId> {
        self.state.lookup_by_session_id(session_id)
    }

    fn get(&self, attachable_id: &AttachableId) -> Option<&AgentEntry> {
        self.state.get(attachable_id)
    }

    fn list_agents(&self) -> Vec<(AttachableId, AgentEntry)> {
        self.state.list_agents()
    }

    fn save(&self) -> Result<(), String> {
        if let Some(parent) = self.path.as_path().parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("failed to create agent state dir: {e}"))?;
        }
        let json = serde_json::to_string_pretty(self.state.registry()).map_err(|e| format!("failed to serialize agent state: {e}"))?;
        std::fs::write(self.path.as_path(), json).map_err(|e| format!("failed to write agent state: {e}"))?;
        Ok(())
    }
}

// ---------- in-memory implementation ----------

#[derive(Default)]
pub struct InMemoryAgentStateStore {
    state: AgentStoreState,
}

impl InMemoryAgentStateStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_registry(registry: AgentRegistry) -> Self {
        Self { state: AgentStoreState::from_registry(registry) }
    }
}

impl AgentStateStoreApi for InMemoryAgentStateStore {
    fn registry(&self) -> &AgentRegistry {
        self.state.registry()
    }

    fn upsert(&mut self, attachable_id: AttachableId, entry: AgentEntry) {
        self.state.upsert(attachable_id, entry);
    }

    fn remove(&mut self, attachable_id: &AttachableId) {
        self.state.remove(attachable_id);
    }

    fn lookup_by_session_id(&self, session_id: &str) -> Option<&AttachableId> {
        self.state.lookup_by_session_id(session_id)
    }

    fn get(&self, attachable_id: &AttachableId) -> Option<&AgentEntry> {
        self.state.get(attachable_id)
    }

    fn list_agents(&self) -> Vec<(AttachableId, AgentEntry)> {
        self.state.list_agents()
    }

    fn save(&self) -> Result<(), String> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_base(dir: &tempfile::TempDir) -> DaemonHostPath {
        DaemonHostPath::new(dir.path())
    }

    fn sample_entry() -> AgentEntry {
        AgentEntry {
            harness: AgentHarness::ClaudeCode,
            status: AgentStatus::Active,
            model: Some("opus-4".into()),
            session_title: Some("Debug login flow".into()),
            session_id: Some("sess-abc".into()),
            last_event_epoch_secs: 1710000000,
        }
    }

    fn sample_entry_no_session() -> AgentEntry {
        AgentEntry {
            harness: AgentHarness::Codex,
            status: AgentStatus::Idle,
            model: None,
            session_title: None,
            session_id: None,
            last_event_epoch_secs: 1710000100,
        }
    }

    // --- contract tests run against both implementations ---

    fn contract_upsert_and_get(store: &mut dyn AgentStateStoreApi) {
        let id = AttachableId::new("att-1");
        let entry = sample_entry();

        store.upsert(id.clone(), entry.clone());

        let got = store.get(&id).expect("should find agent");
        assert_eq!(got, &entry);
    }

    fn contract_upsert_updates_existing(store: &mut dyn AgentStateStoreApi) {
        let id = AttachableId::new("att-1");
        store.upsert(id.clone(), sample_entry());

        let updated = AgentEntry { status: AgentStatus::Idle, last_event_epoch_secs: 1710000200, ..sample_entry() };
        store.upsert(id.clone(), updated.clone());

        let got = store.get(&id).expect("should find agent");
        assert_eq!(got.status, AgentStatus::Idle);
        assert_eq!(got.last_event_epoch_secs, 1710000200);
    }

    fn contract_upsert_cleans_stale_session_index(store: &mut dyn AgentStateStoreApi) {
        let id = AttachableId::new("att-1");
        store.upsert(id.clone(), sample_entry()); // session_id = "sess-abc"
        assert_eq!(store.lookup_by_session_id("sess-abc"), Some(&id));

        // Update with a different session_id
        let updated = AgentEntry { session_id: Some("sess-new".into()), ..sample_entry() };
        store.upsert(id.clone(), updated);

        // Old session_id should be removed, new one should resolve
        assert!(store.lookup_by_session_id("sess-abc").is_none());
        assert_eq!(store.lookup_by_session_id("sess-new"), Some(&id));
    }

    fn contract_remove_clears_agent_and_session_index(store: &mut dyn AgentStateStoreApi) {
        let id = AttachableId::new("att-1");
        store.upsert(id.clone(), sample_entry());

        assert!(store.lookup_by_session_id("sess-abc").is_some());
        store.remove(&id);

        assert!(store.get(&id).is_none());
        assert!(store.lookup_by_session_id("sess-abc").is_none());
    }

    fn contract_remove_nonexistent_is_noop(store: &mut dyn AgentStateStoreApi) {
        store.remove(&AttachableId::new("att-nope"));
        assert!(store.list_agents().is_empty());
    }

    fn contract_session_id_lookup(store: &mut dyn AgentStateStoreApi) {
        let id = AttachableId::new("att-1");
        store.upsert(id.clone(), sample_entry());

        assert_eq!(store.lookup_by_session_id("sess-abc"), Some(&id));
        assert!(store.lookup_by_session_id("sess-nope").is_none());
    }

    fn contract_session_id_lookup_absent_when_no_session_id(store: &mut dyn AgentStateStoreApi) {
        let id = AttachableId::new("att-2");
        store.upsert(id.clone(), sample_entry_no_session());

        assert!(store.lookup_by_session_id("").is_none());
        assert!(store.get(&id).is_some());
    }

    fn contract_list_agents_returns_all(store: &mut dyn AgentStateStoreApi) {
        store.upsert(AttachableId::new("att-1"), sample_entry());
        store.upsert(AttachableId::new("att-2"), sample_entry_no_session());

        let agents = store.list_agents();
        assert_eq!(agents.len(), 2);
    }

    fn contract_roundtrip_preserves_state(
        mut store: impl AgentStateStoreApi,
        reload: impl FnOnce(AgentRegistry) -> Box<dyn AgentStateStoreApi>,
    ) {
        let id = AttachableId::new("att-1");
        store.upsert(id.clone(), sample_entry());
        store.save().expect("save");

        let reloaded = reload(store.registry().clone());
        let got = reloaded.get(&id).expect("should find agent after reload");
        assert_eq!(got, &sample_entry());
        assert_eq!(reloaded.lookup_by_session_id("sess-abc"), Some(&id));
    }

    // --- in-memory contract tests ---

    #[test]
    fn in_memory_upsert_and_get() {
        contract_upsert_and_get(&mut InMemoryAgentStateStore::new());
    }

    #[test]
    fn in_memory_upsert_updates_existing() {
        contract_upsert_updates_existing(&mut InMemoryAgentStateStore::new());
    }

    #[test]
    fn in_memory_upsert_cleans_stale_session_index() {
        contract_upsert_cleans_stale_session_index(&mut InMemoryAgentStateStore::new());
    }

    #[test]
    fn in_memory_remove_clears_agent_and_session_index() {
        contract_remove_clears_agent_and_session_index(&mut InMemoryAgentStateStore::new());
    }

    #[test]
    fn in_memory_remove_nonexistent_is_noop() {
        contract_remove_nonexistent_is_noop(&mut InMemoryAgentStateStore::new());
    }

    #[test]
    fn in_memory_session_id_lookup() {
        contract_session_id_lookup(&mut InMemoryAgentStateStore::new());
    }

    #[test]
    fn in_memory_session_id_lookup_absent_when_no_session_id() {
        contract_session_id_lookup_absent_when_no_session_id(&mut InMemoryAgentStateStore::new());
    }

    #[test]
    fn in_memory_list_agents_returns_all() {
        contract_list_agents_returns_all(&mut InMemoryAgentStateStore::new());
    }

    #[test]
    fn in_memory_roundtrip_preserves_state() {
        contract_roundtrip_preserves_state(InMemoryAgentStateStore::new(), |registry| {
            Box::new(InMemoryAgentStateStore::from_registry(registry))
        });
    }

    // --- file-backed contract tests ---

    #[test]
    fn file_backed_upsert_and_get() {
        contract_upsert_and_get(&mut AgentStateStore::with_base(&temp_base(&tempfile::tempdir().expect("tempdir"))));
    }

    #[test]
    fn file_backed_upsert_updates_existing() {
        contract_upsert_updates_existing(&mut AgentStateStore::with_base(&temp_base(&tempfile::tempdir().expect("tempdir"))));
    }

    #[test]
    fn file_backed_upsert_cleans_stale_session_index() {
        contract_upsert_cleans_stale_session_index(&mut AgentStateStore::with_base(&temp_base(&tempfile::tempdir().expect("tempdir"))));
    }

    #[test]
    fn file_backed_remove_clears_agent_and_session_index() {
        contract_remove_clears_agent_and_session_index(&mut AgentStateStore::with_base(&temp_base(&tempfile::tempdir().expect("tempdir"))));
    }

    #[test]
    fn file_backed_remove_nonexistent_is_noop() {
        contract_remove_nonexistent_is_noop(&mut AgentStateStore::with_base(&temp_base(&tempfile::tempdir().expect("tempdir"))));
    }

    #[test]
    fn file_backed_session_id_lookup() {
        contract_session_id_lookup(&mut AgentStateStore::with_base(&temp_base(&tempfile::tempdir().expect("tempdir"))));
    }

    #[test]
    fn file_backed_session_id_lookup_absent_when_no_session_id() {
        contract_session_id_lookup_absent_when_no_session_id(&mut AgentStateStore::with_base(&temp_base(
            &tempfile::tempdir().expect("tempdir"),
        )));
    }

    #[test]
    fn file_backed_list_agents_returns_all() {
        contract_list_agents_returns_all(&mut AgentStateStore::with_base(&temp_base(&tempfile::tempdir().expect("tempdir"))));
    }

    #[test]
    fn file_backed_roundtrip_preserves_state() {
        let dir = tempfile::tempdir().expect("tempdir");
        contract_roundtrip_preserves_state(AgentStateStore::with_base(&temp_base(&dir)), |_| {
            Box::new(AgentStateStore::with_base(&temp_base(&dir)))
        });
    }

    #[test]
    fn empty_store_roundtrips() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = AgentStateStore::with_base(&temp_base(&dir));
        store.save().expect("save empty");

        let reloaded = AgentStateStore::with_base(&temp_base(&dir));
        assert!(reloaded.registry().agents.is_empty());
        assert!(reloaded.registry().session_index.is_empty());
    }

    #[test]
    fn corrupt_file_loads_as_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("agents").join("state.json");
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(&path, "{ not valid json").expect("write corrupt");

        let store = AgentStateStore::with_base(&temp_base(&dir));
        assert!(store.registry().agents.is_empty());
    }

    #[test]
    fn agent_entry_serde_roundtrip() {
        let entry = sample_entry();
        let json = serde_json::to_string(&entry).expect("serialize");
        let decoded: AgentEntry = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decoded, entry);
    }

    #[test]
    fn agent_registry_serde_roundtrip() {
        let mut registry = AgentRegistry::default();
        registry.agents.insert(AttachableId::new("att-1"), sample_entry());
        registry.session_index.insert("sess-abc".into(), AttachableId::new("att-1"));

        let json = serde_json::to_string(&registry).expect("serialize");
        let decoded: AgentRegistry = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decoded, registry);
    }
}
