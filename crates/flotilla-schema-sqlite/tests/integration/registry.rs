use flotilla_schema::{Kind, KindRegistry, RegistryError, Stored};

use crate::fixtures::MessageRow as Message;

#[derive(Debug, Clone, PartialEq, Stored)]
#[store(kind = "Convoy", code = "ms")]
struct SameCode {
    name: String,
}

#[derive(Debug, Clone, PartialEq, Stored)]
#[store(kind = "Message", code = "k2")]
struct SameName {
    body: String,
}

// Behaviour (#3320 representation): the kind-code registry rejects a duplicate code and a
// duplicate kind name with typed errors, and looks kinds up by code.
#[test]
fn registry_rejects_duplicate_codes_and_names() {
    let mut registry = KindRegistry::new();
    registry.register::<Message>().expect("message");
    assert_eq!(registry.get(Message::CODE).expect("registered"), &Message::table_set());
    assert!(matches!(
        registry.register::<SameCode>(),
        Err(RegistryError::DuplicateCode { code, existing, kind }) if code == Message::CODE && existing == "Message" && kind == "Convoy"
    ));
    assert!(matches!(registry.register::<SameName>(), Err(RegistryError::DuplicateName { kind }) if kind == "Message"));
    assert_eq!(registry.iter().count(), 1);
}
