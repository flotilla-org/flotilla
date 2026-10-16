//! The kind registry: which kinds a store holds, keyed by their stable codes.

use std::collections::BTreeMap;

use crate::{
    descriptor::{DescriptorError, TableSet},
    traits::Kind,
    value::KindCode,
};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RegistryError {
    #[error("kind code {code} is taken by {existing}; {kind} cannot reuse it")]
    DuplicateCode { code: KindCode, existing: String, kind: String },
    #[error("kind name {kind} is registered twice")]
    DuplicateName { kind: String },
    #[error("schema object {name} of kind {kind} is already declared by kind {existing}")]
    DuplicateObject { name: String, kind: String, existing: String },
    #[error(transparent)]
    Descriptor(#[from] DescriptorError),
}

/// Registered kinds and their validated descriptors. Codes, kind names and schema object names
/// (tables and indexes) are unique across the registry.
#[derive(Debug, Clone, Default)]
pub struct KindRegistry {
    kinds: BTreeMap<KindCode, TableSet>,
}

impl KindRegistry {
    pub fn new() -> KindRegistry {
        KindRegistry::default()
    }

    pub fn register<K: Kind>(&mut self) -> Result<(), RegistryError> {
        self.register_table_set(K::table_set())
    }

    /// Registers a descriptor directly; `register` is the typed path.
    pub fn register_table_set(&mut self, set: TableSet) -> Result<(), RegistryError> {
        set.validate()?;
        if let Some(existing) = self.kinds.get(&set.code) {
            return Err(RegistryError::DuplicateCode { code: set.code, existing: existing.kind.clone(), kind: set.kind });
        }
        if self.kinds.values().any(|existing| existing.kind == set.kind) {
            return Err(RegistryError::DuplicateName { kind: set.kind });
        }
        for existing in self.kinds.values() {
            if let Some(name) = set.object_names().find(|name| existing.object_names().any(|other| other == *name)) {
                return Err(RegistryError::DuplicateObject { name, kind: set.kind.clone(), existing: existing.kind.clone() });
            }
        }
        self.kinds.insert(set.code, set);
        Ok(())
    }

    pub fn get(&self, code: KindCode) -> Option<&TableSet> {
        self.kinds.get(&code)
    }

    /// Registered kinds in code order.
    pub fn iter(&self) -> impl Iterator<Item = &TableSet> {
        self.kinds.values()
    }
}

#[cfg(test)]
mod tests {
    use super::{KindRegistry, RegistryError};
    use crate::{
        descriptor::{ColumnType, TableRole, TableSet},
        layout::Layout,
        value::KindCode,
    };

    fn kind(name: &str, code: u32) -> TableSet {
        let mut layout = Layout::root(name, KindCode(code));
        layout.column("body", ColumnType::Text, true);
        layout.finish()
    }

    // Behaviour (#3320): the registry rejects a duplicate kind code and a duplicate kind name, and
    // lookup by code returns the registered descriptor.
    #[test]
    fn registry_rejects_duplicates_and_looks_up_by_code() {
        let mut registry = KindRegistry::new();
        registry.register_table_set(kind("Message", 1)).expect("first kind");
        assert_eq!(registry.get(KindCode(1)).map(|set| set.kind.as_str()), Some("Message"));
        assert_eq!(registry.get(KindCode(2)), None);
        assert!(matches!(
            registry.register_table_set(kind("Convoy", 1)),
            Err(RegistryError::DuplicateCode { code: KindCode(1), existing, kind }) if existing == "Message" && kind == "Convoy"
        ));
        assert!(matches!(registry.register_table_set(kind("Message", 2)), Err(RegistryError::DuplicateName { kind }) if kind == "Message"));
        assert!(matches!(registry.register_table_set(kind("Objects", 3)), Err(RegistryError::Descriptor(_))));
        registry.register_table_set(kind("Convoy", 2)).expect("distinct kind");
        assert_eq!(registry.iter().count(), 2);
    }

    #[test]
    fn registry_rejects_table_names_shared_across_kinds() {
        let mut registry = KindRegistry::new();
        let mut layout = Layout::root("Message", KindCode(1));
        layout.table("log", TableRole::Child, |layout| layout.key(|layout| layout.column("pos", ColumnType::Integer, true)));
        registry.register_table_set(layout.finish()).expect("first kind");
        assert!(matches!(
            registry.register_table_set(kind("MessageLog", 2)),
            Err(RegistryError::DuplicateObject { name, .. }) if name == "message_log"
        ));
    }
}
