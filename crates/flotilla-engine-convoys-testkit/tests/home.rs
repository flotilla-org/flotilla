//! The composed engine home serves every registered kind, as a daemon does (#3108).

use flotilla_engine_convoys_testkit::EngineHome;
use flotilla_store::{list_resource_kind, REGISTERED_RESOURCE_KINDS};

// Behaviour: no registered kind is left to the store under the engine. A kind the composition
// leaves out is refused as not composed, which a test reading it would otherwise meet.
#[tokio::test]
async fn every_registered_kind_is_served_by_the_engine() {
    let home = EngineHome::open("home");
    let mut uncomposed = Vec::new();
    for kind in REGISTERED_RESOURCE_KINDS {
        if let Err(error) = list_resource_kind(home.backend(), "flotilla", kind.plural).await {
            if error.to_string().contains("not composed") {
                uncomposed.push(kind.kind);
            }
        }
    }
    assert!(uncomposed.is_empty(), "kinds not composed: {uncomposed:?}");
}
