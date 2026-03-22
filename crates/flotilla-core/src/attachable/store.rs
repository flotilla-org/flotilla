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
    Attachable, AttachableContent, AttachableId, AttachableSet, AttachableSetId, BindingObjectKind, ProviderBinding, TerminalAttachable,
    TerminalPurpose,
};
use crate::config::flotilla_config_dir;

type BindingKey = (String, String, BindingObjectKind, String);

#[derive(Debug)]
pub struct RemovedSetInfo {
    pub member_binding_refs: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachableRegistry {
    #[serde(default)]
    pub sets: IndexMap<AttachableSetId, AttachableSet>,
    #[serde(default)]
    pub attachables: IndexMap<AttachableId, Attachable>,
    #[serde(default)]
    pub bindings: Vec<ProviderBinding>,
}

pub trait AttachableStoreApi: Send + Sync {
    fn binding_count(&self) -> usize;
    fn registry(&self) -> &AttachableRegistry;
    fn allocate_set_id(&self) -> AttachableSetId;
    fn allocate_attachable_id(&self) -> AttachableId;
    fn insert_set(&mut self, set: AttachableSet);
    fn insert_attachable(&mut self, attachable: Attachable);
    fn ensure_terminal_set(&mut self, host_affinity: Option<HostName>, checkout: Option<HostPath>) -> AttachableSetId;
    #[allow(clippy::too_many_arguments)]
    fn ensure_terminal_attachable(
        &mut self,
        set_id: &AttachableSetId,
        provider_category: &str,
        provider_name: &str,
        external_ref: &str,
        terminal_purpose: TerminalPurpose,
        command: &str,
        working_directory: PathBuf,
        status: TerminalStatus,
    ) -> AttachableId;
    fn ensure_terminal_set_with_change(&mut self, host_affinity: Option<HostName>, checkout: Option<HostPath>) -> (AttachableSetId, bool);
    #[allow(clippy::too_many_arguments)]
    fn ensure_terminal_attachable_with_change(
        &mut self,
        set_id: &AttachableSetId,
        provider_category: &str,
        provider_name: &str,
        external_ref: &str,
        terminal_purpose: TerminalPurpose,
        command: &str,
        working_directory: PathBuf,
        status: TerminalStatus,
    ) -> (AttachableId, bool);
    fn replace_binding(&mut self, binding: ProviderBinding) -> bool;
    fn lookup_binding(
        &self,
        provider_category: &str,
        provider_name: &str,
        object_kind: BindingObjectKind,
        external_ref: &str,
    ) -> Option<&str>;
    fn remove_binding_object(
        &mut self,
        provider_category: &str,
        provider_name: &str,
        object_kind: BindingObjectKind,
        external_ref: &str,
    ) -> bool;
    fn remove_set(&mut self, id: &AttachableSetId) -> Option<RemovedSetInfo>;
    fn sets_for_checkout(&self, checkout: &HostPath) -> Vec<AttachableSetId>;
    fn update_terminal_status(&mut self, id: &AttachableId, status: TerminalStatus) -> bool;
    fn save(&self) -> Result<(), String>;
}

pub type SharedAttachableStore = Arc<Mutex<dyn AttachableStoreApi>>;

pub fn shared_attachable_store<S>(store: S) -> SharedAttachableStore
where
    S: AttachableStoreApi + 'static,
{
    Arc::new(Mutex::new(store))
}

pub fn shared_file_backed_attachable_store(base: impl AsRef<Path>) -> SharedAttachableStore {
    shared_attachable_store(AttachableStore::with_base(base))
}

pub fn shared_in_memory_attachable_store() -> SharedAttachableStore {
    shared_attachable_store(InMemoryAttachableStore::new())
}

#[derive(Debug, Clone, Default)]
struct AttachableStoreState {
    registry: AttachableRegistry,
    binding_index: HashMap<BindingKey, String>,
}

impl AttachableStoreState {
    fn from_registry(registry: AttachableRegistry) -> Self {
        let binding_index = Self::build_binding_index(&registry);
        Self { registry, binding_index }
    }

    fn binding_count(&self) -> usize {
        self.registry.bindings.len()
    }

    fn registry(&self) -> &AttachableRegistry {
        &self.registry
    }

    fn allocate_set_id(&self) -> AttachableSetId {
        AttachableSetId::new(Uuid::new_v4().to_string())
    }

    fn allocate_attachable_id(&self) -> AttachableId {
        AttachableId::new(Uuid::new_v4().to_string())
    }

    fn insert_set(&mut self, set: AttachableSet) {
        self.registry.sets.insert(set.id.clone(), set);
    }

    fn insert_attachable(&mut self, attachable: Attachable) {
        self.registry.attachables.insert(attachable.id.clone(), attachable);
    }

    fn ensure_terminal_set(&mut self, host_affinity: Option<HostName>, checkout: Option<HostPath>) -> AttachableSetId {
        self.ensure_terminal_set_with_change(host_affinity, checkout).0
    }

    #[allow(clippy::too_many_arguments)]
    fn ensure_terminal_attachable(
        &mut self,
        set_id: &AttachableSetId,
        provider_category: &str,
        provider_name: &str,
        external_ref: &str,
        terminal_purpose: TerminalPurpose,
        command: &str,
        working_directory: PathBuf,
        status: TerminalStatus,
    ) -> AttachableId {
        self.ensure_terminal_attachable_with_change(
            set_id,
            provider_category,
            provider_name,
            external_ref,
            terminal_purpose,
            command,
            working_directory,
            status,
        )
        .0
    }

    fn ensure_terminal_set_with_change(&mut self, host_affinity: Option<HostName>, checkout: Option<HostPath>) -> (AttachableSetId, bool) {
        if let Some(existing) = self.registry.sets.values().find(|set| set.host_affinity == host_affinity && set.checkout == checkout) {
            return (existing.id.clone(), false);
        }

        let id = self.allocate_set_id();
        self.insert_set(AttachableSet { id: id.clone(), host_affinity, checkout, template_identity: None, members: Vec::new() });
        (id, true)
    }

    #[allow(clippy::too_many_arguments)]
    fn ensure_terminal_attachable_with_change(
        &mut self,
        set_id: &AttachableSetId,
        provider_category: &str,
        provider_name: &str,
        external_ref: &str,
        terminal_purpose: TerminalPurpose,
        command: &str,
        working_directory: PathBuf,
        status: TerminalStatus,
    ) -> (AttachableId, bool) {
        let new_content = AttachableContent::Terminal(TerminalAttachable {
            purpose: terminal_purpose,
            command: command.to_string(),
            working_directory,
            status,
        });
        if let Some(existing_id) = self.lookup_binding(provider_category, provider_name, BindingObjectKind::Attachable, external_ref) {
            let attachable_id = AttachableId::new(existing_id.to_string());
            let old_set_id = self.registry.attachables.get(&attachable_id).map(|existing| existing.set_id.clone());
            let mut changed = false;
            if let Some(existing) = self.registry.attachables.get_mut(&attachable_id) {
                if existing.set_id != *set_id {
                    existing.set_id = set_id.clone();
                    changed = true;
                }
                if existing.content != new_content {
                    existing.content = new_content;
                    changed = true;
                }
            }
            if let Some(old_set_id) = old_set_id.filter(|old| old != set_id) {
                changed |= self.remove_member_link(&old_set_id, &attachable_id);
            }
            changed |= self.ensure_member_link(set_id, &attachable_id);
            return (attachable_id, changed);
        }

        let id = self.allocate_attachable_id();
        self.insert_attachable(Attachable { id: id.clone(), set_id: set_id.clone(), content: new_content });
        let mut changed = true;
        changed |= self.ensure_member_link(set_id, &id);
        changed |= self.replace_binding(ProviderBinding {
            provider_category: provider_category.to_string(),
            provider_name: provider_name.to_string(),
            object_kind: BindingObjectKind::Attachable,
            object_id: id.to_string(),
            external_ref: external_ref.to_string(),
        });
        (id, changed)
    }

    fn replace_binding(&mut self, binding: ProviderBinding) -> bool {
        if self.registry.bindings.iter().any(|existing| existing == &binding) {
            return false;
        }
        let key = Self::binding_key(&binding.provider_category, &binding.provider_name, &binding.object_kind, &binding.external_ref);
        self.binding_index.insert(key, binding.object_id.clone());
        self.registry.bindings.retain(|existing| {
            !(existing.provider_category == binding.provider_category
                && existing.provider_name == binding.provider_name
                && existing.object_kind == binding.object_kind
                && existing.external_ref == binding.external_ref)
        });
        self.registry.bindings.push(binding);
        true
    }

    fn lookup_binding(
        &self,
        provider_category: &str,
        provider_name: &str,
        object_kind: BindingObjectKind,
        external_ref: &str,
    ) -> Option<&str> {
        let key = Self::binding_key(provider_category, provider_name, &object_kind, external_ref);
        self.binding_index.get(&key).map(String::as_str)
    }

    fn remove_binding_object(
        &mut self,
        provider_category: &str,
        provider_name: &str,
        object_kind: BindingObjectKind,
        external_ref: &str,
    ) -> bool {
        let key = Self::binding_key(provider_category, provider_name, &object_kind, external_ref);
        let Some(object_id) = self.binding_index.remove(&key) else {
            return false;
        };

        self.registry.bindings.retain(|binding| {
            !(binding.provider_category == provider_category
                && binding.provider_name == provider_name
                && binding.object_kind == object_kind
                && binding.external_ref == external_ref)
        });

        match object_kind {
            BindingObjectKind::Attachable => {
                let attachable_id = AttachableId::new(object_id);
                if let Some(attachable) = self.registry.attachables.shift_remove(&attachable_id) {
                    let _ = self.remove_member_link(&attachable.set_id, &attachable_id);
                }
            }
            BindingObjectKind::AttachableSet => {
                let set_id = AttachableSetId::new(object_id);
                self.registry.sets.shift_remove(&set_id);
            }
        }

        true
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

    fn ensure_member_link(&mut self, set_id: &AttachableSetId, attachable_id: &AttachableId) -> bool {
        if let Some(set) = self.registry.sets.get_mut(set_id) {
            if !set.members.contains(attachable_id) {
                set.members.push(attachable_id.clone());
                return true;
            }
        }
        false
    }

    fn remove_member_link(&mut self, set_id: &AttachableSetId, attachable_id: &AttachableId) -> bool {
        if let Some(set) = self.registry.sets.get_mut(set_id) {
            let original_len = set.members.len();
            set.members.retain(|member| member != attachable_id);
            return set.members.len() != original_len;
        }
        false
    }

    fn remove_set(&mut self, id: &AttachableSetId) -> Option<RemovedSetInfo> {
        let set = self.registry.sets.swap_remove(id)?;

        let mut member_binding_refs = Vec::new();
        let mut removed_object_ids: Vec<String> = vec![id.to_string()];

        for member_id in &set.members {
            self.registry.attachables.swap_remove(member_id);
            removed_object_ids.push(member_id.to_string());

            // Collect terminal_pool binding external refs for this member
            for binding in &self.registry.bindings {
                if binding.object_kind == BindingObjectKind::Attachable
                    && binding.object_id == member_id.to_string()
                    && binding.provider_category == "terminal_pool"
                {
                    member_binding_refs.push(binding.external_ref.clone());
                }
            }
        }

        // Remove all bindings referencing the set ID or any member ID
        self.registry.bindings.retain(|binding| !removed_object_ids.contains(&binding.object_id));

        // Rebuild the binding index
        self.binding_index = Self::build_binding_index(&self.registry);

        Some(RemovedSetInfo { member_binding_refs })
    }

    fn sets_for_checkout(&self, checkout: &HostPath) -> Vec<AttachableSetId> {
        self.registry.sets.values().filter(|set| set.checkout.as_ref() == Some(checkout)).map(|set| set.id.clone()).collect()
    }

    fn update_terminal_status(&mut self, id: &AttachableId, status: TerminalStatus) -> bool {
        if let Some(attachable) = self.registry.attachables.get_mut(id) {
            let AttachableContent::Terminal(ref mut terminal) = attachable.content;
            if terminal.status != status {
                terminal.status = status;
                return true;
            }
        }
        false
    }
}

pub struct AttachableStore {
    path: PathBuf,
    state: AttachableStoreState,
}

impl AttachableStore {
    pub fn new() -> Self {
        Self::with_path(flotilla_config_dir().join("attachables").join("registry.json"))
    }

    pub fn with_base(base: impl AsRef<Path>) -> Self {
        Self::with_path(base.as_ref().join("attachables").join("registry.json"))
    }

    pub fn with_path(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let registry = match Self::load_registry(&path) {
            Ok(registry) => registry,
            Err(err) => {
                tracing::warn!(path = %path.display(), err = %err, "failed to load attachable registry, starting empty");
                AttachableRegistry::default()
            }
        };
        Self { path, state: AttachableStoreState::from_registry(registry) }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn binding_count(&self) -> usize {
        self.state.binding_count()
    }

    pub fn registry(&self) -> &AttachableRegistry {
        self.state.registry()
    }

    pub fn allocate_set_id(&self) -> AttachableSetId {
        self.state.allocate_set_id()
    }

    pub fn allocate_attachable_id(&self) -> AttachableId {
        self.state.allocate_attachable_id()
    }

    pub fn insert_set(&mut self, set: AttachableSet) {
        self.state.insert_set(set);
    }

    pub fn insert_attachable(&mut self, attachable: Attachable) {
        self.state.insert_attachable(attachable);
    }

    pub fn ensure_terminal_set(&mut self, host_affinity: Option<HostName>, checkout: Option<HostPath>) -> AttachableSetId {
        self.state.ensure_terminal_set(host_affinity, checkout)
    }

    pub fn ensure_terminal_set_with_change(
        &mut self,
        host_affinity: Option<HostName>,
        checkout: Option<HostPath>,
    ) -> (AttachableSetId, bool) {
        self.state.ensure_terminal_set_with_change(host_affinity, checkout)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn ensure_terminal_attachable(
        &mut self,
        set_id: &AttachableSetId,
        provider_category: &str,
        provider_name: &str,
        external_ref: &str,
        terminal_purpose: TerminalPurpose,
        command: &str,
        working_directory: PathBuf,
        status: TerminalStatus,
    ) -> AttachableId {
        self.state.ensure_terminal_attachable(
            set_id,
            provider_category,
            provider_name,
            external_ref,
            terminal_purpose,
            command,
            working_directory,
            status,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn ensure_terminal_attachable_with_change(
        &mut self,
        set_id: &AttachableSetId,
        provider_category: &str,
        provider_name: &str,
        external_ref: &str,
        terminal_purpose: TerminalPurpose,
        command: &str,
        working_directory: PathBuf,
        status: TerminalStatus,
    ) -> (AttachableId, bool) {
        self.state.ensure_terminal_attachable_with_change(
            set_id,
            provider_category,
            provider_name,
            external_ref,
            terminal_purpose,
            command,
            working_directory,
            status,
        )
    }

    pub fn replace_binding(&mut self, binding: ProviderBinding) -> bool {
        self.state.replace_binding(binding)
    }

    pub fn lookup_binding(
        &self,
        provider_category: &str,
        provider_name: &str,
        object_kind: BindingObjectKind,
        external_ref: &str,
    ) -> Option<&str> {
        self.state.lookup_binding(provider_category, provider_name, object_kind, external_ref)
    }

    pub fn remove_binding_object(
        &mut self,
        provider_category: &str,
        provider_name: &str,
        object_kind: BindingObjectKind,
        external_ref: &str,
    ) -> bool {
        self.state.remove_binding_object(provider_category, provider_name, object_kind, external_ref)
    }

    pub fn remove_set(&mut self, id: &AttachableSetId) -> Option<RemovedSetInfo> {
        self.state.remove_set(id)
    }

    pub fn sets_for_checkout(&self, checkout: &HostPath) -> Vec<AttachableSetId> {
        self.state.sets_for_checkout(checkout)
    }

    pub fn update_terminal_status(&mut self, id: &AttachableId, status: TerminalStatus) -> bool {
        self.state.update_terminal_status(id, status)
    }

    pub fn save(&self) -> Result<(), String> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("failed to create attachable dir: {e}"))?;
        }
        let json =
            serde_json::to_string_pretty(self.state.registry()).map_err(|e| format!("failed to serialize attachable registry: {e}"))?;
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
}

impl Default for AttachableStore {
    fn default() -> Self {
        Self::new()
    }
}

impl AttachableStoreApi for AttachableStore {
    fn binding_count(&self) -> usize {
        self.state.binding_count()
    }

    fn registry(&self) -> &AttachableRegistry {
        self.state.registry()
    }

    fn allocate_set_id(&self) -> AttachableSetId {
        self.state.allocate_set_id()
    }

    fn allocate_attachable_id(&self) -> AttachableId {
        self.state.allocate_attachable_id()
    }

    fn insert_set(&mut self, set: AttachableSet) {
        self.state.insert_set(set);
    }

    fn insert_attachable(&mut self, attachable: Attachable) {
        self.state.insert_attachable(attachable);
    }

    fn ensure_terminal_set(&mut self, host_affinity: Option<HostName>, checkout: Option<HostPath>) -> AttachableSetId {
        self.state.ensure_terminal_set(host_affinity, checkout)
    }

    fn ensure_terminal_set_with_change(&mut self, host_affinity: Option<HostName>, checkout: Option<HostPath>) -> (AttachableSetId, bool) {
        self.state.ensure_terminal_set_with_change(host_affinity, checkout)
    }

    fn ensure_terminal_attachable(
        &mut self,
        set_id: &AttachableSetId,
        provider_category: &str,
        provider_name: &str,
        external_ref: &str,
        terminal_purpose: TerminalPurpose,
        command: &str,
        working_directory: PathBuf,
        status: TerminalStatus,
    ) -> AttachableId {
        self.state.ensure_terminal_attachable(
            set_id,
            provider_category,
            provider_name,
            external_ref,
            terminal_purpose,
            command,
            working_directory,
            status,
        )
    }

    fn ensure_terminal_attachable_with_change(
        &mut self,
        set_id: &AttachableSetId,
        provider_category: &str,
        provider_name: &str,
        external_ref: &str,
        terminal_purpose: TerminalPurpose,
        command: &str,
        working_directory: PathBuf,
        status: TerminalStatus,
    ) -> (AttachableId, bool) {
        self.state.ensure_terminal_attachable_with_change(
            set_id,
            provider_category,
            provider_name,
            external_ref,
            terminal_purpose,
            command,
            working_directory,
            status,
        )
    }

    fn replace_binding(&mut self, binding: ProviderBinding) -> bool {
        self.state.replace_binding(binding)
    }

    fn lookup_binding(
        &self,
        provider_category: &str,
        provider_name: &str,
        object_kind: BindingObjectKind,
        external_ref: &str,
    ) -> Option<&str> {
        self.state.lookup_binding(provider_category, provider_name, object_kind, external_ref)
    }

    fn remove_binding_object(
        &mut self,
        provider_category: &str,
        provider_name: &str,
        object_kind: BindingObjectKind,
        external_ref: &str,
    ) -> bool {
        self.state.remove_binding_object(provider_category, provider_name, object_kind, external_ref)
    }

    fn remove_set(&mut self, id: &AttachableSetId) -> Option<RemovedSetInfo> {
        self.state.remove_set(id)
    }

    fn sets_for_checkout(&self, checkout: &HostPath) -> Vec<AttachableSetId> {
        self.state.sets_for_checkout(checkout)
    }

    fn update_terminal_status(&mut self, id: &AttachableId, status: TerminalStatus) -> bool {
        self.state.update_terminal_status(id, status)
    }

    fn save(&self) -> Result<(), String> {
        AttachableStore::save(self)
    }
}

#[derive(Default)]
pub struct InMemoryAttachableStore {
    state: AttachableStoreState,
}

impl InMemoryAttachableStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_registry(registry: AttachableRegistry) -> Self {
        Self { state: AttachableStoreState::from_registry(registry) }
    }
}

impl AttachableStoreApi for InMemoryAttachableStore {
    fn binding_count(&self) -> usize {
        self.state.binding_count()
    }

    fn registry(&self) -> &AttachableRegistry {
        self.state.registry()
    }

    fn allocate_set_id(&self) -> AttachableSetId {
        self.state.allocate_set_id()
    }

    fn allocate_attachable_id(&self) -> AttachableId {
        self.state.allocate_attachable_id()
    }

    fn insert_set(&mut self, set: AttachableSet) {
        self.state.insert_set(set);
    }

    fn insert_attachable(&mut self, attachable: Attachable) {
        self.state.insert_attachable(attachable);
    }

    fn ensure_terminal_set(&mut self, host_affinity: Option<HostName>, checkout: Option<HostPath>) -> AttachableSetId {
        self.state.ensure_terminal_set(host_affinity, checkout)
    }

    fn ensure_terminal_set_with_change(&mut self, host_affinity: Option<HostName>, checkout: Option<HostPath>) -> (AttachableSetId, bool) {
        self.state.ensure_terminal_set_with_change(host_affinity, checkout)
    }

    fn ensure_terminal_attachable(
        &mut self,
        set_id: &AttachableSetId,
        provider_category: &str,
        provider_name: &str,
        external_ref: &str,
        terminal_purpose: TerminalPurpose,
        command: &str,
        working_directory: PathBuf,
        status: TerminalStatus,
    ) -> AttachableId {
        self.state.ensure_terminal_attachable(
            set_id,
            provider_category,
            provider_name,
            external_ref,
            terminal_purpose,
            command,
            working_directory,
            status,
        )
    }

    fn ensure_terminal_attachable_with_change(
        &mut self,
        set_id: &AttachableSetId,
        provider_category: &str,
        provider_name: &str,
        external_ref: &str,
        terminal_purpose: TerminalPurpose,
        command: &str,
        working_directory: PathBuf,
        status: TerminalStatus,
    ) -> (AttachableId, bool) {
        self.state.ensure_terminal_attachable_with_change(
            set_id,
            provider_category,
            provider_name,
            external_ref,
            terminal_purpose,
            command,
            working_directory,
            status,
        )
    }

    fn replace_binding(&mut self, binding: ProviderBinding) -> bool {
        self.state.replace_binding(binding)
    }

    fn lookup_binding(
        &self,
        provider_category: &str,
        provider_name: &str,
        object_kind: BindingObjectKind,
        external_ref: &str,
    ) -> Option<&str> {
        self.state.lookup_binding(provider_category, provider_name, object_kind, external_ref)
    }

    fn remove_binding_object(
        &mut self,
        provider_category: &str,
        provider_name: &str,
        object_kind: BindingObjectKind,
        external_ref: &str,
    ) -> bool {
        self.state.remove_binding_object(provider_category, provider_name, object_kind, external_ref)
    }

    fn remove_set(&mut self, id: &AttachableSetId) -> Option<RemovedSetInfo> {
        self.state.remove_set(id)
    }

    fn sets_for_checkout(&self, checkout: &HostPath) -> Vec<AttachableSetId> {
        self.state.sets_for_checkout(checkout)
    }

    fn update_terminal_status(&mut self, id: &AttachableId, status: TerminalStatus) -> bool {
        self.state.update_terminal_status(id, status)
    }

    fn save(&self) -> Result<(), String> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::attachable::types::{TerminalAttachable, TerminalPurpose};

    fn contract_ensure_terminal_attachable_reuses_existing_binding(store: &mut impl AttachableStoreApi) {
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
        assert_eq!(store.registry().attachables.len(), 1);
        let attachable = store.registry().attachables.get(&first).expect("attachable");
        assert_eq!(
            attachable.content,
            AttachableContent::Terminal(TerminalAttachable {
                purpose: TerminalPurpose { checkout: "feat".into(), role: "shell".into(), index: 0 },
                command: "codex".into(),
                working_directory: PathBuf::from("/repo/wt-feat"),
                status: TerminalStatus::Disconnected,
            })
        );
    }

    fn contract_ensure_terminal_set_groups_members_by_host_and_checkout(store: &mut impl AttachableStoreApi) {
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

        let set = store.registry().sets.get(&set_a).expect("set");
        assert_eq!(set.members, vec![shell, agent]);
        assert_eq!(store.registry().sets.len(), 1);
    }

    fn contract_ensure_terminal_attachable_uses_binding_as_primary_identity(store: &mut impl AttachableStoreApi) {
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
        assert_eq!(store.registry().attachables.len(), 2);
        assert_eq!(
            store.registry().attachables.get(&second).map(|a| match &a.content {
                AttachableContent::Terminal(terminal) => terminal.purpose.index,
            }),
            Some(0)
        );
    }

    fn contract_replacing_binding_is_deterministic(store: &mut impl AttachableStoreApi) {
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

    fn contract_roundtrip_preserves_stable_ids(
        mut store: impl AttachableStoreApi,
        reload: impl FnOnce(AttachableRegistry) -> Box<dyn AttachableStoreApi>,
    ) {
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

        let mut reloaded = reload(store.registry().clone());
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
    fn opaque_ids_roundtrip_as_strings() {
        let set_id = AttachableSetId::new("set-123");
        let attachable_id = AttachableId::new("att-456");

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

        let set_id = AttachableSetId::new("set-1");
        let attachable_id = AttachableId::new("att-1");

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
            content: AttachableContent::Terminal(TerminalAttachable {
                purpose: TerminalPurpose { checkout: "feat".into(), role: "shell".into(), index: 0 },
                command: "claude".into(),
                working_directory: PathBuf::from("/repo/wt-feat"),
                status: TerminalStatus::Running,
            }),
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
    fn file_backed_contract_ensure_terminal_attachable_reuses_existing_binding() {
        contract_ensure_terminal_attachable_reuses_existing_binding(&mut AttachableStore::with_base(
            tempfile::tempdir().expect("tempdir").path(),
        ));
    }

    #[test]
    fn in_memory_contract_ensure_terminal_attachable_reuses_existing_binding() {
        contract_ensure_terminal_attachable_reuses_existing_binding(&mut InMemoryAttachableStore::new());
    }

    #[test]
    fn file_backed_contract_ensure_terminal_set_groups_members_by_host_and_checkout() {
        contract_ensure_terminal_set_groups_members_by_host_and_checkout(&mut AttachableStore::with_base(
            tempfile::tempdir().expect("tempdir").path(),
        ));
    }

    #[test]
    fn in_memory_contract_ensure_terminal_set_groups_members_by_host_and_checkout() {
        contract_ensure_terminal_set_groups_members_by_host_and_checkout(&mut InMemoryAttachableStore::new());
    }

    #[test]
    fn file_backed_contract_ensure_terminal_attachable_uses_binding_as_primary_identity() {
        contract_ensure_terminal_attachable_uses_binding_as_primary_identity(&mut AttachableStore::with_base(
            tempfile::tempdir().expect("tempdir").path(),
        ));
    }

    #[test]
    fn in_memory_contract_ensure_terminal_attachable_uses_binding_as_primary_identity() {
        contract_ensure_terminal_attachable_uses_binding_as_primary_identity(&mut InMemoryAttachableStore::new());
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
    fn file_backed_contract_replacing_binding_is_deterministic() {
        contract_replacing_binding_is_deterministic(&mut AttachableStore::with_base(tempfile::tempdir().expect("tempdir").path()));
    }

    #[test]
    fn in_memory_contract_replacing_binding_is_deterministic() {
        contract_replacing_binding_is_deterministic(&mut InMemoryAttachableStore::new());
    }

    #[test]
    fn file_backed_contract_roundtrip_preserves_stable_ids() {
        let dir = tempfile::tempdir().expect("tempdir");
        contract_roundtrip_preserves_stable_ids(AttachableStore::with_base(dir.path()), |_| {
            Box::new(AttachableStore::with_base(dir.path()))
        });
    }

    #[test]
    fn in_memory_contract_roundtrip_preserves_stable_ids() {
        contract_roundtrip_preserves_stable_ids(InMemoryAttachableStore::new(), |registry| {
            Box::new(InMemoryAttachableStore::from_registry(registry))
        });
    }

    #[test]
    fn provider_local_state_is_not_identity_source() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("attachables").join("registry.json");
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

        std::fs::remove_file(path).expect("remove persisted registry");

        let store = AttachableStore::with_base(dir.path());
        assert_ne!(
            store.lookup_binding("terminal_pool", "shpool", BindingObjectKind::Attachable, "flotilla/feat/shell/0"),
            Some(attachable_id.as_str())
        );
        assert!(store.registry().attachables.is_empty());
    }

    fn contract_remove_set_deletes_set_and_members_and_bindings(store: &mut impl AttachableStoreApi) {
        let host = HostName::new("desktop");
        let checkout = HostPath::new(host.clone(), "/repo/wt-feat");
        let set_id = store.ensure_terminal_set(Some(host), Some(checkout));

        let _shell = store.ensure_terminal_attachable(
            &set_id,
            "terminal_pool",
            "shpool",
            "flotilla/feat/shell/0",
            TerminalPurpose { checkout: "feat".into(), role: "shell".into(), index: 0 },
            "bash",
            PathBuf::from("/repo/wt-feat"),
            TerminalStatus::Running,
        );
        let _agent = store.ensure_terminal_attachable(
            &set_id,
            "terminal_pool",
            "shpool",
            "flotilla/feat/agent/0",
            TerminalPurpose { checkout: "feat".into(), role: "agent".into(), index: 0 },
            "claude",
            PathBuf::from("/repo/wt-feat"),
            TerminalStatus::Running,
        );

        store.replace_binding(ProviderBinding {
            provider_category: "workspace_manager".into(),
            provider_name: "cmux".into(),
            object_kind: BindingObjectKind::AttachableSet,
            object_id: set_id.to_string(),
            external_ref: "workspace:1".into(),
        });

        let removed = store.remove_set(&set_id);
        assert!(removed.is_some());
        let removed = removed.expect("should return removed set info");
        assert_eq!(removed.member_binding_refs.len(), 2);
        assert!(removed.member_binding_refs.contains(&"flotilla/feat/shell/0".to_string()));
        assert!(removed.member_binding_refs.contains(&"flotilla/feat/agent/0".to_string()));
        assert!(store.registry().sets.is_empty());
        assert!(store.registry().attachables.is_empty());
        assert!(store.registry().bindings.is_empty());
    }

    fn contract_remove_set_returns_none_for_unknown_id(store: &mut impl AttachableStoreApi) {
        let unknown = AttachableSetId::new("nonexistent");
        assert!(store.remove_set(&unknown).is_none());
    }

    #[test]
    fn file_backed_contract_remove_set_deletes_set_and_members_and_bindings() {
        contract_remove_set_deletes_set_and_members_and_bindings(&mut AttachableStore::with_base(
            tempfile::tempdir().expect("tempdir").path(),
        ));
    }

    #[test]
    fn in_memory_contract_remove_set_deletes_set_and_members_and_bindings() {
        contract_remove_set_deletes_set_and_members_and_bindings(&mut InMemoryAttachableStore::new());
    }

    #[test]
    fn file_backed_contract_remove_set_returns_none_for_unknown_id() {
        contract_remove_set_returns_none_for_unknown_id(&mut AttachableStore::with_base(tempfile::tempdir().expect("tempdir").path()));
    }

    #[test]
    fn in_memory_contract_remove_set_returns_none_for_unknown_id() {
        contract_remove_set_returns_none_for_unknown_id(&mut InMemoryAttachableStore::new());
    }

    fn contract_sets_for_checkout_returns_matching_sets(store: &mut impl AttachableStoreApi) {
        let host = HostName::new("desktop");
        let checkout_a = HostPath::new(host.clone(), "/repo/wt-feat");
        let checkout_b = HostPath::new(host.clone(), "/repo/wt-main");
        let set_a = store.ensure_terminal_set(Some(host.clone()), Some(checkout_a.clone()));
        let _set_b = store.ensure_terminal_set(Some(host.clone()), Some(checkout_b.clone()));
        let found = store.sets_for_checkout(&checkout_a);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0], set_a);
    }

    fn contract_sets_for_checkout_returns_empty_for_unknown(store: &mut impl AttachableStoreApi) {
        let unknown = HostPath::new(HostName::new("desktop"), "/repo/nonexistent");
        assert!(store.sets_for_checkout(&unknown).is_empty());
    }

    #[test]
    fn file_backed_contract_sets_for_checkout_returns_matching_sets() {
        contract_sets_for_checkout_returns_matching_sets(&mut AttachableStore::with_base(tempfile::tempdir().expect("tempdir").path()));
    }

    #[test]
    fn in_memory_contract_sets_for_checkout_returns_matching_sets() {
        contract_sets_for_checkout_returns_matching_sets(&mut InMemoryAttachableStore::new());
    }

    #[test]
    fn file_backed_contract_sets_for_checkout_returns_empty_for_unknown() {
        contract_sets_for_checkout_returns_empty_for_unknown(&mut AttachableStore::with_base(tempfile::tempdir().expect("tempdir").path()));
    }

    #[test]
    fn in_memory_contract_sets_for_checkout_returns_empty_for_unknown() {
        contract_sets_for_checkout_returns_empty_for_unknown(&mut InMemoryAttachableStore::new());
    }
}
