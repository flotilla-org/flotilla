use flotilla_resources::{ApiPaths, InMemoryBackend, InputMeta, Resource, ResourceBackend, StatusPatch};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy)]
struct CounterResource;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct CounterSpec {
    name: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct CounterStatus {
    value: u32,
    note: Option<String>,
}

enum CounterPatch {
    Increment,
    SetNote(&'static str),
}

impl Resource for CounterResource {
    type Spec = CounterSpec;
    type Status = CounterStatus;
    type StatusPatch = CounterPatch;

    const API_PATHS: ApiPaths = ApiPaths { group: "flotilla.work", version: "v1", plural: "counters", kind: "Counter" };
}

impl StatusPatch<CounterStatus> for CounterPatch {
    fn apply(&self, status: &mut CounterStatus) {
        match self {
            Self::Increment => status.value += 1,
            Self::SetNote(note) => status.note = Some((*note).to_string()),
        }
    }
}

fn counter_meta(name: &str) -> InputMeta {
    InputMeta { name: name.to_string(), labels: Default::default(), annotations: Default::default() }
}

fn counter_spec(name: &str) -> CounterSpec {
    CounterSpec { name: name.to_string() }
}

#[tokio::test]
async fn apply_status_patch_updates_existing_status() {
    let resolver = ResourceBackend::InMemory(InMemoryBackend::default()).using::<CounterResource>("flotilla");
    let created = resolver.create(&counter_meta("alpha"), &counter_spec("alpha")).await.expect("create should succeed");
    let current = resolver
        .update_status("alpha", &created.metadata.resource_version, &CounterStatus { value: 1, note: None })
        .await
        .expect("seed status should succeed");

    let updated =
        flotilla_resources::apply_status_patch(&resolver, "alpha", &CounterPatch::Increment).await.expect("status patch should succeed");

    assert_eq!(updated.status.expect("status"), CounterStatus { value: 2, note: None });
    assert_eq!(updated.metadata.resource_version, "3");
    assert_eq!(current.metadata.resource_version, "2");
}

#[tokio::test]
async fn apply_status_patch_initializes_missing_status_from_default() {
    let resolver = ResourceBackend::InMemory(InMemoryBackend::default()).using::<CounterResource>("flotilla");
    let created = resolver.create(&counter_meta("beta"), &counter_spec("beta")).await.expect("create should succeed");

    let updated = flotilla_resources::apply_status_patch(&resolver, "beta", &CounterPatch::SetNote("ready"))
        .await
        .expect("status patch should succeed");

    assert_eq!(created.status, None);
    assert_eq!(updated.status.expect("status"), CounterStatus { value: 0, note: Some("ready".to_string()) });
    assert_eq!(updated.metadata.resource_version, "2");
}
