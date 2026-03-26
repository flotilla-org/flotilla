use super::*;
use crate::attachable::types::{TerminalAttachable, TerminalPurpose};

fn temp_base(dir: &tempfile::TempDir) -> DaemonHostPath {
    DaemonHostPath::new(dir.path())
}

fn contract_ensure_terminal_attachable_reuses_existing_binding(store: &mut impl AttachableStoreApi) {
    let set_id = store.ensure_terminal_set(Some(HostName::new("desktop")), Some(HostPath::new(HostName::new("desktop"), "/repo/wt-feat")));

    let first = store.ensure_terminal_attachable(
        &set_id,
        "terminal_pool",
        "shpool",
        "flotilla/feat/shell/0",
        TerminalPurpose { checkout: "feat".into(), role: "shell".into(), index: 0 },
        "claude",
        ExecutionEnvironmentPath::new("/repo/wt-feat"),
        TerminalStatus::Running,
    );
    let second = store.ensure_terminal_attachable(
        &set_id,
        "terminal_pool",
        "shpool",
        "flotilla/feat/shell/0",
        TerminalPurpose { checkout: "feat".into(), role: "shell".into(), index: 0 },
        "codex",
        ExecutionEnvironmentPath::new("/repo/wt-feat"),
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
            working_directory: ExecutionEnvironmentPath::new("/repo/wt-feat"),
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
        ExecutionEnvironmentPath::new("/repo/wt-feat"),
        TerminalStatus::Running,
    );
    let agent = store.ensure_terminal_attachable(
        &set_a,
        "terminal_pool",
        "shpool",
        "flotilla/feat/agent/0",
        TerminalPurpose { checkout: "feat".into(), role: "agent".into(), index: 0 },
        "codex",
        ExecutionEnvironmentPath::new("/repo/wt-feat"),
        TerminalStatus::Running,
    );

    let set = store.registry().sets.get(&set_a).expect("set");
    assert_eq!(set.members, vec![shell, agent]);
    assert_eq!(store.registry().sets.len(), 1);
}

fn contract_ensure_terminal_attachable_uses_binding_as_primary_identity(store: &mut impl AttachableStoreApi) {
    let set_id = store.ensure_terminal_set(Some(HostName::new("desktop")), Some(HostPath::new(HostName::new("desktop"), "/repo/wt-feat")));

    let first = store.ensure_terminal_attachable(
        &set_id,
        "terminal_pool",
        "shpool",
        "flotilla/feat/shell/0",
        TerminalPurpose { checkout: "feat".into(), role: "shell".into(), index: 0 },
        "claude",
        ExecutionEnvironmentPath::new("/repo/wt-feat"),
        TerminalStatus::Running,
    );
    let second = store.ensure_terminal_attachable(
        &set_id,
        "terminal_pool",
        "shpool",
        "flotilla/feat/shell/1",
        TerminalPurpose { checkout: "feat".into(), role: "shell".into(), index: 0 },
        "claude",
        ExecutionEnvironmentPath::new("/repo/wt-feat"),
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
        ExecutionEnvironmentPath::new("/repo/wt-feat"),
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
        ExecutionEnvironmentPath::new("/repo/wt-feat"),
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
    let store = AttachableStore::with_base(&temp_base(&dir));
    store.save().expect("save empty registry");

    let reloaded = AttachableStore::with_base(&temp_base(&dir));
    assert_eq!(reloaded.registry(), store.registry());
    assert!(reloaded.registry().sets.is_empty());
    assert!(reloaded.registry().attachables.is_empty());
    assert!(reloaded.registry().bindings.is_empty());
}

#[test]
fn registry_roundtrip_rebuilds_binding_index() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut store = AttachableStore::with_base(&temp_base(&dir));

    let set_id = AttachableSetId::new("set-1");
    let attachable_id = AttachableId::new("att-1");

    store.insert_set(AttachableSet {
        id: set_id.clone(),
        host_affinity: Some(HostName::new("desktop")),
        checkout: Some(HostPath::new(HostName::new("desktop"), "/repo/wt-feat")),
        template_identity: Some("default".into()),
        environment_id: None,
        members: vec![attachable_id.clone()],
    });
    store.insert_attachable(Attachable {
        id: attachable_id.clone(),
        set_id: set_id.clone(),
        content: AttachableContent::Terminal(TerminalAttachable {
            purpose: TerminalPurpose { checkout: "feat".into(), role: "shell".into(), index: 0 },
            command: "claude".into(),
            working_directory: ExecutionEnvironmentPath::new("/repo/wt-feat"),
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

    let reloaded = AttachableStore::with_base(&temp_base(&dir));
    assert_eq!(reloaded.registry(), store.registry());
    assert_eq!(reloaded.lookup_binding("terminal_pool", "shpool", BindingObjectKind::Attachable, "flotilla/feat/shell/0"), Some("att-1"));
    assert_eq!(reloaded.lookup_binding("workspace_manager", "tmux", BindingObjectKind::AttachableSet, "workspace:7"), Some("set-1"));
}

#[test]
fn file_backed_contract_ensure_terminal_attachable_reuses_existing_binding() {
    contract_ensure_terminal_attachable_reuses_existing_binding(&mut AttachableStore::with_base(&temp_base(
        &tempfile::tempdir().expect("tempdir"),
    )));
}

#[test]
fn in_memory_contract_ensure_terminal_attachable_reuses_existing_binding() {
    contract_ensure_terminal_attachable_reuses_existing_binding(&mut InMemoryAttachableStore::new());
}

#[test]
fn file_backed_contract_ensure_terminal_set_groups_members_by_host_and_checkout() {
    contract_ensure_terminal_set_groups_members_by_host_and_checkout(&mut AttachableStore::with_base(&temp_base(
        &tempfile::tempdir().expect("tempdir"),
    )));
}

#[test]
fn in_memory_contract_ensure_terminal_set_groups_members_by_host_and_checkout() {
    contract_ensure_terminal_set_groups_members_by_host_and_checkout(&mut InMemoryAttachableStore::new());
}

#[test]
fn file_backed_contract_ensure_terminal_attachable_uses_binding_as_primary_identity() {
    contract_ensure_terminal_attachable_uses_binding_as_primary_identity(&mut AttachableStore::with_base(&temp_base(
        &tempfile::tempdir().expect("tempdir"),
    )));
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

    let store = AttachableStore::with_base(&temp_base(&dir));
    assert!(store.registry().sets.is_empty());
    assert!(store.registry().attachables.is_empty());
    assert!(store.registry().bindings.is_empty());
}

#[test]
fn file_backed_contract_replacing_binding_is_deterministic() {
    contract_replacing_binding_is_deterministic(&mut AttachableStore::with_base(&temp_base(&tempfile::tempdir().expect("tempdir"))));
}

#[test]
fn in_memory_contract_replacing_binding_is_deterministic() {
    contract_replacing_binding_is_deterministic(&mut InMemoryAttachableStore::new());
}

#[test]
fn file_backed_contract_roundtrip_preserves_stable_ids() {
    let dir = tempfile::tempdir().expect("tempdir");
    contract_roundtrip_preserves_stable_ids(AttachableStore::with_base(&temp_base(&dir)), |_| {
        Box::new(AttachableStore::with_base(&temp_base(&dir)))
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
    let mut store = AttachableStore::with_base(&temp_base(&dir));
    let set_id = store.ensure_terminal_set(Some(HostName::new("desktop")), Some(HostPath::new(HostName::new("desktop"), "/repo/wt-feat")));
    let attachable_id = store.ensure_terminal_attachable(
        &set_id,
        "terminal_pool",
        "shpool",
        "flotilla/feat/shell/0",
        TerminalPurpose { checkout: "feat".into(), role: "shell".into(), index: 0 },
        "claude",
        ExecutionEnvironmentPath::new("/repo/wt-feat"),
        TerminalStatus::Running,
    );
    store.save().expect("save registry");

    std::fs::remove_file(path).expect("remove persisted registry");

    let store = AttachableStore::with_base(&temp_base(&dir));
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
        ExecutionEnvironmentPath::new("/repo/wt-feat"),
        TerminalStatus::Running,
    );
    let _agent = store.ensure_terminal_attachable(
        &set_id,
        "terminal_pool",
        "shpool",
        "flotilla/feat/agent/0",
        TerminalPurpose { checkout: "feat".into(), role: "agent".into(), index: 0 },
        "claude",
        ExecutionEnvironmentPath::new("/repo/wt-feat"),
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
    contract_remove_set_deletes_set_and_members_and_bindings(&mut AttachableStore::with_base(&temp_base(
        &tempfile::tempdir().expect("tempdir"),
    )));
}

#[test]
fn in_memory_contract_remove_set_deletes_set_and_members_and_bindings() {
    contract_remove_set_deletes_set_and_members_and_bindings(&mut InMemoryAttachableStore::new());
}

#[test]
fn file_backed_contract_remove_set_returns_none_for_unknown_id() {
    contract_remove_set_returns_none_for_unknown_id(&mut AttachableStore::with_base(&temp_base(&tempfile::tempdir().expect("tempdir"))));
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

fn contract_lookup_workspace_ref_for_set(store: &mut impl AttachableStoreApi) {
    let set_id = AttachableSetId::new("set-1");
    store.insert_set(AttachableSet {
        id: set_id.clone(),
        host_affinity: None,
        checkout: None,
        template_identity: None,
        environment_id: None,
        members: Vec::new(),
    });

    // No bindings yet => None
    assert_eq!(store.lookup_workspace_ref_for_set("workspace_manager", "cmux", &set_id), None);

    // Add a workspace binding for this set
    store.replace_binding(ProviderBinding {
        provider_category: "workspace_manager".into(),
        provider_name: "cmux".into(),
        object_kind: BindingObjectKind::AttachableSet,
        object_id: set_id.to_string(),
        external_ref: "workspace:1".into(),
    });
    assert_eq!(store.lookup_workspace_ref_for_set("workspace_manager", "cmux", &set_id), Some("workspace:1".to_string()));

    // Different provider_name => None
    assert_eq!(store.lookup_workspace_ref_for_set("workspace_manager", "tmux", &set_id), None);

    // Attachable binding for same object_id doesn't match (wrong object_kind)
    let att_id = AttachableId::new("att-1");
    store.insert_attachable(Attachable {
        id: att_id.clone(),
        set_id: set_id.clone(),
        content: AttachableContent::Terminal(TerminalAttachable {
            purpose: TerminalPurpose { checkout: "feat".into(), role: "shell".into(), index: 0 },
            command: "bash".into(),
            working_directory: ExecutionEnvironmentPath::new("/repo"),
            status: TerminalStatus::Running,
        }),
    });
    store.replace_binding(ProviderBinding {
        provider_category: "workspace_manager".into(),
        provider_name: "cmux".into(),
        object_kind: BindingObjectKind::Attachable,
        object_id: att_id.to_string(),
        external_ref: "session:99".into(),
    });
    // Still returns the set binding, not the attachable binding
    assert_eq!(store.lookup_workspace_ref_for_set("workspace_manager", "cmux", &set_id), Some("workspace:1".to_string()));
}

fn contract_lookup_workspace_ref_for_set_ignores_other_set_ids(store: &mut impl AttachableStoreApi) {
    let set_id = AttachableSetId::new("set-1");
    store.insert_set(AttachableSet {
        id: set_id.clone(),
        host_affinity: None,
        checkout: None,
        template_identity: None,
        environment_id: None,
        members: Vec::new(),
    });

    // Binding for a different object_id (not set-1)
    store.replace_binding(ProviderBinding {
        provider_category: "workspace_manager".into(),
        provider_name: "cmux".into(),
        object_kind: BindingObjectKind::AttachableSet,
        object_id: "OLD-UUID".into(),
        external_ref: "workspace:old".into(),
    });
    // Binding for the actual set
    store.replace_binding(ProviderBinding {
        provider_category: "workspace_manager".into(),
        provider_name: "cmux".into(),
        object_kind: BindingObjectKind::AttachableSet,
        object_id: set_id.to_string(),
        external_ref: "workspace:new".into(),
    });

    // Lookup returns the binding matching set-1, ignoring the OLD-UUID binding
    assert_eq!(store.lookup_workspace_ref_for_set("workspace_manager", "cmux", &set_id), Some("workspace:new".to_string()));
}

#[test]
fn file_backed_contract_sets_for_checkout_returns_matching_sets() {
    contract_sets_for_checkout_returns_matching_sets(&mut AttachableStore::with_base(&temp_base(&tempfile::tempdir().expect("tempdir"))));
}

#[test]
fn in_memory_contract_sets_for_checkout_returns_matching_sets() {
    contract_sets_for_checkout_returns_matching_sets(&mut InMemoryAttachableStore::new());
}

#[test]
fn file_backed_contract_sets_for_checkout_returns_empty_for_unknown() {
    contract_sets_for_checkout_returns_empty_for_unknown(&mut AttachableStore::with_base(&temp_base(
        &tempfile::tempdir().expect("tempdir"),
    )));
}

#[test]
fn in_memory_contract_sets_for_checkout_returns_empty_for_unknown() {
    contract_sets_for_checkout_returns_empty_for_unknown(&mut InMemoryAttachableStore::new());
}

#[test]
fn file_backed_contract_lookup_workspace_ref_for_set() {
    contract_lookup_workspace_ref_for_set(&mut AttachableStore::with_base(&temp_base(&tempfile::tempdir().expect("tempdir"))));
}

#[test]
fn in_memory_contract_lookup_workspace_ref_for_set() {
    contract_lookup_workspace_ref_for_set(&mut InMemoryAttachableStore::new());
}

#[test]
fn file_backed_contract_lookup_workspace_ref_for_set_ignores_other_set_ids() {
    contract_lookup_workspace_ref_for_set_ignores_other_set_ids(&mut AttachableStore::with_base(&temp_base(
        &tempfile::tempdir().expect("tempdir"),
    )));
}

#[test]
fn in_memory_contract_lookup_workspace_ref_for_set_ignores_other_set_ids() {
    contract_lookup_workspace_ref_for_set_ignores_other_set_ids(&mut InMemoryAttachableStore::new());
}
