use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use flotilla_protocol::{HostName, HostPath, TerminalStatus};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::types::{
    Attachable, AttachableId, AttachableKind, AttachableSet, AttachableSetId, BindingObjectKind, ProviderBinding, TerminalPurpose,
};
use crate::config::flotilla_config_dir;

type BindingKey = (String, String, BindingObjectKind, String);

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachableRegistry {
    #[serde(default)]
    pub sets: IndexMap<AttachableSetId, AttachableSet>,
    #[serde(default)]
    pub attachables: IndexMap<AttachableId, Attachable>,
    #[serde(default)]
    pub bindings: Vec<ProviderBinding>,
}

pub struct AttachableStore {
    path: PathBuf,
    registry: AttachableRegistry,
    binding_index: HashMap<BindingKey, String>,
}

pub type SharedAttachableStore = Arc<Mutex<AttachableStore>>;

impl AttachableStore {
    pub fn new() -> Self {
        Self::with_path(flotilla_config_dir().join("attachables").join("registry.json"))
    }

    pub fn with_base(base: impl AsRef<Path>) -> Self {
        Self::with_path(base.as_ref().join("attachables").join("registry.json"))
    }

    pub fn with_path(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let registry = Self::load_registry(&path).unwrap_or_default();
        let binding_index = Self::build_binding_index(&registry);
        Self { path, registry, binding_index }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn binding_count(&self) -> usize {
        self.registry.bindings.len()
    }

    pub fn registry(&self) -> &AttachableRegistry {
        &self.registry
    }

    pub fn allocate_set_id(&self) -> AttachableSetId {
        AttachableSetId(Uuid::new_v4().to_string())
    }

    pub fn allocate_attachable_id(&self) -> AttachableId {
        AttachableId(Uuid::new_v4().to_string())
    }

    pub fn insert_set(&mut self, set: AttachableSet) {
        self.registry.sets.insert(set.id.clone(), set);
    }

    pub fn insert_attachable(&mut self, attachable: Attachable) {
        self.registry.attachables.insert(attachable.id.clone(), attachable);
    }

    pub fn ensure_terminal_set(&mut self, host_affinity: Option<HostName>, checkout: Option<HostPath>) -> AttachableSetId {
        if let Some(existing) = self.registry.sets.values().find(|set| set.host_affinity == host_affinity && set.checkout == checkout) {
            return existing.id.clone();
        }

        let id = self.allocate_set_id();
        self.insert_set(AttachableSet { id: id.clone(), host_affinity, checkout, template_identity: None, members: Vec::new() });
        id
    }

    pub fn ensure_terminal_attachable(
        &mut self,
        set_id: &AttachableSetId,
        provider_category: &str,
        provider_name: &str,
        external_ref: &str,
        terminal_purpose: TerminalPurpose,
        command: impl Into<String>,
        working_directory: PathBuf,
        status: TerminalStatus,
    ) -> AttachableId {
        if let Some(existing_id) = self.lookup_binding(provider_category, provider_name, BindingObjectKind::Attachable, external_ref) {
            let attachable_id = AttachableId(existing_id.to_string());
            let old_set_id = self.registry.attachables.get(&attachable_id).map(|existing| existing.set_id.clone());
            if let Some(existing) = self.registry.attachables.get_mut(&attachable_id) {
                existing.set_id = set_id.clone();
                existing.kind = AttachableKind::Terminal;
                existing.terminal_purpose = Some(terminal_purpose);
                existing.command = command.into();
                existing.working_directory = working_directory;
                existing.status = status;
            }
            if let Some(old_set_id) = old_set_id.filter(|old| old != set_id) {
                self.remove_member_link(&old_set_id, &attachable_id);
            }
            self.ensure_member_link(set_id, &attachable_id);
            return attachable_id;
        }

        let id = self.allocate_attachable_id();
        self.insert_attachable(Attachable {
            id: id.clone(),
            set_id: set_id.clone(),
            kind: AttachableKind::Terminal,
            terminal_purpose: Some(terminal_purpose),
            command: command.into(),
            working_directory,
            status,
        });
        self.ensure_member_link(set_id, &id);
        self.replace_binding(ProviderBinding {
            provider_category: provider_category.to_string(),
            provider_name: provider_name.to_string(),
            object_kind: BindingObjectKind::Attachable,
            object_id: id.to_string(),
            external_ref: external_ref.to_string(),
        });
        id
    }

    pub fn replace_binding(&mut self, binding: ProviderBinding) {
        let key = Self::binding_key(&binding.provider_category, &binding.provider_name, &binding.object_kind, &binding.external_ref);
        self.binding_index.insert(key, binding.object_id.clone());
        self.registry.bindings.retain(|existing| {
            !(existing.provider_category == binding.provider_category
                && existing.provider_name == binding.provider_name
                && existing.object_kind == binding.object_kind
                && existing.external_ref == binding.external_ref)
        });
        self.registry.bindings.push(binding);
    }

    pub fn lookup_binding(
        &self,
        provider_category: &str,
        provider_name: &str,
        object_kind: BindingObjectKind,
        external_ref: &str,
    ) -> Option<&str> {
        let key = Self::binding_key(provider_category, provider_name, &object_kind, external_ref);
        self.binding_index.get(&key).map(String::as_str)
    }

    pub fn save(&self) -> Result<(), String> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("failed to create attachable dir: {e}"))?;
        }
        let json = serde_json::to_string_pretty(&self.registry).map_err(|e| format!("failed to serialize attachable registry: {e}"))?;
        std::fs::write(&self.path, json).map_err(|e| format!("failed to write attachable registry: {e}"))?;
        Ok(())
    }

    fn load_registry(path: &Path) -> Result<AttachableRegistry, String> {
        let contents = match std::fs::read_to_string(path) {
            Ok(contents) => contents,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(AttachableRegistry::default()),
            Err(e) => return Err(format!("failed to read attachable registry: {e}")),
        };
        serde_json::from_str(&contents).map_err(|e| format!("failed to parse attachable registry: {e}"))
    }

    fn build_binding_index(registry: &AttachableRegistry) -> HashMap<BindingKey, String> {
        registry
            .bindings
            .iter()
            .map(|binding| {
                (
                    Self::binding_key(&binding.provider_category, &binding.provider_name, &binding.object_kind, &binding.external_ref),
                    binding.object_id.clone(),
                )
            })
            .collect()
    }

    fn binding_key(provider_category: &str, provider_name: &str, object_kind: &BindingObjectKind, external_ref: &str) -> BindingKey {
        (provider_category.to_string(), provider_name.to_string(), object_kind.clone(), external_ref.to_string())
    }

    fn ensure_member_link(&mut self, set_id: &AttachableSetId, attachable_id: &AttachableId) {
        if let Some(set) = self.registry.sets.get_mut(set_id) {
            if !set.members.contains(attachable_id) {
                set.members.push(attachable_id.clone());
            }
        }
    }

    fn remove_member_link(&mut self, set_id: &AttachableSetId, attachable_id: &AttachableId) {
        if let Some(set) = self.registry.sets.get_mut(set_id) {
            set.members.retain(|member| member != attachable_id);
        }
    }
}

impl Default for AttachableStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::attachable::types::TerminalPurpose;

    #[test]
    fn opaque_ids_roundtrip_as_strings() {
        let set_id = AttachableSetId("set-123".into());
        let attachable_id = AttachableId("att-456".into());

        let set_json = serde_json::to_string(&set_id).expect("serialize set id");
        let attachable_json = serde_json::to_string(&attachable_id).expect("serialize attachable id");

        assert_eq!(set_json, "\"set-123\"");
        assert_eq!(attachable_json, "\"att-456\"");
        assert_eq!(serde_json::from_str::<AttachableSetId>(&set_json).expect("deserialize"), set_id);
        assert_eq!(serde_json::from_str::<AttachableId>(&attachable_json).expect("deserialize"), attachable_id);
    }

    #[test]
    fn empty_registry_roundtrips() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = AttachableStore::with_base(dir.path());
        store.save().expect("save empty registry");

        let reloaded = AttachableStore::with_base(dir.path());
        assert_eq!(reloaded.registry(), store.registry());
        assert!(reloaded.registry().sets.is_empty());
        assert!(reloaded.registry().attachables.is_empty());
        assert!(reloaded.registry().bindings.is_empty());
    }

    #[test]
    fn registry_roundtrip_rebuilds_binding_index() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut store = AttachableStore::with_base(dir.path());

        let set_id = AttachableSetId("set-1".into());
        let attachable_id = AttachableId("att-1".into());

        store.insert_set(AttachableSet {
            id: set_id.clone(),
            host_affinity: Some(HostName::new("desktop")),
            checkout: Some(HostPath::new(HostName::new("desktop"), "/repo/wt-feat")),
            template_identity: Some("default".into()),
            members: vec![attachable_id.clone()],
        });
        store.insert_attachable(Attachable {
            id: attachable_id.clone(),
            set_id: set_id.clone(),
            kind: AttachableKind::Terminal,
            terminal_purpose: Some(TerminalPurpose { checkout: "feat".into(), role: "shell".into(), index: 0 }),
            command: "claude".into(),
            working_directory: PathBuf::from("/repo/wt-feat"),
            status: TerminalStatus::Running,
        });
        store.replace_binding(ProviderBinding {
            provider_category: "terminal_pool".into(),
            provider_name: "shpool".into(),
            object_kind: BindingObjectKind::Attachable,
            object_id: attachable_id.to_string(),
            external_ref: "flotilla/feat/shell/0".into(),
        });
        store.replace_binding(ProviderBinding {
            provider_category: "workspace_manager".into(),
            provider_name: "tmux".into(),
            object_kind: BindingObjectKind::AttachableSet,
            object_id: set_id.to_string(),
            external_ref: "workspace:7".into(),
        });

        store.save().expect("save populated registry");

        let reloaded = AttachableStore::with_base(dir.path());
        assert_eq!(reloaded.registry(), store.registry());
        assert_eq!(
            reloaded.lookup_binding("terminal_pool", "shpool", BindingObjectKind::Attachable, "flotilla/feat/shell/0"),
            Some("att-1")
        );
        assert_eq!(reloaded.lookup_binding("workspace_manager", "tmux", BindingObjectKind::AttachableSet, "workspace:7"), Some("set-1"));
    }

    #[test]
    fn ensure_terminal_attachable_reuses_existing_binding() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut store = AttachableStore::with_base(dir.path());
        let set_id =
            store.ensure_terminal_set(Some(HostName::new("desktop")), Some(HostPath::new(HostName::new("desktop"), "/repo/wt-feat")));

        let first = store.ensure_terminal_attachable(
            &set_id,
            "terminal_pool",
            "shpool",
            "flotilla/feat/shell/0",
            TerminalPurpose { checkout: "feat".into(), role: "shell".into(), index: 0 },
            "claude",
            PathBuf::from("/repo/wt-feat"),
            TerminalStatus::Running,
        );
        let second = store.ensure_terminal_attachable(
            &set_id,
            "terminal_pool",
            "shpool",
            "flotilla/feat/shell/0",
            TerminalPurpose { checkout: "feat".into(), role: "shell".into(), index: 0 },
            "codex",
            PathBuf::from("/repo/wt-feat"),
            TerminalStatus::Disconnected,
        );

        assert_eq!(first, second);
        assert_eq!(store.registry.attachables.len(), 1);
        let attachable = store.registry.attachables.get(&first).expect("attachable");
        assert_eq!(attachable.command, "codex");
        assert_eq!(attachable.status, TerminalStatus::Disconnected);
    }

    #[test]
    fn ensure_terminal_set_groups_members_by_host_and_checkout() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut store = AttachableStore::with_base(dir.path());
        let host = HostName::new("desktop");
        let checkout = HostPath::new(host.clone(), "/repo/wt-feat");

        let set_a = store.ensure_terminal_set(Some(host.clone()), Some(checkout.clone()));
        let set_b = store.ensure_terminal_set(Some(host.clone()), Some(checkout.clone()));
        assert_eq!(set_a, set_b);

        let shell = store.ensure_terminal_attachable(
            &set_a,
            "terminal_pool",
            "shpool",
            "flotilla/feat/shell/0",
            TerminalPurpose { checkout: "feat".into(), role: "shell".into(), index: 0 },
            "claude",
            PathBuf::from("/repo/wt-feat"),
            TerminalStatus::Running,
        );
        let agent = store.ensure_terminal_attachable(
            &set_a,
            "terminal_pool",
            "shpool",
            "flotilla/feat/agent/0",
            TerminalPurpose { checkout: "feat".into(), role: "agent".into(), index: 0 },
            "codex",
            PathBuf::from("/repo/wt-feat"),
            TerminalStatus::Running,
        );

        let set = store.registry.sets.get(&set_a).expect("set");
        assert_eq!(set.members, vec![shell, agent]);
        assert_eq!(store.registry.sets.len(), 1);
    }

    #[test]
    fn ensure_terminal_attachable_uses_binding_as_primary_identity() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut store = AttachableStore::with_base(dir.path());
        let set_id =
            store.ensure_terminal_set(Some(HostName::new("desktop")), Some(HostPath::new(HostName::new("desktop"), "/repo/wt-feat")));

        let first = store.ensure_terminal_attachable(
            &set_id,
            "terminal_pool",
            "shpool",
            "flotilla/feat/shell/0",
            TerminalPurpose { checkout: "feat".into(), role: "shell".into(), index: 0 },
            "claude",
            PathBuf::from("/repo/wt-feat"),
            TerminalStatus::Running,
        );
        let second = store.ensure_terminal_attachable(
            &set_id,
            "terminal_pool",
            "shpool",
            "flotilla/feat/shell/1",
            TerminalPurpose { checkout: "feat".into(), role: "shell".into(), index: 0 },
            "claude",
            PathBuf::from("/repo/wt-feat"),
            TerminalStatus::Running,
        );

        assert_ne!(first, second);
        assert_eq!(store.registry.attachables.len(), 2);
        assert_eq!(store.registry.attachables.get(&second).and_then(|a| a.terminal_purpose.as_ref()).map(|p| p.index), Some(0));
    }

    #[test]
    fn corrupt_registry_file_loads_as_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("attachables").join("registry.json");
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(&path, "{ not valid json").expect("write corrupt registry");

        let store = AttachableStore::with_base(dir.path());
        assert!(store.registry().sets.is_empty());
        assert!(store.registry().attachables.is_empty());
        assert!(store.registry().bindings.is_empty());
    }

    #[test]
    fn replacing_binding_is_deterministic() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut store = AttachableStore::with_base(dir.path());

        store.replace_binding(ProviderBinding {
            provider_category: "workspace_manager".into(),
            provider_name: "cmux".into(),
            object_kind: BindingObjectKind::AttachableSet,
            object_id: "set-old".into(),
            external_ref: "workspace:1".into(),
        });
        store.replace_binding(ProviderBinding {
            provider_category: "workspace_manager".into(),
            provider_name: "cmux".into(),
            object_kind: BindingObjectKind::AttachableSet,
            object_id: "set-new".into(),
            external_ref: "workspace:1".into(),
        });

        assert_eq!(store.lookup_binding("workspace_manager", "cmux", BindingObjectKind::AttachableSet, "workspace:1"), Some("set-new"));
        assert_eq!(store.binding_count(), 1);
    }

    #[test]
    fn save_load_preserves_stable_ids() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut store = AttachableStore::with_base(dir.path());
        let host = HostName::new("desktop");
        let checkout = HostPath::new(host.clone(), "/repo/wt-feat");
        let set_id = store.ensure_terminal_set(Some(host), Some(checkout));
        let attachable_id = store.ensure_terminal_attachable(
            &set_id,
            "terminal_pool",
            "shpool",
            "flotilla/feat/shell/0",
            TerminalPurpose { checkout: "feat".into(), role: "shell".into(), index: 0 },
            "claude",
            PathBuf::from("/repo/wt-feat"),
            TerminalStatus::Running,
        );
        store.save().expect("save registry");

        let mut reloaded = AttachableStore::with_base(dir.path());
        let same_set_id =
            reloaded.ensure_terminal_set(Some(HostName::new("desktop")), Some(HostPath::new(HostName::new("desktop"), "/repo/wt-feat")));
        let same_attachable_id = reloaded.ensure_terminal_attachable(
            &same_set_id,
            "terminal_pool",
            "shpool",
            "flotilla/feat/shell/0",
            TerminalPurpose { checkout: "feat".into(), role: "shell".into(), index: 0 },
            "claude",
            PathBuf::from("/repo/wt-feat"),
            TerminalStatus::Running,
        );

        assert_eq!(same_set_id, set_id);
        assert_eq!(same_attachable_id, attachable_id);
    }

    #[test]
    fn provider_local_state_is_not_identity_source() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut store = AttachableStore::with_base(dir.path());
        let set_id =
            store.ensure_terminal_set(Some(HostName::new("desktop")), Some(HostPath::new(HostName::new("desktop"), "/repo/wt-feat")));
        let attachable_id = store.ensure_terminal_attachable(
            &set_id,
            "terminal_pool",
            "shpool",
            "flotilla/feat/shell/0",
            TerminalPurpose { checkout: "feat".into(), role: "shell".into(), index: 0 },
            "claude",
            PathBuf::from("/repo/wt-feat"),
            TerminalStatus::Running,
        );
        store.save().expect("save registry");

        std::fs::remove_file(store.path()).expect("remove persisted registry");

        let store = AttachableStore::with_base(dir.path());
        assert_ne!(
            store.lookup_binding("terminal_pool", "shpool", BindingObjectKind::Attachable, "flotilla/feat/shell/0"),
            Some(attachable_id.0.as_str())
        );
        assert!(store.registry().attachables.is_empty());
    }
}
