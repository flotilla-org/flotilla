//! Session capability reporting. Credential material stays behind the runtime
//! port; cards contain only scope and effective permissions.
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use flotilla_agent::adapter::CAPABILITIES_HEADING;
use flotilla_capability_contract::{credential_card, CredentialCapability, SessionCapabilitySource};
use flotilla_resources::account_access::AccountRecords;
use flotilla_resources::lifecycle_reads::LifecycleReads;
use flotilla_resources::terminal_records::{newest_launches, TerminalRecord};
use flotilla_resources::vessel_records::VesselRecord;
use flotilla_resources::{
    message_record_name, vessel_placement_pin, Convoy, EnvironmentMount, EnvironmentMountMode, FulfilmentKind, MessageRelation,
    ResourceError, ResourceObject, TerminalSession, TerminalSessionPhase, TerminalSessionSource, TerminalSessionSpec, Vessel,
};
use flotilla_store::{load_metrics::carry, ResourceBackend};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;
use tracing::warn;

use crate::reconcilers::vessel::vessel_requirement;

/// The credentials a crew session is granted: its vessel's requirement in its convoy's frozen
/// workflow snapshot. A session held for no vessel is granted none.
pub async fn credential_references(
    backend: &ResourceBackend,
    namespace: &str,
    session: &ResourceObject<TerminalSession>,
) -> Result<BTreeSet<String>, String> {
    let TerminalSessionSource::Agent { context, .. } = &session.spec.source else { return Ok(BTreeSet::new()) };
    Ok(vessel_requirement(backend, namespace, &context.vessel_ref).await?.credential_refs)
}

/// Verify image capabilities from the selected origin's checked lifecycle reading.
#[allow(clippy::too_many_arguments)]
pub async fn session_card(
    backend: &ResourceBackend,
    namespace: &str,
    convoy_name: &str,
    spec: &TerminalSessionSpec,
    credentials: &[CredentialCapability],
    endpoints: &BTreeMap<String, String>,
    reads: Arc<dyn LifecycleReads>,
    accounts: &dyn AccountRecords,
) -> Result<String, String> {
    let convoy = backend.including_replicas::<Convoy>(namespace).get(convoy_name).await.map_err(|error| error.to_string())?.object;
    let work = match &spec.source {
        TerminalSessionSource::Agent { context, .. } => Some(
            backend
                .including_replicas::<Vessel>(namespace)
                .get(&context.vessel_ref)
                .await
                .map_err(|error| error.to_string())?
                .object
                .spec
                .vessel_name,
        ),
        _ => None,
    };
    let card = CardRead { backend, namespace, convoy: &convoy, work: work.as_deref() };
    render_card(&card, spec, credentials, endpoints, reads, accounts).await
}

/// What a card is read from besides its session: the session's convoy and, for a crew, the work
/// its vessel serves in it.
#[derive(Clone, Copy)]
struct CardRead<'a> {
    backend: &'a ResourceBackend,
    namespace: &'a str,
    convoy: &'a ResourceObject<Convoy>,
    work: Option<&'a str>,
}

async fn render_card(
    read: &CardRead<'_>,
    spec: &TerminalSessionSpec,
    credentials: &[CredentialCapability],
    endpoints: &BTreeMap<String, String>,
    reads: Arc<dyn LifecycleReads>,
    accounts: &dyn AccountRecords,
) -> Result<String, String> {
    let CardRead { backend, namespace, convoy, work } = *read;
    let convoy_name = convoy.metadata.name.as_str();
    let mut text = format!(
        "{}\n\nYou act as `{}` in project `{}`.\n\n### Repositories in scope\n",
        CAPABILITIES_HEADING.trim_start(),
        spec.role,
        convoy.spec.project_ref.as_deref().unwrap_or(namespace)
    );
    for repository in &convoy.spec.repositories {
        text.push_str(&format!("- {} ({})\n", repository.url, repository.repo_ref));
    }
    let repository_scope = match work {
        Some(work) => {
            let policy = vessel_placement_pin(convoy, work).map(|pin| pin.decision.kind).or_else(|| {
                convoy.status.as_ref().and_then(|status| status.placement_decision.as_ref().map(|decision| decision.kind.clone()))
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
                .and_then(|workflow| workflow.vessels.iter().find(|requirement| requirement.name == work))
                .and_then(|requirement| requirement.repository_refs.clone())
        }
        None => None,
    };
    let checkouts = flotilla_agent::adapter::multi_repository_checkouts(
        &convoy.spec,
        &repository_scope
            .clone()
            .unwrap_or_else(|| convoy.spec.repositories.iter().map(|repository| repository.repo_ref.clone()).collect::<Vec<_>>()),
        &spec.cwd,
    );
    if let Some(scope) = repository_scope {
        text.push_str(&format!(
            "Your vessel's repository scope: {}.\n",
            scope.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ")
        ));
    }
    text.push_str("\n### Credentials\n");
    text.push_str(&account_card(accounts, namespace, convoy_name, &spec.role).await?);
    text.push_str(&credential_card(credentials));
    text.push_str(&format!("\n### Session\nYou work at `{}` in environment `{}`.\n", spec.cwd, spec.env_ref));
    if !checkouts.is_empty() {
        text.push_str("The work directory is the common parent of one checkout per repository; it is not itself a git repository:\n");
        for (repository, path) in &checkouts {
            let branch = convoy.spec.r#ref.as_deref().map(|branch| format!(", branch `{branch}`")).unwrap_or_default();
            text.push_str(&format!("- `{path}` — {}{branch}, remote {}\n", repository.repo_ref, repository.url));
        }
    }
    let environment = reads.environments()?.find(&spec.env_ref)?.ok_or_else(|| format!("environment {} not found", spec.env_ref))?;
    // Backing host/image identities are not reachable capabilities; an equivalent
    // placement move must not generate a capability notice.
    text.push_str("\n### Reachable paths\n");
    if environment.spec.host_direct.is_some() {
        text.push_str(&format!("- `{}` (read-write)\n", spec.cwd));
    }
    if let Some(docker) = &environment.spec.docker {
        text.push_str(&reachable_paths(&docker.mounts));
        if let Some(build) = &docker.image_build_ref {
            let (reads, name) = (Arc::clone(&reads), build.clone());
            let build = tokio::task::spawn_blocking(carry(move || reads.image_builds()?.get(&name)))
                .await
                .map_err(|error| format!("image capability read: {error}"))??;
            // Only a built image has passed every layer's probe.
            if let Some(spec) = build.filter(|build| build.digest().is_some()).map(|build| build.spec) {
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

/// The requester's open account bindings (#3907): which managed credential and account its
/// harness runs on, and how it was chosen. Never material.
async fn account_card(accounts: &dyn AccountRecords, namespace: &str, convoy: &str, role: &str) -> Result<String, String> {
    use flotilla_resources::account_access::requester_parts;
    let bindings = accounts.convoy_bindings(namespace, convoy, false).await?;
    let mut lines = bindings
        .iter()
        .map(|binding| &binding.spec)
        .filter(|spec| spec.state.value.is_open())
        .filter(|spec| requester_parts(&spec.requester).is_some_and(|(_, bound_role)| bound_role == role))
        .map(|spec| {
            let account = spec.account.as_deref().map(|account| format!(" (account {account})")).unwrap_or_default();
            format!("- Account for `{}`: credential `{}`{account}, {}.\n", spec.requirement, spec.credential, spec.describe_selection())
        })
        .collect::<Vec<_>>();
    lines.sort();
    Ok(lines.concat())
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
/// It publishes through the Message path installed at startup.
#[derive(Default)]
pub struct CapabilityCardTracker(
    Mutex<BTreeMap<(String, String), SessionCardObservation>>,
    crate::ports::MessagePath,
    /// By namespace, the sessions whose last card refresh failed.
    std::sync::Mutex<BTreeMap<String, BTreeSet<String>>>,
);

type SessionCardObservation = Arc<Mutex<CardObservation>>;

impl CapabilityCardTracker {
    /// A tracker publishing through `path` (the composition root's slot).
    pub fn with_message_path(path: crate::ports::MessagePath) -> Self {
        Self(Mutex::default(), path, std::sync::Mutex::default())
    }

    fn failed(&self, namespace: &str) -> BTreeSet<String> {
        self.2.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).get(namespace).cloned().unwrap_or_default()
    }

    /// Settles a refresh that tried `retried` again: those it tried leave the set unless `failed`
    /// names them, and a card that failed since the refresh read the set stays in it.
    fn settle_failed(&self, namespace: &str, retried: &BTreeSet<String>, failed: BTreeSet<String>) {
        let mut held = self.2.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let set = held.entry(namespace.to_owned()).or_default();
        set.retain(|session| !retried.contains(session));
        set.extend(failed);
    }

    /// Session `session`'s launch could not render its card: the next refresh of any scope tries
    /// it again.
    pub fn retry(&self, namespace: &str, session: &str) {
        self.2.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).entry(namespace.to_owned()).or_default().insert(session.to_owned());
    }

    fn primitives(&self) -> Result<Arc<dyn flotilla_messaging::EngineMessages>, ResourceError> {
        crate::ports::installed_messages(&self.1)
    }

    async fn observation(&self, namespace: &str, session: &str) -> SessionCardObservation {
        // The map lock protects lookup/pruning only. Backend I/O holds a session's
        // observation lock, so unrelated launches and refreshes proceed independently.
        Arc::clone(self.0.lock().await.entry((namespace.to_string(), session.to_string())).or_default())
    }
}

/// What this daemon has told one session's agent about its capabilities. Held in memory only:
/// after a restart the first reading of a running session is a baseline the agent may not have
/// seen, so the next change it hears of is a full card.
#[derive(Default)]
struct CardObservation {
    delivered: Option<String>,
    pending: BTreeMap<u64, (String, String)>,
    /// The digest of the newest card observed.
    digest: Option<String>,
    /// The newest published revision. Revisions are milliseconds since the epoch at least, so a
    /// restarted daemon's revisions never name an earlier daemon's Messages.
    revision: u64,
    /// The newest published revision's Message, which the next one supersedes.
    message: Option<String>,
}

fn card_digest(card: &str) -> String {
    format!(
        "{:x}",
        Sha256::digest(
            [b"capability-card-v2\0".as_slice(), serde_json::to_vec(&card_sections(card)).expect("card sections serialize").as_slice()]
                .concat()
        )
    )
}

/// Record the observation without submitting another turn. Later
/// changes publish a superseding system Message through the existing inbox.
/// Relaunches retain the chain: a changed inline card may also arrive as a Message.
pub async fn observe_card(
    backend: &ResourceBackend,
    namespace: &str,
    session: &ResourceObject<TerminalSession>,
    card: &str,
    tracker: &CapabilityCardTracker,
) -> Result<(), String> {
    observe(backend, namespace, session, card, tracker, None).await
}

/// [`observe_card`], with the session's crew address when the caller has read it already.
async fn observe(
    backend: &ResourceBackend,
    namespace: &str,
    session: &ResourceObject<TerminalSession>,
    card: &str,
    tracker: &CapabilityCardTracker,
    receiver: Option<&str>,
) -> Result<(), String> {
    let observation = tracker.observation(namespace, &session.metadata.name).await;
    let mut observation = observation.lock().await;
    let observed = card_digest(card);
    let Some(previous) = observation.digest.clone() else {
        // The first reading since this daemon started: a launch it sends with the card sets
        // `delivered` afterwards; a session launched earlier holds a card this daemon never saw.
        observation.digest = Some(observed);
        return Ok(());
    };
    if previous == observed {
        return Ok(());
    }
    let receiver = match receiver {
        Some(receiver) => receiver.to_owned(),
        None => card_receiver(backend, namespace, session).await?,
    };
    // Pending notices supersede one another. Promote only snapshots the inbox delivered.
    let mut retired = Vec::new();
    let pending = observation.pending.clone();
    let named: Vec<_> = pending.values().map(|(name, _)| (receiver.clone(), name.clone())).collect();
    let sent = tracker
        .primitives()
        .map_err(|error| error.to_string())?
        .sent_messages(namespace, &named)
        .await
        .map_err(|error| error.to_string())?;
    for (revision, (name, snapshot)) in &pending {
        let Some(message) = sent.iter().find(|message| message.standing.name == *name) else {
            // An audit record can be reaped before the next capability change.
            // Without its receipt evidence, recover with a full card.
            observation.delivered = None;
            retired.push(*revision);
            continue;
        };
        let standing = &message.standing;
        if standing.phase.has_delivery_evidence() || standing.delivered_at.is_some() {
            observation.delivered = Some(snapshot.clone());
            retired.push(*revision);
        } else if standing.phase.is_terminal() {
            retired.push(*revision);
        }
    }
    for revision in retired {
        observation.pending.remove(&revision);
    }
    let now = u64::try_from(chrono::Utc::now().timestamp_millis()).unwrap_or_default();
    let revision = observation.revision.checked_add(1).ok_or("capabilities revision overflow")?.max(now);
    let body = observation.delivered.as_ref().map(|baseline| card_delta(baseline, card)).unwrap_or_else(|| card.to_string());
    let supersedes = observation.message.clone();
    let published = CardRevision { revision, card, body: &body, supersedes };
    let (name, digest) = publish_card_revision(namespace, session, &receiver, published, tracker).await?;
    if digest == observed {
        observation.pending.insert(revision, (name.clone(), card.to_string()));
    }
    observation.digest = Some(digest);
    observation.revision = revision;
    observation.message = Some(name);
    Ok(())
}

/// One revision of a session's capability card: the card, the body sent for it (the card or its
/// delta) and the revision it supersedes.
struct CardRevision<'a> {
    revision: u64,
    card: &'a str,
    body: &'a str,
    supersedes: Option<String>,
}

async fn publish_card_revision(
    namespace: &str,
    session: &ResourceObject<TerminalSession>,
    receiver: &str,
    CardRevision { revision, card, body, supersedes }: CardRevision<'_>,
    tracker: &CapabilityCardTracker,
) -> Result<(String, String), String> {
    let inbox = flotilla_resources::convoy::engine::role_inbox(receiver)?;
    let primitives = tracker.primitives().map_err(|error| error.to_string())?;
    let sender = primitives.fleet_role(namespace, "capabilities").await.map_err(|error| error.to_string())?;
    let name = message_record_name(inbox.as_str(), sender.as_str(), &format!("{}:capabilities@{revision}", session.metadata.name));
    let supersedes = match supersedes {
        // Reaped audit data cannot be a predecessor.
        Some(name) => primitives.holds(&inbox, &name).await.map_err(|error| error.to_string())?.then_some(name),
        None => None,
    };
    // Admission is idempotent. Recover a published revision before trying to supersede it with
    // a newer observation: a message's name is its producer key's digest, the same revision of
    // the same session's card, so it differs from this intent only in its body and predecessor.
    let recovered = primitives
        .sent_messages(namespace, &[(receiver.to_owned(), name.clone())])
        .await
        .map_err(|error| error.to_string())?
        .into_iter()
        .next();
    // The engine keeps no annotations: a recovered revision's card is the body it was published with.
    let digest = recovered.as_ref().map(|message| card_digest(&message.body)).unwrap_or_else(|| card_digest(card));
    let (body, supersedes) = match recovered {
        Some(message) => (message.body, message.supersedes),
        None => (body.to_string(), supersedes),
    };
    // The card is about the seat it is for, at its revision.
    let at = flotilla_address::Revision::new(format!("capabilities@{revision}")).map_err(|error| error.to_string())?;
    let mut intent = flotilla_messaging::Intent::new(sender, inbox.clone(), MessageRelation::System, body);
    intent.references = vec![flotilla_messaging::IntentReference::subject(inbox, at)];
    intent.supersedes = supersedes;
    primitives.admit(&name, &intent).await.map_err(|error| error.to_string())?;
    Ok((name, digest))
}

/// The crew address a session's capability Messages are sent to: its role in its convoy's vessel.
async fn card_receiver(backend: &ResourceBackend, namespace: &str, session: &ResourceObject<TerminalSession>) -> Result<String, String> {
    let TerminalSessionSource::Agent { context, .. } = &session.spec.source else {
        return Err("capability Messages require an agent session".into());
    };
    let convoy = backend.including_replicas::<Convoy>(namespace).get(&context.convoy).await.map_err(|error| error.to_string())?.object;
    let vessel = backend.including_replicas::<Vessel>(namespace).get(&context.vessel_ref).await.map_err(|error| error.to_string())?.object;
    Ok(flotilla_resources::crew_role_address(
        convoy.spec.project_ref.as_deref().unwrap_or(namespace),
        &context.convoy,
        &vessel.spec.vessel_name,
        &session.spec.role,
    ))
}

/// Advisory launch observation: callers can warn and still launch with the card.
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

/// Which of this home's sessions' cards a refresh reads.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CardScope {
    /// Every session's.
    pub every: bool,
    /// The sessions' in these environments.
    pub environments: BTreeSet<String>,
    /// The sessions' of these convoys' vessels.
    pub convoys: BTreeSet<String>,
}

impl CardScope {
    /// Every session's card.
    pub fn every() -> Self {
        Self { every: true, ..Self::default() }
    }

    /// Whether it names no card.
    pub fn is_empty(&self) -> bool {
        !self.every && self.environments.is_empty() && self.convoys.is_empty()
    }
}

/// The newest launch of each of this home's sessions that `scope` names or `retried` names, and
/// the vessel launches read finding them, by name.
fn sessions_in(
    reads: &dyn LifecycleReads,
    scope: &CardScope,
    retried: &BTreeSet<String>,
) -> Result<(Vec<TerminalRecord>, BTreeMap<String, VesselRecord>), String> {
    let terminals = reads.terminals()?;
    let here = terminals.home().to_owned();
    let mut vessels = BTreeMap::new();
    if scope.every {
        return Ok((newest_launches(terminals.at(&here)?).into_values().collect(), vessels));
    }
    let mut found = Vec::new();
    for environment in &scope.environments {
        found.extend(terminals.of_environment(environment, &here)?);
    }
    for convoy in &scope.convoys {
        for vessel in reads.vessels()?.of_convoy(convoy)? {
            found.extend(terminals.of_vessel(&vessel.name)?);
            vessels.insert(vessel.name.clone(), vessel);
        }
    }
    // A launch found may not be its session's newest, which is read by the session's name.
    let mut named =
        found.iter().filter(|record| record.home == here).map(|record| record.session_name().to_owned()).collect::<BTreeSet<_>>();
    named.extend(retried.iter().cloned());
    let mut sessions = Vec::new();
    for name in named {
        sessions.extend(terminals.get(&name, None)?);
    }
    Ok((sessions, vessels))
}

/// Refreshes the cards of the running crew sessions at this home that `scope` names, and of those
/// whose last refresh failed, and forgets the cards of those that ended. Each card reads its
/// session's vessel and convoy once, by key.
pub async fn refresh_cards(
    backend: &ResourceBackend,
    namespace: &str,
    source: &dyn SessionCapabilitySource,
    tracker: &CapabilityCardTracker,
    reads: Arc<dyn LifecycleReads>,
    accounts: &dyn AccountRecords,
    scope: &CardScope,
) -> Result<(), String> {
    let retried = tracker.failed(namespace);
    if scope.is_empty() && retried.is_empty() {
        return Ok(());
    }
    let (sessions, mut vessels) = {
        let (reads, scope, retried) = (Arc::clone(&reads), scope.clone(), retried.clone());
        tokio::task::spawn_blocking(carry(move || sessions_in(reads.as_ref(), &scope, &retried)))
            .await
            .map_err(|error| format!("read the sessions for capability cards: {error}"))??
    };
    let phase = |record: &TerminalRecord| record.read_view(namespace, None, None, None).status.map(|status| status.phase);
    let live = sessions
        .iter()
        .filter(|record| phase(record).is_none_or(|phase| matches!(phase, TerminalSessionPhase::Starting | TerminalSessionPhase::Running)))
        .map(|record| record.session_name())
        .collect::<BTreeSet<_>>();
    let named = sessions.iter().map(|record| record.session_name()).collect::<BTreeSet<_>>();
    tracker
        .0
        .lock()
        .await
        .retain(|(space, name), _| space != namespace || live.contains(name.as_str()) || (!scope.every && !named.contains(name.as_str())));
    let mut errors = Vec::new();
    let mut failed = BTreeSet::new();
    for record in &sessions {
        if record.is_agent() && phase(record).is_none_or(|phase| phase == TerminalSessionPhase::Starting) {
            // A card to try again stays to try once its session runs.
            if retried.contains(record.session_name()) {
                failed.insert(record.session_name().to_owned());
            }
            continue;
        }
        if !record.is_agent() || phase(record) != Some(TerminalSessionPhase::Running) {
            continue;
        }
        let refreshed = refresh_record_card(backend, namespace, source, tracker, Arc::clone(&reads), accounts, record, &mut vessels).await;
        if let Err(error) = refreshed {
            warn!(session = %record.session_name(), %error, "failed to refresh session capability card");
            errors.push(format!("{}: {error}", record.session_name()));
            failed.insert(record.session_name().to_owned());
        }
    }
    // A failed card is tried again at the next refresh, whatever it names; one that ended is not.
    tracker.settle_failed(namespace, &retried, failed);
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

/// Refreshes the card of running crew launch `record`, reading its vessel from `vessels` when a
/// convoy's reading found it, else by key, and its convoy by key.
#[allow(clippy::too_many_arguments)]
async fn refresh_record_card(
    backend: &ResourceBackend,
    namespace: &str,
    source: &dyn SessionCapabilitySource,
    tracker: &CapabilityCardTracker,
    reads: Arc<dyn LifecycleReads>,
    accounts: &dyn AccountRecords,
    record: &TerminalRecord,
    vessels: &mut BTreeMap<String, VesselRecord>,
) -> Result<(), String> {
    let name = record.vessel.clone().unwrap_or_default();
    let vessel = match vessels.get(&name) {
        Some(vessel) => Ok(Some(vessel.clone())),
        None => reads.vessels().and_then(|held| match held.get(&name, Some(&record.home))? {
            Some(vessel) => Ok(Some(vessel)),
            None => held.get(&name, None),
        }),
    };
    let vessel = match vessel {
        Ok(Some(vessel)) => vessel,
        Ok(None) => return Err(format!("read vessel {name}: not found")),
        Err(error) => return Err(format!("read vessel {name}: {error}")),
    };
    vessels.insert(name, vessel.clone());
    {
        let convoy = backend
            .including_replicas::<Convoy>(namespace)
            .get(&vessel.convoy)
            .await
            .map_err(|error| format!("read convoy {}: {error}", vessel.convoy))?
            .object;
        let work = vessel.work();
        let credential_refs = convoy
            .status
            .as_ref()
            .and_then(|status| status.workflow_snapshot.as_ref())
            .and_then(|snapshot| snapshot.vessels.iter().find(|requirement| requirement.name == work))
            .map(|requirement| requirement.credential_refs.clone())
            .ok_or_else(|| format!("vessel {work} missing from convoy {}'s snapshot", convoy.metadata.name))?;
        let session = record.read_view(namespace, Some(&vessel.terminal_holder()), None, None);
        let credentials = source.credentials(&session.spec.env_ref, &credential_refs).await?;
        let endpoints = source.endpoints(&session.spec.env_ref, &session.metadata.name).await?;
        let read = CardRead { backend, namespace, convoy: &convoy, work: Some(work) };
        let card = render_card(&read, &session.spec, &credentials, &endpoints, reads, accounts).await?;
        let receiver = flotilla_resources::crew_role_address(
            convoy.spec.project_ref.as_deref().unwrap_or(namespace),
            &vessel.convoy,
            work,
            &session.spec.role,
        );
        observe(backend, namespace, &session, &card, tracker, Some(&receiver)).await
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
