//! Session capability reporting. Credential material stays behind the runtime
//! port; cards contain only scope and effective permissions.
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use flotilla_agent::adapter::CAPABILITIES_HEADING;
use flotilla_capability_contract::{credential_card, CredentialCapability, SessionCapabilitySource};
use flotilla_resources::{
    message_record_name, vessel_placement_pin, Convoy, Environment, EnvironmentMount, EnvironmentMountMode, FulfilmentKind, ImageBuild,
    ImageBuildPhase, InputMeta, Message, MessageReference, MessageRelation, MessageSpec, ResourceError, ResourceObject, ResourceRef,
    TerminalSession, TerminalSessionPhase, TerminalSessionSource, TerminalSessionSpec, Vessel, CREDENTIAL_REFS_ANNOTATION,
};
use flotilla_store::{MessageInbox, ResourceBackend};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;
use tracing::warn;

pub fn credential_references(session: &ResourceObject<TerminalSession>) -> Result<BTreeSet<String>, String> {
    session
        .metadata
        .annotations
        .get(CREDENTIAL_REFS_ANNOTATION)
        .map(|encoded| serde_json::from_str(encoded).map_err(|error| format!("invalid session credential references: {error}")))
        .transpose()
        .map(|references| references.unwrap_or_default())
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
        "{}\n\nYou act as `{}` in project `{}`.\n\n### Repositories in scope\n",
        CAPABILITIES_HEADING.trim_start(),
        spec.role,
        convoy.spec.project_ref.as_deref().unwrap_or(namespace)
    );
    for repository in &convoy.spec.repositories {
        text.push_str(&format!("- {} ({})\n", repository.url, repository.repo_ref));
    }
    let repository_scope = match &spec.source {
        TerminalSessionSource::Agent { context, .. } => {
            let vessel =
                backend.including_replicas::<Vessel>(namespace).get(&context.vessel_ref).await.map_err(|error| error.to_string())?.object;
            let policy = vessel_placement_pin(&convoy, &vessel.spec.vessel_name).map(|pin| pin.decision.policy_name).or_else(|| {
                convoy.status.as_ref().and_then(|status| status.placement_decision.as_ref().map(|decision| decision.policy_name.clone()))
            });
            if let Some(policy) = policy {
                let kind =
                    backend.including_replicas::<FulfilmentKind>(namespace).get(&policy).await.map_err(|error| error.to_string())?.object;
                text.push_str("\n### Environment grants\n");
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
    text.push_str("\n### Credentials\n");
    text.push_str(&credential_card(credentials));
    text.push_str(&format!("\n### Session\nYou work at `{}` in environment `{}`.\n", spec.cwd, spec.env_ref));
    let environment =
        backend.including_replicas::<Environment>(namespace).get(&spec.env_ref).await.map_err(|error| error.to_string())?.object;
    // Backing host/image identities are not reachable capabilities; an equivalent
    // placement move must not generate a capability notice.
    text.push_str("\n### Reachable paths\n");
    if environment.spec.host_direct.is_some() {
        text.push_str(&format!("- `{}` (read-write)\n", spec.cwd));
    }
    if let Some(docker) = &environment.spec.docker {
        text.push_str(&reachable_paths(&docker.mounts));
        if let Some(build) = &docker.image_build_ref {
            let build = backend.including_replicas::<ImageBuild>(namespace).get(build).await.map_err(|error| error.to_string())?.object;
            // A Built image passed every layer's probe, so its layers' provides are verified.
            if let (Some(spec), Some(ImageBuildPhase::Built)) = (build.spec.current(), build.status.as_ref().map(|status| status.phase)) {
                let provides = spec.layers.iter().flat_map(|layer| layer.provides.iter().cloned()).collect::<BTreeSet<_>>();
                text.push_str(&format!("Verified tools and runtimes: {}.\n", provides.into_iter().collect::<Vec<_>>().join(", ")));
            }
        }
    }
    text.push_str("\n### Available endpoints (local address → destination)\n");
    for (destination, address) in endpoints {
        text.push_str(&format!("- `{address}` → {destination}\n"));
    }
    if endpoints.is_empty() {
        text.push_str("No endpoints are recorded for this session.\n");
    }
    text.push_str("\nScope: use the repositories and credential permissions listed above. Run `flotilla crew capabilities` when unsure; it reports the current delivery.\n");
    Ok(text)
}

fn mount_mode(mode: EnvironmentMountMode) -> &'static str {
    match mode {
        EnvironmentMountMode::Ro => "read-only",
        EnvironmentMountMode::Rw => "read-write",
    }
}

/// Render targets only. Git plumbing belongs to the repository, not the path tree.
pub fn reachable_paths(mounts: &[EnvironmentMount]) -> String {
    let mut paths = BTreeMap::new();
    let mut git = BTreeMap::<String, BTreeMap<String, EnvironmentMountMode>>::new();
    for mount in mounts {
        let components = mount.target_path.split('/').collect::<Vec<_>>();
        if let Some(index) = components.iter().position(|component| *component == ".git") {
            let repository = components[..index].join("/");
            git.entry(repository).or_default().insert(components[index + 1..].join("/"), mount.mode);
        } else {
            paths.insert(mount.target_path.clone(), mount.mode);
        }
    }
    let mut text = String::new();
    for (path, mode) in &paths {
        let ancestors = path.match_indices('/').filter(|(index, _)| *index > 0 && paths.contains_key(&path[..*index])).count()
            + usize::from(path != "/" && paths.contains_key("/"));
        text.push_str(&format!("{}- `{path}` ({})\n", "  ".repeat(ancestors), mount_mode(*mode)));
    }
    for (repository, modes) in git {
        let name = repository.rsplit('/').next().unwrap_or("repository");
        let base = modes.get("").map(|mode| mount_mode(*mode)).unwrap_or("not mounted");
        let mut details = Vec::new();
        for (path, mode) in &modes {
            if path.is_empty() {
                continue;
            }
            // Collapse every checkout-specific worktrees entry, retaining each effective mode.
            let part = if path.starts_with("worktrees/") { "checkout metadata" } else { path.as_str() };
            details.push(format!("{part} {}", mount_mode(*mode)));
        }
        details.sort();
        details.dedup();
        text.push_str(&format!(
            "- Git metadata for {name}: {base}{}\n",
            if details.is_empty() { String::new() } else { format!(", {}", details.join(", ")) }
        ));
    }
    text
}

// The Scope footer is static consultation guidance. Actual repository/vessel scope,
// grants, credentials and reachable endpoints/paths are retained as section rows.
fn card_sections(card: &str) -> BTreeMap<String, BTreeSet<String>> {
    let mut sections = BTreeMap::<String, BTreeSet<String>>::new();
    let mut section = "Session".to_string();
    for line in card.lines().filter(|line| !line.trim().is_empty()) {
        if let Some(heading) = line.strip_prefix("### ") {
            section = heading.to_string();
        } else if !line.starts_with("## ") && !line.starts_with("Scope: ") {
            sections.entry(section.clone()).or_default().insert(line.to_string());
        }
    }
    sections
}

fn line_identity(line: &str) -> &str {
    // Parse the renderer's row delimiters, not punctuation inside names or target paths.
    if line.trim_start().starts_with("- Credential `") {
        return line.rsplit_once("`: repositories").map(|(name, _)| name).unwrap_or(line);
    }
    if let Some((_, destination)) = line.rsplit_once(" → ") {
        return destination;
    }
    if line.trim_start().starts_with("- `") {
        return line.rsplit_once("` (").map(|(path, _)| path.trim_start()).unwrap_or(line);
    }
    if line.starts_with("- Git metadata for ") {
        return line.rsplit_once(": ").map(|(name, _)| name).unwrap_or(line);
    }
    line
}

/// Changes are explicit and grouped; unchanged sections and rows never repeat.
pub fn card_delta(previous: &str, current: &str) -> String {
    let before = card_sections(previous);
    let after = card_sections(current);
    let mut text = String::from("## Capability changes\n");
    for section in before.keys().chain(after.keys()).collect::<BTreeSet<_>>() {
        let empty = BTreeSet::new();
        let old = before.get(section).unwrap_or(&empty);
        let new = after.get(section).unwrap_or(&empty);
        let mut removed = old.difference(new).collect::<Vec<_>>();
        let mut added = new.difference(old).collect::<Vec<_>>();
        let mut changed = Vec::new();
        removed.retain(|old| {
            if let Some(index) = added.iter().position(|new| line_identity(old) == line_identity(new)) {
                changed.push((*old, added.remove(index)));
                false
            } else {
                true
            }
        });
        if removed.is_empty() && added.is_empty() && changed.is_empty() {
            continue;
        }
        text.push_str(&format!("\n### {section}\n{} added, {} removed, {} changed.\n", added.len(), removed.len(), changed.len()));
        for (old, new) in changed {
            text.push_str(&format!("Changed: {old} → {new}\n"));
        }
        for line in removed {
            text.push_str(&format!("Removed: {line}\n"));
        }
        for line in added {
            text.push_str(&format!("Added: {line}\n"));
        }
    }
    text
}

/// Ephemeral baselines preserve stored resource shapes. Pending snapshots survive
/// annotation CAS retries; delivery evidence advances the agent's delta baseline.
#[derive(Default)]
pub struct CapabilityCardTracker(Mutex<BTreeMap<(String, String), SessionCardObservation>>);

type SessionCardObservation = Arc<Mutex<CardObservation>>;

impl CapabilityCardTracker {
    async fn observation(&self, namespace: &str, session: &str) -> SessionCardObservation {
        // The map lock protects lookup/pruning only. Backend I/O holds a session's
        // observation lock, so unrelated launches and refreshes proceed independently.
        Arc::clone(self.0.lock().await.entry((namespace.to_string(), session.to_string())).or_default())
    }
}

#[derive(Default)]
struct CardObservation {
    delivered: Option<String>,
    pending: BTreeMap<u64, (String, String)>,
}

const DIGEST_ANNOTATION: &str = "flotilla.work/capabilities-digest";
const REVISION_ANNOTATION: &str = "flotilla.work/capabilities-revision";
const MESSAGE_ANNOTATION: &str = "flotilla.work/capabilities-message";

fn card_digest(card: &str) -> String {
    format!(
        "{:x}",
        Sha256::digest(
            [b"capability-card-v2\0".as_slice(), serde_json::to_vec(&card_sections(card)).expect("card sections serialize").as_slice()]
                .concat()
        )
    )
}

/// Persist the launch observation without submitting another turn. Later
/// changes publish a superseding system Message through the existing inbox.
/// Relaunches retain the chain: a changed inline card may also arrive as a Message.
pub async fn observe_card(
    backend: &ResourceBackend,
    namespace: &str,
    session: &ResourceObject<TerminalSession>,
    card: &str,
    tracker: &CapabilityCardTracker,
) -> Result<(), String> {
    let observation = tracker.observation(namespace, &session.metadata.name).await;
    let mut observation = observation.lock().await;
    let previous = session.metadata.annotations.get(DIGEST_ANNOTATION).cloned();
    if previous.as_ref().is_some_and(|previous| previous == &card_digest(card)) {
        if observation.delivered.is_none()
            && observation.pending.is_empty()
            && !session.metadata.annotations.contains_key(MESSAGE_ANNOTATION)
        {
            observation.delivered = Some(card.to_string());
        }
        return Ok(());
    }
    // Pending notices supersede one another. Promote only snapshots the inbox delivered.
    let mut retired = Vec::new();
    let pending = observation.pending.clone();
    for (revision, (name, snapshot)) in &pending {
        let message = match backend.clone().using::<Message>(namespace).get(name).await {
            Ok(message) => message,
            Err(ResourceError::NotFound { .. }) => {
                // An audit record can be reaped before the next capability change.
                // Without its receipt evidence, recover with a full card.
                observation.delivered = None;
                retired.push(*revision);
                continue;
            }
            Err(error) => return Err(error.to_string()),
        };
        if message.status.as_ref().is_some_and(|status| status.phase.has_delivery_evidence() || status.resolved_receiver.is_some()) {
            observation.delivered = Some(snapshot.clone());
            retired.push(*revision);
        } else if message.status.as_ref().is_some_and(|status| status.phase.is_terminal()) {
            retired.push(*revision);
        }
    }
    for revision in retired {
        observation.pending.remove(&revision);
    }
    let revision = session
        .metadata
        .annotations
        .get(REVISION_ANNOTATION)
        .map(|value| value.parse::<u64>().map_err(|error| format!("invalid {REVISION_ANNOTATION} value {value:?}: {error}")))
        .transpose()?
        .unwrap_or(0)
        .checked_add(1)
        .ok_or("capabilities revision overflow")?;
    let mut meta = InputMeta::from(&session.metadata);
    let observed_digest = if previous.is_some() {
        let body = observation.delivered.as_ref().map(|baseline| card_delta(baseline, card)).unwrap_or_else(|| card.to_string());
        let (name, digest) = publish_card_revision(backend, namespace, session, revision, card, &body).await?;
        if digest == card_digest(card) {
            observation.pending.insert(revision, (name.clone(), card.to_string()));
        }
        meta.annotations.insert(MESSAGE_ANNOTATION.into(), name);
        digest
    } else {
        observation.delivered = Some(card.to_string());
        card_digest(card)
    };
    meta.annotations.insert(DIGEST_ANNOTATION.into(), observed_digest);
    meta.annotations.insert(REVISION_ANNOTATION.into(), revision.to_string());
    backend
        .clone()
        .using::<TerminalSession>(namespace)
        .update(&meta, &session.metadata.resource_version, &session.spec)
        .await
        .map_err(|error| error.to_string())?;
    Ok(())
}

/// Admission is idempotent. Recover a published revision whose annotation CAS
/// lost a race before trying to supersede it with a newer observation.
async fn recover_published_revision(
    backend: &ResourceBackend,
    namespace: &str,
    name: &str,
    requested: &MessageSpec,
) -> Result<Option<ResourceObject<Message>>, String> {
    let messages = backend.clone().using::<Message>(namespace);
    match messages.get(name).await {
        Ok(existing) => {
            let mut expected = requested.clone();
            expected.body = existing.spec.body.clone();
            if expected != existing.spec {
                return Err("capability Message identity collision".into());
            }
            Ok(Some(existing))
        }
        Err(ResourceError::NotFound { .. }) => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}

async fn publish_card_revision(
    backend: &ResourceBackend,
    namespace: &str,
    session: &ResourceObject<TerminalSession>,
    revision: u64,
    card: &str,
    body: &str,
) -> Result<(String, String), String> {
    let TerminalSessionSource::Agent { context, .. } = &session.spec.source else {
        return Err("capability Messages require an agent session".into());
    };
    let convoy = backend.including_replicas::<Convoy>(namespace).get(&context.convoy).await.map_err(|error| error.to_string())?.object;
    let vessel = backend.including_replicas::<Vessel>(namespace).get(&context.vessel_ref).await.map_err(|error| error.to_string())?.object;
    let receiver = format!(
        "{}/{}/{}/{}",
        convoy.spec.project_ref.as_deref().unwrap_or(namespace),
        context.convoy,
        vessel.spec.vessel_name,
        session.spec.role
    );
    let sender = "system:capabilities";
    let name = message_record_name(&receiver, sender, &format!("{}:capabilities@{revision}", session.metadata.name));
    let subject = MessageReference::ControlRecord {
        resource: ResourceRef::new("flotilla.work/v1", "TerminalSession", namespace, &session.metadata.name),
        revision: format!("capabilities@{revision}"),
    };
    let supersedes = match session.metadata.annotations.get(MESSAGE_ANNOTATION) {
        Some(name) => match backend.clone().using::<Message>(namespace).get(name).await {
            Ok(_) => Some(name.clone()),
            Err(ResourceError::NotFound { .. }) => None, // Reaped audit data cannot be a predecessor.
            Err(error) => return Err(error.to_string()),
        },
        None => None,
    };
    let requested = MessageSpec::builder()
        .sender(sender.into())
        .receiver(receiver)
        .relation(MessageRelation::System)
        .body(body.to_string())
        .references(vec![subject.clone()])
        .subject(subject)
        .maybe_supersedes(supersedes)
        .build();
    let recovered = recover_published_revision(backend, namespace, &name, &requested).await?;
    let digest = recovered
        .as_ref()
        .and_then(|message| message.metadata.annotations.get(DIGEST_ANNOTATION))
        .cloned()
        // Previous-generation Messages without this annotation always contain full cards.
        .unwrap_or_else(|| recovered.as_ref().map(|message| card_digest(&message.spec.body)).unwrap_or_else(|| card_digest(card)));
    let spec = recovered.map(|message| message.spec).unwrap_or(requested);
    MessageInbox::new(backend.clone(), namespace)
        .accept(
            &InputMeta::builder().name(name.clone()).annotations(BTreeMap::from([(DIGEST_ANNOTATION.into(), digest.clone())])).build(),
            &spec,
            chrono::Utc::now(),
        )
        .await
        .map_err(|error| error.to_string())?;
    Ok((name, digest))
}

/// Advisory launch persistence: callers can warn and still launch with the card.
pub async fn observe_launch_card(
    backend: &ResourceBackend,
    namespace: &str,
    name: &str,
    card: &str,
    tracker: &CapabilityCardTracker,
) -> Result<(), String> {
    let session = backend.clone().using::<TerminalSession>(namespace).get(name).await.map_err(|error| error.to_string())?;
    observe_card(backend, namespace, &session, card, tracker).await?;
    let observation = tracker.observation(namespace, name).await;
    let mut observation = observation.lock().await;
    observation.delivered = Some(card.to_string());
    observation.pending.clear();
    Ok(())
}

async fn refresh_session_card(
    backend: &ResourceBackend,
    namespace: &str,
    source: &dyn SessionCapabilitySource,
    session: &ResourceObject<TerminalSession>,
    tracker: &CapabilityCardTracker,
) -> Result<(), String> {
    let TerminalSessionSource::Agent { context, .. } = &session.spec.source else { return Ok(()) };
    let credentials = source.credentials(&session.spec.env_ref, &credential_references(session)?).await?;
    let card = session_card(
        backend,
        namespace,
        &context.convoy,
        &session.spec,
        &credentials,
        &source.endpoints(&session.spec.env_ref, &session.metadata.name).await?,
    )
    .await?;
    observe_card(backend, namespace, session, &card, tracker).await
}

pub async fn refresh_cards(
    backend: &ResourceBackend,
    namespace: &str,
    source: &dyn SessionCapabilitySource,
    tracker: &CapabilityCardTracker,
) -> Result<(), String> {
    let mut errors = Vec::new();
    let sessions = backend.clone().using::<TerminalSession>(namespace).list().await.map_err(|error| error.to_string())?;
    let live = sessions
        .items
        .iter()
        .filter(|session| {
            session
                .status
                .as_ref()
                .is_none_or(|status| matches!(status.phase, TerminalSessionPhase::Starting | TerminalSessionPhase::Running))
        })
        .map(|session| session.metadata.name.as_str())
        .collect::<BTreeSet<_>>();
    tracker.0.lock().await.retain(|(space, name), _| space != namespace || live.contains(name.as_str()));
    for session in sessions.items {
        if !session.status.as_ref().is_some_and(|status| status.phase == TerminalSessionPhase::Running) {
            continue;
        }
        if let Err(error) = refresh_session_card(backend, namespace, source, &session, tracker).await {
            warn!(session = %session.metadata.name, %error, "failed to refresh session capability card");
            errors.push(format!("{}: {error}", session.metadata.name));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Mount cards expose target paths, preserve modes and collapse each repository's Git plumbing.
    #[hegel::test]
    fn reachable_paths_never_render_sources(tc: hegel::TestCase) {
        use hegel::generators as gs;
        let count = tc.draw(gs::integers::<usize>().min_value(0).max_value(4));
        let root_mounted = tc.draw(gs::booleans());
        let mut mounts = Vec::new();
        if root_mounted {
            mounts.push(EnvironmentMount {
                source_path: "/private-host/root".into(),
                target_path: "/".into(),
                mode: EnvironmentMountMode::Ro,
            });
        }
        for index in 0..count {
            for (suffix, mode) in [
                ("", EnvironmentMountMode::Rw),
                ("/config", EnvironmentMountMode::Ro),
                ("/hooks", EnvironmentMountMode::Ro),
                ("/worktrees", EnvironmentMountMode::Ro),
                ("/worktrees/private-host-checkout", EnvironmentMountMode::Rw),
            ] {
                mounts.push(EnvironmentMount {
                    source_path: format!("/private-host/repo-{index}/.git{suffix}"),
                    target_path: format!("/reachable/repo-{index}/.git{suffix}"),
                    mode,
                });
            }
        }
        mounts.push(EnvironmentMount {
            source_path: "/private-host/checkout".into(),
            target_path: "/workspace".into(),
            mode: EnvironmentMountMode::Rw,
        });
        mounts.push(EnvironmentMount {
            source_path: "/private-host/config".into(),
            target_path: "/workspace/config".into(),
            mode: EnvironmentMountMode::Ro,
        });
        let card = reachable_paths(&mounts);
        assert!(!card.contains("private-host"));
        assert!(!card.contains(".git"));
        assert_eq!(card.matches("Git metadata for").count(), count);
        let indent = if root_mounted { "  " } else { "" };
        assert!(card.contains(&format!("{indent}- `/workspace` (read-write)\n{indent}  - `/workspace/config` (read-only)")));
        for index in 0..count {
            assert!(card.contains(&format!("Git metadata for repo-{index}: read-write")));
            assert!(card.contains("config read-only"));
            assert!(card.contains("hooks read-only"));
            assert!(card.contains("checkout metadata read-write"));
        }
        mounts.reverse();
        assert_eq!(reachable_paths(&mounts), card);
    }

    // Added, removed and changed rows appear only in changed sections, with a one-line summary.
    #[test]
    fn delta_renders_changes_directly() {
        let before = "## Your capabilities\n### Credentials\n- Credential `github`: repositories [repo]; permissions: contents: read\n- Credential `old`: repositories [repo]; permissions: unknown\n### Reachable paths\n- `/workspace` (read-write)\n### Endpoints\n- `127.0.0.1:1` → API\n";
        let after = "## Your capabilities\n### Credentials\n- Credential `github`: repositories [repo]; permissions: contents: write\n- Credential `new`: repositories [repo]; permissions: unknown\n### Reachable paths\n- `/workspace` (read-write)\n### Endpoints\n- `127.0.0.1:2` → API\n";
        assert_eq!(card_delta(before, after), "## Capability changes\n\n### Credentials\n1 added, 1 removed, 1 changed.\nChanged: - Credential `github`: repositories [repo]; permissions: contents: read → - Credential `github`: repositories [repo]; permissions: contents: write\nRemoved: - Credential `old`: repositories [repo]; permissions: unknown\nAdded: - Credential `new`: repositories [repo]; permissions: unknown\n\n### Endpoints\n0 added, 0 removed, 1 changed.\nChanged: - `127.0.0.1:1` → API → - `127.0.0.1:2` → API\n");
        assert_eq!(card_delta(after, after), "## Capability changes\n");
    }

    // Colons inside target paths are part of their identity, so renamed paths remain
    // additions/removals while a mode change for the same target remains a changed row.
    #[test]
    fn delta_preserves_punctuation_in_target_identity() {
        let before = "### Paths\n- `/workspace/a: removed` (read-write)\n- `/workspace/a: same` (read-only)\n";
        let after = "### Paths\n- `/workspace/a: added` (read-write)\n- `/workspace/a: same` (read-write)\n";
        let delta = card_delta(before, after);
        assert!(delta.contains("1 added, 1 removed, 1 changed."));
        assert!(delta.contains("Removed: - `/workspace/a: removed` (read-write)"));
        assert!(delta.contains("Added: - `/workspace/a: added` (read-write)"));
        assert!(delta.contains("Changed: - `/workspace/a: same` (read-only) → - `/workspace/a: same` (read-write)"));
    }

    // Row order is not a delivered capability change; the generation marker forces one upgrade card.
    #[test]
    fn digest_ignores_order_and_changes_generation() {
        let card = "## Your capabilities\n### Repositories\n- one\n- two\n";
        assert_eq!(card_digest(card), card_digest("## Your capabilities\n### Repositories\n- two\n- one\n"));
        assert_ne!(card_digest(card), format!("{:x}", Sha256::digest(card.as_bytes())));
        // Consultation guidance is informational; all actual scope remains in section rows.
        assert_eq!(
            card_digest(&format!("{card}Scope: consult the card\n")),
            card_digest(&format!("{card}Scope: updated consultation guidance\n"))
        );
    }

    // Durable observation size is fixed even when the original card is large.
    #[hegel::test]
    fn card_observation_has_bounded_digest(tc: hegel::TestCase) {
        use hegel::generators as gs;
        let length = tc.draw(gs::integers::<usize>().min_value(0).max_value(100_000));
        let card = "🛥".repeat(length);
        assert_eq!(card_digest(&card).len(), 64);
        assert_ne!(card_digest(&card), card_digest(&(card + "changed")));
    }
}
