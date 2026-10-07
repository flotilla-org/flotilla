//! Session capability reporting. Credential material stays behind the runtime
//! port; cards contain only scope and effective permissions.
use std::collections::{BTreeMap, BTreeSet};

use async_trait::async_trait;
use flotilla_resources::{Convoy, Environment, ImageBuild, ResourceBackend, TerminalSessionSpec};

#[derive(Debug, Clone, PartialEq, Eq, bon::Builder)]
pub struct CredentialCapability {
    pub name: String,
    pub repositories: Vec<String>,
    pub permissions: Option<BTreeMap<String, String>>,
}

/// Reports delivered credentials, rather than desired grants or token contents.
/// A refresh that fails must leave this observation at the previous delivery.
#[async_trait]
pub trait SessionCapabilitySource: Send + Sync {
    async fn credentials(&self, environment: &str, references: &BTreeSet<String>) -> Result<Vec<CredentialCapability>, String>;
    async fn endpoints(&self, _environment: &str) -> Result<BTreeMap<String, String>, String> {
        Ok(BTreeMap::new())
    }
}

pub fn credential_references(
    session: &flotilla_resources::ResourceObject<flotilla_resources::TerminalSession>,
) -> Result<BTreeSet<String>, String> {
    session
        .metadata
        .annotations
        .get(flotilla_resources::CREDENTIAL_REFS_ANNOTATION)
        .map(|encoded| serde_json::from_str(encoded).map_err(|error| format!("invalid session credential references: {error}")))
        .transpose()
        .map(|references| references.unwrap_or_default())
}

/// Project explicitly named endpoint variables without retaining authentication
/// embedded in proxy URLs. Unrecognized environment entries are never a card.
pub fn endpoint_for_env(key: &str, value: &str) -> Option<(String, String)> {
    let destination = match key {
        "FLOTILLA_DAEMON_SOCKET" => "Flotilla daemon",
        "FORGEJO_API_URL" => "Forgejo API",
        "DISPLAY" | "WAYLAND_DISPLAY" => "GUI display",
        _ => match key.to_ascii_uppercase().as_str() {
            "HTTP_PROXY" => "HTTP egress proxy",
            "HTTPS_PROXY" => "HTTPS egress proxy",
            "ALL_PROXY" => "Egress proxy",
            _ => return None,
        },
    };
    let address = if destination.contains("proxy") || destination == "Forgejo API" {
        let mut address = reqwest::Url::parse(value).ok()?;
        address.host_str()?;
        address.set_username("").ok()?;
        address.set_password(None).ok()?;
        if destination.contains("proxy") {
            address.set_path("");
        }
        address.set_query(None);
        address.set_fragment(None);
        address.to_string()
    } else {
        value.to_string()
    };
    Some((destination.to_string(), address))
}

pub fn credential_card(credentials: &[CredentialCapability]) -> String {
    let mut text = String::new();
    for credential in credentials {
        text.push_str(&format!("- Credential `{}`: repositories [{}]; permissions: ", credential.name, credential.repositories.join(", ")));
        match &credential.permissions {
            Some(permissions) => {
                text.push_str(&permissions.iter().map(|(name, level)| format!("{name}: {level}")).collect::<Vec<_>>().join(", "))
            }
            None => text.push_str("not reported by the credential adapter"),
        }
        text.push('\n');
    }
    let writable = credentials
        .iter()
        .filter(|credential| {
            credential.permissions.as_ref().is_some_and(|permissions| {
                permissions.get("workflows").is_some_and(|level| level == "write")
                    && permissions.get("contents").is_some_and(|level| level == "write")
            })
        })
        .flat_map(|credential| credential.repositories.iter().cloned())
        .collect::<std::collections::BTreeSet<_>>();
    if writable.is_empty() {
        text.push_str("You can supply `.github/workflows` changes as a fenced diff in the PR description under 'Operator-applied workflow change', keeping them out of your commits; the operator applies it.\n");
    } else {
        text.push_str(&format!("You can push `.github/workflows` changes in [{}].\n", writable.into_iter().collect::<Vec<_>>().join(", ")));
        text.push_str(
            "For other repositories in scope, supply workflow changes as an operator-applied fenced diff in the PR description.\n",
        );
    }
    text
}

pub async fn session_card(
    backend: &ResourceBackend,
    namespace: &str,
    convoy_name: &str,
    spec: &TerminalSessionSpec,
    credentials: &[CredentialCapability],
    endpoints: &BTreeMap<String, String>,
) -> Result<String, String> {
    let convoy = backend.including_replicas::<Convoy>(namespace).get(convoy_name).await.map_err(|error| error.to_string())?.object;
    let mut text = format!(
        "## Your capabilities\n\nYou act as `{}` in project `{}`.\n\nRepositories in scope:\n",
        spec.role,
        convoy.spec.project_ref.as_deref().unwrap_or(namespace)
    );
    for repository in &convoy.spec.repositories {
        text.push_str(&format!("- {} ({})\n", repository.url, repository.repo_ref));
    }
    let repository_scope = match &spec.source {
        flotilla_resources::TerminalSessionSource::Agent { context, .. } => {
            let vessel = backend
                .including_replicas::<flotilla_resources::Vessel>(namespace)
                .get(&context.vessel_ref)
                .await
                .map_err(|error| error.to_string())?
                .object;
            let policy = flotilla_resources::vessel_placement_pin(&convoy, &vessel.spec.vessel_name)
                .map(|pin| pin.decision.policy_name)
                .or_else(|| {
                    convoy
                        .status
                        .as_ref()
                        .and_then(|status| status.placement_decision.as_ref().map(|decision| decision.policy_name.clone()))
                });
            if let Some(policy) = policy {
                let kind = backend
                    .including_replicas::<flotilla_resources::FulfilmentKind>(namespace)
                    .get(&policy)
                    .await
                    .map_err(|error| error.to_string())?
                    .object;
                text.push_str("\nEnvironment grants:\n");
                for grant in &kind.spec.grants {
                    let description = match grant.0.as_str() {
                        "network:host" => "use host networking and its available egress",
                        "network:scoped" => "use networking within the provisioned scope",
                        "display:gui-session" => "use a display / GUI session",
                        "runtime:container" => "run containers with the available container runtime",
                        _ => &grant.0,
                    };
                    text.push_str(&format!("- You can {description}.\n"));
                }
            }
            convoy
                .status
                .as_ref()
                .and_then(|status| status.workflow_snapshot.as_ref())
                .and_then(|workflow| workflow.vessels.iter().find(|requirement| requirement.name == vessel.spec.vessel_name))
                .and_then(|requirement| requirement.repository_refs.clone())
        }
        _ => None,
    };
    if let Some(scope) = repository_scope {
        text.push_str(&format!(
            "Your vessel's repository scope: {}.\n",
            scope.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ")
        ));
    }
    text.push_str(&credential_card(credentials));
    text.push_str(&format!("\nYou work at `{}` in environment `{}`.\n", spec.cwd, spec.env_ref));
    let environment =
        backend.including_replicas::<Environment>(namespace).get(&spec.env_ref).await.map_err(|error| error.to_string())?.object;
    if let Some(direct) = &environment.spec.host_direct {
        text.push_str(&format!("Placement: host-direct on `{}`.\n", direct.host_ref));
    }
    if let Some(docker) = &environment.spec.docker {
        text.push_str(&format!("Placement: contained on `{}`, image `{}`.\n", docker.host_ref, docker.image));
        for mount in &docker.mounts {
            text.push_str(&format!("- Mount `{}` → `{}` ({:?})\n", mount.source_path, mount.target_path, mount.mode));
        }
        if let Some(build) = &docker.image_build_ref {
            let build = backend.including_replicas::<ImageBuild>(namespace).get(build).await.map_err(|error| error.to_string())?.object;
            if let Some(status) = &build.status {
                text.push_str(&format!(
                    "Verified tools and runtimes: {}.\n",
                    status.verified_provides.iter().cloned().collect::<Vec<_>>().join(", ")
                ));
            }
        }
    }
    text.push_str("\nAvailable endpoints (local address → destination):\n");
    for (destination, address) in endpoints {
        text.push_str(&format!("- `{address}` → {destination}\n"));
    }
    if endpoints.is_empty() {
        text.push_str("No endpoints are recorded for this session.\n");
    }
    text.push_str("\nScope: use the repositories and credential permissions listed above. Run `flotilla crew capabilities` when unsure; it reports the current delivery.\n");
    Ok(text)
}

const CARD_ANNOTATION: &str = "flotilla.work/capabilities-card";
const REVISION_ANNOTATION: &str = "flotilla.work/capabilities-revision";
const MESSAGE_ANNOTATION: &str = "flotilla.work/capabilities-message";

/// Persist the launch observation without submitting another turn. Later
/// changes publish a superseding system Message through the existing inbox.
pub async fn observe_card(
    backend: &ResourceBackend,
    namespace: &str,
    session: &flotilla_resources::ResourceObject<flotilla_resources::TerminalSession>,
    card: &str,
) -> Result<(), String> {
    use flotilla_resources::{
        InputMeta, MessageReference, MessageRelation, MessageSpec, ResourceRef, TerminalSession, TerminalSessionSource,
    };
    if session.metadata.annotations.get(CARD_ANNOTATION).is_some_and(|previous| previous == card) {
        return Ok(());
    }
    let mut meta = InputMeta::from(&session.metadata);
    let revision = session
        .metadata
        .annotations
        .get(REVISION_ANNOTATION)
        .map(|value| value.parse::<u64>())
        .transpose()
        .map_err(|error| error.to_string())?
        .unwrap_or(0)
        .checked_add(1)
        .ok_or("capabilities revision overflow")?;
    if session.metadata.annotations.contains_key(CARD_ANNOTATION) {
        let TerminalSessionSource::Agent { context, .. } = &session.spec.source else { return Ok(()) };
        let convoy = backend.including_replicas::<Convoy>(namespace).get(&context.convoy).await.map_err(|error| error.to_string())?.object;
        let vessel = backend
            .including_replicas::<flotilla_resources::Vessel>(namespace)
            .get(&context.vessel_ref)
            .await
            .map_err(|error| error.to_string())?
            .object;
        let receiver = format!(
            "{}/{}/{}/{}",
            convoy.spec.project_ref.as_deref().unwrap_or(namespace),
            context.convoy,
            vessel.spec.vessel_name,
            session.spec.role
        );
        let sender = "system:capabilities";
        let name =
            flotilla_resources::message_record_name(&receiver, sender, &format!("{}:capabilities@{revision}", session.metadata.name));
        let subject = MessageReference::ControlRecord {
            resource: ResourceRef::new("flotilla.work/v1", "TerminalSession", namespace, &session.metadata.name),
            revision: format!("capabilities@{revision}"),
        };
        let spec = MessageSpec::builder()
            .sender(sender.to_string())
            .receiver(receiver)
            .relation(MessageRelation::System)
            .body(card.to_string())
            .references(vec![subject.clone()])
            .subject(subject)
            .maybe_supersedes(session.metadata.annotations.get(MESSAGE_ANNOTATION).cloned())
            .build();
        let messages = backend.clone().using::<flotilla_resources::Message>(namespace);
        if let Ok(existing) = messages.get(&name).await {
            if existing.spec.body != card {
                let mut expected = spec.clone();
                expected.body = existing.spec.body.clone();
                if expected != existing.spec {
                    return Err("capability Message identity collision".into());
                }
                // Publication may have succeeded before a concurrent status write
                // prevented recording its observation. Recover that revision first;
                // the next refresh can supersede it with the newest observation.
                flotilla_resources::MessageInbox::new(backend.clone(), namespace)
                    .accept(&InputMeta::builder().name(name.clone()).build(), &existing.spec, chrono::Utc::now())
                    .await
                    .map_err(|error| error.to_string())?;
                meta.annotations.insert(CARD_ANNOTATION.into(), existing.spec.body);
                meta.annotations.insert(REVISION_ANNOTATION.into(), revision.to_string());
                meta.annotations.insert(MESSAGE_ANNOTATION.into(), name);
                backend
                    .clone()
                    .using::<TerminalSession>(namespace)
                    .update(&meta, &session.metadata.resource_version, &session.spec)
                    .await
                    .map_err(|error| error.to_string())?;
                return Ok(());
            }
        }
        // Admit immediately so every predecessor is superseded before another
        // revision can be published, regardless of store listing order.
        flotilla_resources::MessageInbox::new(backend.clone(), namespace)
            .accept(&InputMeta::builder().name(name.clone()).build(), &spec, chrono::Utc::now())
            .await
            .map_err(|error| error.to_string())?;
        meta.annotations.insert(MESSAGE_ANNOTATION.into(), name);
    }
    meta.annotations.insert(CARD_ANNOTATION.into(), card.into());
    meta.annotations.insert(REVISION_ANNOTATION.into(), revision.to_string());
    backend
        .clone()
        .using::<TerminalSession>(namespace)
        .update(&meta, &session.metadata.resource_version, &session.spec)
        .await
        .map_err(|error| error.to_string())?;
    Ok(())
}

pub async fn refresh_cards(backend: &ResourceBackend, namespace: &str, source: &dyn SessionCapabilitySource) -> Result<(), String> {
    use flotilla_resources::{TerminalSession, TerminalSessionPhase, TerminalSessionSource};
    for session in backend.clone().using::<TerminalSession>(namespace).list().await.map_err(|error| error.to_string())?.items {
        if !session.status.as_ref().is_some_and(|status| status.phase == TerminalSessionPhase::Running) {
            continue;
        }
        let TerminalSessionSource::Agent { context, .. } = &session.spec.source else { continue };
        let credentials = source.credentials(&session.spec.env_ref, &credential_references(&session)?).await?;
        let card =
            session_card(backend, namespace, &context.convoy, &session.spec, &credentials, &source.endpoints(&session.spec.env_ref).await?)
                .await?;
        observe_card(backend, namespace, &session, &card).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Proxy endpoints retain the destination, never URL authentication or
    // credential-bearing paths, queries and fragments.
    #[hegel::test]
    fn proxy_endpoints_drop_authentication(tc: hegel::TestCase) {
        use hegel::generators as gs;
        let key = ["HTTP_PROXY", "https_proxy", "ALL_PROXY"][tc.draw(gs::integers::<usize>().min_value(0).max_value(2))];
        let port = tc.draw(gs::integers::<u16>());
        let original = format!("https://private-user:private-password@127.0.0.1:{port}/private-path?private-query=value#private-fragment");
        let (_, endpoint) = endpoint_for_env(key, &original).expect("valid proxy endpoint");
        let public = reqwest::Url::parse(&endpoint).expect("public URL");
        assert_eq!(public.host_str(), Some("127.0.0.1"));
        assert_eq!(public.port_or_known_default(), Some(port));
        assert!(public.username().is_empty());
        assert!(public.password().is_none());
        assert!(public.query().is_none());
        assert!(public.fragment().is_none());
        assert!(!endpoint.contains("private"));
        assert!(endpoint_for_env("GITHUB_TOKEN", "private-token").is_none());
        assert!(endpoint_for_env(key, "not a URL").is_none());
    }

    // Workflow push guidance follows effective permission, independently of role.
    #[hegel::test]
    fn workflow_guidance_follows_effective_permission(tc: hegel::TestCase) {
        use hegel::generators as gs;
        // Covers missing, read, write, empty and additional unrelated grants.
        let workflow = tc.draw(gs::integers::<usize>().min_value(0).max_value(2));
        let mut permissions = BTreeMap::new();
        let contents_write = tc.draw(gs::booleans());
        if contents_write {
            permissions.insert("contents".into(), "write".into());
        }
        if workflow > 0 {
            permissions.insert("workflows".into(), ["", "read", "write"][workflow].into());
        }
        let credential = CredentialCapability::builder()
            .name("github".into())
            .repositories(vec!["flotilla-org/flotilla".into()])
            .permissions(permissions)
            .build();
        let text = credential_card(&[credential]);
        let can_push = workflow == 2 && contents_write;
        assert_eq!(text.contains("You can push `.github/workflows`"), can_push);
        assert_eq!(text.contains("the operator applies it"), !can_push);
        assert!(text.contains("flotilla-org/flotilla"));
    }
}
