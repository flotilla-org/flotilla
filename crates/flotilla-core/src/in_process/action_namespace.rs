use std::sync::RwLock;

pub(super) fn provisioning_namespace(namespace: &RwLock<String>) -> String {
    namespace.read().expect("provisioning namespace lock poisoned").clone()
}
