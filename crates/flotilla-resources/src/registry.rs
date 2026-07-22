use futures::{stream::BoxStream, StreamExt};
use serde_json::{json, Value};

use crate::{
    api_version, Checkout, Clone as CloneResource, Convoy, Environment, Host, K8sListMeta, K8sResourceList, PlacementPolicy, Presentation,
    Project, Repository, Resource, ResourceBackend, ResourceError, ResourceList, ResourceObject, TerminalSession, Vessel, WatchEvent,
    WatchStart, WorkflowTemplate,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegisteredResourceKind {
    pub kind: &'static str,
    pub plural: &'static str,
    aliases: &'static [&'static str],
    resource: RegisteredResource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RegisteredResource {
    Checkout,
    Clone,
    Convoy,
    Environment,
    Host,
    PlacementPolicy,
    Presentation,
    Project,
    Repository,
    TerminalSession,
    Vessel,
    WorkflowTemplate,
}

#[derive(Debug, Clone)]
pub struct DynamicResourceList {
    pub kind: String,
    pub plural: String,
    pub namespace: String,
    pub value: Value,
}

#[derive(Debug, Clone)]
pub struct DynamicResourceObject {
    pub kind: String,
    pub plural: String,
    pub namespace: String,
    pub value: Value,
}

pub struct DynamicResourceWatch {
    pub kind: String,
    pub plural: String,
    pub namespace: String,
    pub initial: Vec<Value>,
    pub stream: BoxStream<'static, Result<Value, ResourceError>>,
}

pub const REGISTERED_RESOURCE_KINDS: &[RegisteredResourceKind] = &[
    kind::<Checkout>(RegisteredResource::Checkout, &[]),
    kind::<CloneResource>(RegisteredResource::Clone, &[]),
    kind::<Convoy>(RegisteredResource::Convoy, &[]),
    kind::<Environment>(RegisteredResource::Environment, &[]),
    kind::<Host>(RegisteredResource::Host, &[]),
    kind::<PlacementPolicy>(RegisteredResource::PlacementPolicy, &[
        "placementpolicy",
        "placement_policy",
        "placement_policies",
        "placement-policy",
        "placement-policies",
    ]),
    kind::<Presentation>(RegisteredResource::Presentation, &[]),
    kind::<Project>(RegisteredResource::Project, &[]),
    kind::<Repository>(RegisteredResource::Repository, &[]),
    kind::<TerminalSession>(RegisteredResource::TerminalSession, &[
        "terminalsession",
        "terminal_session",
        "terminal_sessions",
        "terminal-session",
        "terminal-sessions",
    ]),
    kind::<Vessel>(RegisteredResource::Vessel, &[]),
    kind::<WorkflowTemplate>(RegisteredResource::WorkflowTemplate, &[
        "workflowtemplate",
        "workflow_template",
        "workflow_templates",
        "workflow-template",
        "workflow-templates",
    ]),
];

const fn kind<T: Resource>(resource: RegisteredResource, aliases: &'static [&'static str]) -> RegisteredResourceKind {
    RegisteredResourceKind { kind: T::API_PATHS.kind, plural: T::API_PATHS.plural, aliases, resource }
}

macro_rules! dispatch_resource_kind {
    ($resource:expr, $body:ident($($arg:expr),*).await) => {
        match $resource {
            RegisteredResource::Checkout => $body::<Checkout>($($arg),*).await,
            RegisteredResource::Clone => $body::<CloneResource>($($arg),*).await,
            RegisteredResource::Convoy => $body::<Convoy>($($arg),*).await,
            RegisteredResource::Environment => $body::<Environment>($($arg),*).await,
            RegisteredResource::Host => $body::<Host>($($arg),*).await,
            RegisteredResource::PlacementPolicy => $body::<PlacementPolicy>($($arg),*).await,
            RegisteredResource::Presentation => $body::<Presentation>($($arg),*).await,
            RegisteredResource::Project => $body::<Project>($($arg),*).await,
            RegisteredResource::Repository => $body::<Repository>($($arg),*).await,
            RegisteredResource::TerminalSession => $body::<TerminalSession>($($arg),*).await,
            RegisteredResource::Vessel => $body::<Vessel>($($arg),*).await,
            RegisteredResource::WorkflowTemplate => $body::<WorkflowTemplate>($($arg),*).await,
        }
    };
    ($resource:expr, $body:ident()) => {
        match $resource {
            RegisteredResource::Checkout => $body::<Checkout>(),
            RegisteredResource::Clone => $body::<CloneResource>(),
            RegisteredResource::Convoy => $body::<Convoy>(),
            RegisteredResource::Environment => $body::<Environment>(),
            RegisteredResource::Host => $body::<Host>(),
            RegisteredResource::PlacementPolicy => $body::<PlacementPolicy>(),
            RegisteredResource::Presentation => $body::<Presentation>(),
            RegisteredResource::Project => $body::<Project>(),
            RegisteredResource::Repository => $body::<Repository>(),
            RegisteredResource::TerminalSession => $body::<TerminalSession>(),
            RegisteredResource::Vessel => $body::<Vessel>(),
            RegisteredResource::WorkflowTemplate => $body::<WorkflowTemplate>(),
        }
    };
}

pub async fn list_resource_kind(
    backend: &ResourceBackend,
    namespace: &str,
    requested_kind: &str,
) -> Result<DynamicResourceList, ResourceError> {
    dispatch_resource_kind!(lookup_resource_kind(requested_kind)?.resource, list_typed(backend, namespace).await)
}

pub async fn get_resource_kind(
    backend: &ResourceBackend,
    namespace: &str,
    requested_kind: &str,
    name: &str,
) -> Result<DynamicResourceObject, ResourceError> {
    dispatch_resource_kind!(lookup_resource_kind(requested_kind)?.resource, get_typed(backend, namespace, name).await)
}

pub async fn watch_resource_kind(
    backend: &ResourceBackend,
    namespace: &str,
    requested_kind: &str,
) -> Result<DynamicResourceWatch, ResourceError> {
    dispatch_resource_kind!(lookup_resource_kind(requested_kind)?.resource, watch_typed(backend, namespace).await)
}

fn lookup_resource_kind(kind: &str) -> Result<&'static RegisteredResourceKind, ResourceError> {
    let normalized = kind.trim();
    REGISTERED_RESOURCE_KINDS
        .iter()
        .find(|entry| {
            normalized.eq_ignore_ascii_case(entry.kind)
                || normalized.eq_ignore_ascii_case(entry.plural)
                || entry.aliases.iter().any(|alias| normalized.eq_ignore_ascii_case(alias))
        })
        .ok_or_else(|| unknown_kind(kind))
}

fn unknown_kind(kind: &str) -> ResourceError {
    let supported = REGISTERED_RESOURCE_KINDS.iter().map(|entry| entry.plural).collect::<Vec<_>>().join(", ");
    ResourceError::invalid(format!("unknown resource kind '{kind}' (supported: {supported})"))
}

async fn list_typed<T: Resource>(backend: &ResourceBackend, namespace: &str) -> Result<DynamicResourceList, ResourceError> {
    let listed = backend.using::<T>(namespace).list().await?;
    Ok(DynamicResourceList {
        kind: T::API_PATHS.kind.to_string(),
        plural: T::API_PATHS.plural.to_string(),
        namespace: namespace.to_string(),
        value: list_value::<T>(&listed)?,
    })
}

async fn get_typed<T: Resource>(backend: &ResourceBackend, namespace: &str, name: &str) -> Result<DynamicResourceObject, ResourceError> {
    let object = backend.using::<T>(namespace).get(name).await?;
    Ok(DynamicResourceObject {
        kind: T::API_PATHS.kind.to_string(),
        plural: T::API_PATHS.plural.to_string(),
        namespace: namespace.to_string(),
        value: object_value(&object)?,
    })
}

async fn watch_typed<T: Resource>(backend: &ResourceBackend, namespace: &str) -> Result<DynamicResourceWatch, ResourceError> {
    let resolver = backend.using::<T>(namespace);
    let listed = resolver.list().await?;
    let start = WatchStart::resuming_from(&listed);
    let initial = listed.items.iter().map(|object| watch_event_value("ADDED", object)).collect::<Result<Vec<_>, _>>()?;
    let stream = resolver.watch(start).await?.map(|event| event.and_then(|event| typed_watch_event_value(&event))).boxed();
    Ok(DynamicResourceWatch {
        kind: T::API_PATHS.kind.to_string(),
        plural: T::API_PATHS.plural.to_string(),
        namespace: namespace.to_string(),
        initial,
        stream,
    })
}

fn list_value<T: Resource>(listed: &ResourceList<T>) -> Result<Value, ResourceError> {
    let list = K8sResourceList {
        metadata: K8sListMeta { resource_version: listed.resource_version.clone() },
        items: listed.items.iter().map(ResourceObject::to_k8s_object).collect(),
    };
    serde_json::to_value(list).map_err(|error| ResourceError::decode(format!("encode resource list: {error}")))
}

fn object_value<T: Resource>(object: &ResourceObject<T>) -> Result<Value, ResourceError> {
    serde_json::to_value(object.to_k8s_object()).map_err(|error| ResourceError::decode(format!("encode resource object: {error}")))
}

fn typed_watch_event_value<T: Resource>(event: &WatchEvent<T>) -> Result<Value, ResourceError> {
    match event {
        WatchEvent::Added(object) => watch_event_value("ADDED", object),
        WatchEvent::Modified(object) => watch_event_value("MODIFIED", object),
        WatchEvent::Deleted(object) => watch_event_value("DELETED", object),
    }
}

fn watch_event_value<T: Resource>(event_type: &str, object: &ResourceObject<T>) -> Result<Value, ResourceError> {
    Ok(json!({
        "type": event_type,
        "object": object.to_k8s_object(),
    }))
}

pub fn resource_list_api_version(kind: &str) -> Result<String, ResourceError> {
    Ok(dispatch_resource_kind!(lookup_resource_kind(kind)?.resource, api_version_typed()))
}

fn api_version_typed<T: Resource>() -> String {
    api_version(T::API_PATHS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Convoy, ConvoySpec, InMemoryBackend, InputMeta};

    #[tokio::test]
    async fn list_resource_kind_returns_k8s_wire_list() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        backend
            .using::<Convoy>("flotilla")
            .create(&InputMeta::builder().name("demo".to_string()).build(), &ConvoySpec::builder().workflow_ref("wf".to_string()).build())
            .await
            .expect("create convoy");

        let listed = list_resource_kind(&backend, "flotilla", "convoys").await.expect("list convoys");

        assert_eq!(listed.kind, "Convoy");
        assert_eq!(listed.value["items"][0]["apiVersion"], "flotilla.work/v1");
        assert_eq!(listed.value["items"][0]["kind"], "Convoy");
        assert_eq!(listed.value["items"][0]["metadata"]["name"], "demo");
    }

    #[tokio::test]
    async fn unknown_kind_reports_registered_plural_names() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let error = list_resource_kind(&backend, "flotilla", "nope").await.expect_err("unknown kind should fail");
        let message = error.to_string();
        assert!(message.contains("unknown resource kind 'nope'"));
        assert!(message.contains("convoys"));
    }
}
