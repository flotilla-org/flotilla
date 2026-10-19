use std::sync::RwLock;

pub fn provisioning_namespace(namespace: &RwLock<String>) -> String {
    namespace.read().expect("provisioning namespace lock poisoned").clone()
}
