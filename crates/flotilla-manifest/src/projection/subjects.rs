//! ADR 0051: replicated observations are the sole subject fact source.
use std::collections::{BTreeMap, BTreeSet};

use flotilla_address::Forge as AddressForge;
use flotilla_protocol::{IssueSource, Relationship, Subject, SubjectKind};
use flotilla_resources::{
    address, ForgeKind, ForgeSpec, Observation, ObservedChangeRequestState, ObservedChecks, ObservedIssueState, ObservedMergeability,
    ObservedReviewDecision,
};

use super::{
    entity, role_entity, standing_attempts, Catalog, CatalogInput, ConvoyPhase, ConvoyRow, EntityRef, MetadataTarget, MetadataValue,
    StandingRoleRow, SubjectCatalogInput,
};
use crate::keys::{
    KEY_CHANGE_REQUEST_AUTHOR, KEY_CHANGE_REQUEST_AUTHOR_OBSERVED_AT, KEY_CHANGE_REQUEST_CHECKS, KEY_CHANGE_REQUEST_CHECKS_OBSERVED_AT,
    KEY_CHANGE_REQUEST_HEAD_SHA, KEY_CHANGE_REQUEST_HEAD_SHA_OBSERVED_AT, KEY_CHANGE_REQUEST_MERGEABLE,
    KEY_CHANGE_REQUEST_MERGEABLE_OBSERVED_AT, KEY_CHANGE_REQUEST_READINESS, KEY_CHANGE_REQUEST_REVIEW_ACTIONABLE_AT_HEAD,
    KEY_CHANGE_REQUEST_REVIEW_ACTIONABLE_AT_HEAD_OBSERVED_AT, KEY_CHANGE_REQUEST_REVIEW_DECISION,
    KEY_CHANGE_REQUEST_REVIEW_DECISION_OBSERVED_AT, KEY_CHANGE_REQUEST_REVIEW_REQUESTED_FROM_OWNER,
    KEY_CHANGE_REQUEST_REVIEW_REQUESTED_FROM_OWNER_OBSERVED_AT, KEY_CHANGE_REQUEST_STATE, KEY_CHANGE_REQUEST_STATE_OBSERVED_AT,
    KEY_CHANGE_REQUEST_TITLE, KEY_CHANGE_REQUEST_TITLE_OBSERVED_AT, KEY_DISPLAY_LABEL, KEY_DISPLAY_LABEL_MEDIUM, KEY_DISPLAY_LABEL_SHORT,
    KEY_FORGE, KEY_FORGE_CHANGE_REQUEST_URL_TEMPLATE, KEY_FORGE_ISSUE_URL_TEMPLATE, KEY_FORGE_KIND, KEY_FORGE_WEB_URL, KEY_ISSUE_ASSIGNEES,
    KEY_ISSUE_ASSIGNEES_OBSERVED_AT, KEY_ISSUE_LABELS, KEY_ISSUE_LABELS_OBSERVED_AT, KEY_ISSUE_STATE, KEY_ISSUE_STATE_OBSERVED_AT,
    KEY_ISSUE_TITLE, KEY_ISSUE_TITLE_OBSERVED_AT, KEY_ISSUE_UPDATED_AT, KEY_ISSUE_UPDATED_AT_OBSERVED_AT, KEY_ROLE_ATTEMPTS,
    KEY_ROLE_CREW_SESSIONS, KEY_ROLE_CURRENT_ATTEMPT, KEY_ROLE_DESIRED_STATE, KEY_ROLE_NEXT_ATTEMPT, KEY_ROLE_RESTART_COUNT,
    KEY_ROLE_STATE, KEY_SUBJECT_ADOPTS, KEY_SUBJECT_KIND, KEY_SUBJECT_NUMBER, KEY_SUBJECT_OF, KEY_SUBJECT_OF_ADOPTS,
    KEY_SUBJECT_OF_PRODUCES, KEY_SUBJECT_OF_REFERENCES, KEY_SUBJECT_OF_SUPERSEDES, KEY_SUBJECT_OF_WORKS_ON, KEY_SUBJECT_ORPHANED,
    KEY_SUBJECT_PRODUCES, KEY_SUBJECT_REFERENCES, KEY_SUBJECT_REPOSITORY_ALIAS, KEY_SUBJECT_SCOPE, KEY_SUBJECT_SERVICE,
    KEY_SUBJECT_SUPERSEDES, KEY_SUBJECT_WORKS_ON, SEGMENT_PROJECT,
};

const BUILTIN_GITHUB_SERVICE: &str = "github.com";

/// The entity a convoy subject names, through the one stored-spelling →
/// address resolver, which also reads the encoded host and URL spellings.
fn subject_entity(forges: &[ForgeSpec], subject: &Subject) -> Option<EntityRef> {
    let address = address::subject_address(subject, forges).ok()?;
    Some(match subject.kind {
        SubjectKind::Issue => entity::issue(&address),
        SubjectKind::ChangeRequest => entity::change_request(&address),
    })
}

fn assert_subject(catalog: &mut Catalog, target: EntityRef, facts: Vec<(&'static str, MetadataValue)>) {
    // Subject links own this fact, including absence when ambiguous; an
    // awareness entry must not leave an inherited project behind.
    if let Some(prior) = catalog.facts.get_mut(&MetadataTarget::Entity(target.clone())) {
        prior.remove(SEGMENT_PROJECT);
    }
    catalog.assert_entity(target, facts, None);
}

fn forward_key(relationship: Relationship) -> &'static str {
    match relationship {
        Relationship::Produces => KEY_SUBJECT_PRODUCES,
        Relationship::Adopts => KEY_SUBJECT_ADOPTS,
        Relationship::WorksOn => KEY_SUBJECT_WORKS_ON,
        Relationship::Supersedes => KEY_SUBJECT_SUPERSEDES,
        Relationship::References => KEY_SUBJECT_REFERENCES,
    }
}

fn reverse_key(relationship: Relationship) -> &'static str {
    match relationship {
        Relationship::Produces => KEY_SUBJECT_OF_PRODUCES,
        Relationship::Adopts => KEY_SUBJECT_OF_ADOPTS,
        Relationship::WorksOn => KEY_SUBJECT_OF_WORKS_ON,
        Relationship::Supersedes => KEY_SUBJECT_OF_SUPERSEDES,
        Relationship::References => KEY_SUBJECT_OF_REFERENCES,
    }
}

// Only supported observation types can enter the projection. Changes to a
// resource field or enum require an explicit mapping here at compile time.
trait PresentationValue {
    fn presentation_value(&self) -> MetadataValue;
}
impl PresentationValue for String {
    fn presentation_value(&self) -> MetadataValue {
        MetadataValue::text(self)
    }
}
impl PresentationValue for bool {
    fn presentation_value(&self) -> MetadataValue {
        MetadataValue::Bool(*self)
    }
}
impl PresentationValue for Vec<String> {
    fn presentation_value(&self) -> MetadataValue {
        MetadataValue::StringList(self.clone())
    }
}
impl PresentationValue for super::Timestamp {
    fn presentation_value(&self) -> MetadataValue {
        MetadataValue::text(self.to_rfc3339())
    }
}
macro_rules! observation_enum {
    ($type:ty, {$($variant:path => $text:literal),+ $(,)?}) => {
        impl PresentationValue for $type {
            fn presentation_value(&self) -> MetadataValue {
                MetadataValue::text(match self { $($variant => $text),+ })
            }
        }
    };
}
observation_enum!(ObservedChangeRequestState, {ObservedChangeRequestState::Open => "open", ObservedChangeRequestState::Draft => "draft", ObservedChangeRequestState::Merged => "merged", ObservedChangeRequestState::Closed => "closed"});
observation_enum!(ObservedChecks, {ObservedChecks::Pass => "pass", ObservedChecks::Fail => "fail", ObservedChecks::Pending => "pending"});
observation_enum!(ObservedMergeability, {ObservedMergeability::Mergeable => "mergeable", ObservedMergeability::Conflicting => "conflicting"});
observation_enum!(ObservedReviewDecision, {ObservedReviewDecision::Approved => "approved", ObservedReviewDecision::ChangesRequested => "changes_requested", ObservedReviewDecision::Required => "required", ObservedReviewDecision::None => "none"});
observation_enum!(ObservedIssueState, {ObservedIssueState::Open => "open", ObservedIssueState::Closed => "closed"});

fn observe<T: PresentationValue>(
    facts: &mut Vec<(&'static str, MetadataValue)>,
    key: &'static str,
    time: &'static str,
    value: &Observation<T>,
) {
    facts.push((time, MetadataValue::text(value.observed_at.to_rfc3339())));
    if let Some(value) = &value.value {
        facts.push((key, value.presentation_value()));
    }
}

pub(super) fn project_role_attempts(catalog: &mut Catalog, role: &StandingRoleRow, convoys: &[ConvoyRow]) {
    let attempts = standing_attempts(role, convoys);
    let current = attempts.iter().rev().find(|convoy| convoy.is_live()).copied();
    let convoy_ref = |convoy: &ConvoyRow| super::convoy_entity(convoy);
    let mut facts = vec![
        (KEY_ROLE_ATTEMPTS, MetadataValue::EntityRefs(attempts.iter().filter_map(|convoy| convoy_ref(convoy)).collect())),
        (KEY_ROLE_DESIRED_STATE, MetadataValue::text("running")),
        (
            KEY_ROLE_STATE,
            MetadataValue::text(if role.state_unknown {
                "unknown"
            } else if role.hold.is_some() {
                "held"
            } else {
                "active"
            }),
        ),
    ];
    if !role.state_unknown {
        facts.push((KEY_ROLE_RESTART_COUNT, MetadataValue::Integer(i64::from(role.strikes))));
    }
    if let Some(next) = role.next_attempt {
        facts.push((KEY_ROLE_NEXT_ATTEMPT, MetadataValue::text(next.to_rfc3339())));
    }
    if let Some(convoy) = current {
        facts.push((KEY_ROLE_CURRENT_ATTEMPT, MetadataValue::EntityRefs(convoy_ref(convoy).into_iter().collect())));
        let sessions = convoy
            .vessels
            .iter()
            .filter_map(|vessel| {
                vessel.materialize.as_ref().map(|name| entity::session(&format!("{}/{}/{}", vessel.host, convoy.resource.namespace, name)))
            })
            .collect::<BTreeSet<_>>();
        facts.push((KEY_ROLE_CREW_SESSIONS, MetadataValue::EntityRefs(sessions.into_iter().collect())));
    }
    catalog.assert_entity(role_entity(role), facts, None);
}

/// The declared and built-in forges share the PM connector contract.
fn project_forge(catalog: &mut Catalog, forge: &AddressForge, kind: ForgeKind, web_url: &str) {
    let (kind, path) = match kind {
        ForgeKind::Github => ("github", "pull"),
        ForgeKind::Forgejo => ("forgejo", "pulls"),
    };
    catalog.assert_entity(
        entity::forge(forge),
        vec![
            (KEY_FORGE_KIND, MetadataValue::text(kind)),
            (KEY_FORGE_WEB_URL, MetadataValue::text(web_url.trim_end_matches('/'))),
            (KEY_FORGE_CHANGE_REQUEST_URL_TEMPLATE, MetadataValue::text(format!("{{web_url}}/{{scope}}/{path}/{{number}}"))),
            (KEY_FORGE_ISSUE_URL_TEMPLATE, MetadataValue::text("{web_url}/{scope}/issues/{number}")),
        ],
        None,
    );
}

pub(super) fn project_subjects(catalog: &mut Catalog, input: &CatalogInput<'_>, observations: &SubjectCatalogInput) {
    project_inbox(catalog, input, observations);
    let forge_specs: Vec<ForgeSpec> = observations.forges.iter().map(|forge| forge.spec.clone()).collect();
    let mut links: BTreeMap<Subject, BTreeMap<Relationship, BTreeSet<EntityRef>>> = BTreeMap::new();
    let mut projects = BTreeMap::new();
    let mut landed = BTreeSet::new();
    let mut convoy_edges = BTreeMap::new();
    for convoy in input.convoys {
        let Some(convoy_entity) = super::convoy_entity(convoy) else { continue };
        if let Some((project, _)) = super::project_fact(Some(&convoy.project_ref)) {
            projects.insert(convoy_entity.clone(), project.id);
        }
        // A future finished_at from clock skew remains in the window until
        // the local clock catches up; it must not disappear prematurely.
        let landed_convoy = convoy.phase == Some(ConvoyPhase::Landed);
        let visible = !convoy.is_ended()
            || (landed_convoy
                && convoy.finished_at.zip(observations.now).is_some_and(|(finished, now)| (now - finished).num_seconds() < 86_400));
        let mut forward: BTreeMap<Relationship, BTreeSet<EntityRef>> = BTreeMap::new();
        for entry in &convoy.subjects {
            if landed_convoy {
                landed.insert(entry.subject.clone());
            }
            if !visible {
                continue;
            }
            if let Some(subject_entity) = subject_entity(&forge_specs, &entry.subject) {
                forward.entry(entry.relationship).or_default().insert(subject_entity);
            }
            links.entry(entry.subject.clone()).or_default().entry(entry.relationship).or_default().insert(convoy_entity.clone());
        }
        if !forward.is_empty() && !catalog.facts.contains_key(&MetadataTarget::Entity(convoy_entity.clone())) {
            catalog.assert_entity(convoy_entity.clone(), super::convoy_identity_facts(convoy, &convoy_entity), None);
        }
        let facts: Vec<_> = forward
            .into_iter()
            .map(|(relationship, targets)| (forward_key(relationship), MetadataValue::EntityRefs(targets.into_iter().collect())))
            .collect();
        if !facts.is_empty() {
            catalog.assert_entity(convoy_entity.clone(), facts.clone(), None);
        }
        convoy_edges.insert(convoy_entity, facts);
    }

    for record in &observations.forges {
        if let Ok(forge) = AddressForge::new(record.spec.forge_id.clone(), record.spec.kind.into()) {
            project_forge(catalog, &forge, record.spec.kind, &record.spec.https_url);
        }
    }
    // Like declared forges, publish the default even without visible subjects.
    // Presentation-only: never declare a resource or re-key repositories.
    if super::subject_forge(&observations.forges, BUILTIN_GITHUB_SERVICE).is_none() {
        project_forge(catalog, &AddressForge::github(), ForgeKind::Github, "https://github.com");
    }

    for role in input.standing_roles {
        let role_ref = role_entity(role);
        projects.insert(role_ref.clone(), entity::project(super::role_project(role)).id);
        let attempts = standing_attempts(role, input.convoys);
        let current = attempts.iter().rev().find(|convoy| convoy.is_live()).copied();
        let mut facts = Vec::new();
        if let Some(convoy) = current {
            let target = super::convoy_entity(convoy);
            if let Some(forward) = target.as_ref().and_then(|target| convoy_edges.get(target)) {
                facts.extend(forward.clone());
            }
            for entry in &convoy.subjects {
                links.entry(entry.subject.clone()).or_default().entry(entry.relationship).or_default().insert(role_ref.clone());
            }
        }
        catalog.assert_entity(role_ref, facts, None);
    }

    let selected = flotilla_resources::select_change_requests(&observations.change_requests);
    let present_convoys: BTreeSet<_> = input.convoys.iter().filter_map(super::convoy_entity).collect();
    let mut orphaned = BTreeSet::new();
    for (subject, record) in &selected {
        if !flotilla_resources::retain_change_request(record)
            || !record.status.as_ref().is_some_and(|status| {
                matches!(status.state.value, Some(ObservedChangeRequestState::Open | ObservedChangeRequestState::Draft))
            })
        {
            continue;
        }
        let departed_producer = record.spec.subject_of.iter().any(|history| {
            history.relationship == Relationship::Produces && !present_convoys.contains(&entity::convoy(&history.origin, &history.convoy))
        });
        if departed_producer {
            orphaned.insert(subject.clone());
        }
        if orphaned.contains(subject) {
            for history in &record.spec.subject_of {
                let convoy = entity::convoy(&history.origin, &history.convoy);
                links.entry(subject.clone()).or_default().entry(history.relationship).or_default().insert(convoy);
            }
        }
    }

    // Index once per rebuild rather than scanning context/forges for each entity.
    let mut aliases = BTreeMap::new();
    for repo in &observations.references.repositories {
        aliases.entry(&repo.source).or_insert(repo.alias.as_str());
    }
    // The forge a subject's presentation links to is the declared Forge that
    // covers its service, else the built-in GitHub; the subject's own id
    // comes from the stored-spelling resolver.
    let services: BTreeSet<_> = links.keys().map(|subject| subject.source.service.as_str()).collect();
    let forges: BTreeMap<_, _> = services
        .into_iter()
        .filter_map(|service| {
            let declared = super::subject_forge(&observations.forges, service)
                .and_then(|forge| AddressForge::new(forge.spec.forge_id.clone(), forge.spec.kind.into()).ok());
            match declared.or_else(|| (service == BUILTIN_GITHUB_SERVICE).then(AddressForge::github)) {
                Some(forge) => Some((service, entity::forge(&forge))),
                None => {
                    catalog.uncovered_services.insert(service.to_owned());
                    None
                }
            }
        })
        .collect();
    let identity_facts = |subject: &Subject| {
        let label = subject.short(&observations.references);
        let kind = match subject.kind {
            SubjectKind::ChangeRequest => "change_request",
            SubjectKind::Issue => "issue",
        };
        let alias = aliases.get(&subject.source).copied().unwrap_or(&subject.source.scope);
        let mut facts = vec![
            (KEY_SUBJECT_SERVICE, MetadataValue::text(&subject.source.service)),
            (KEY_SUBJECT_SCOPE, MetadataValue::text(&subject.source.scope)),
            (KEY_SUBJECT_NUMBER, MetadataValue::text(&subject.id)),
            (KEY_SUBJECT_KIND, MetadataValue::text(kind)),
            (KEY_SUBJECT_REPOSITORY_ALIAS, MetadataValue::text(alias)),
            (KEY_DISPLAY_LABEL, MetadataValue::text(&label)),
            (KEY_DISPLAY_LABEL_MEDIUM, MetadataValue::text(&label)),
            (KEY_DISPLAY_LABEL_SHORT, MetadataValue::text(&label)),
        ];
        if let Some(forge) = forges.get(subject.source.service.as_str()) {
            facts.push((KEY_FORGE, MetadataValue::EntityRefs(vec![forge.clone()])));
        }
        let relationships = links.get(subject);
        let reverse: BTreeSet<_> = relationships.into_iter().flat_map(|links| links.values().flatten().cloned()).collect();
        let linked_projects: BTreeSet<_> = reverse.iter().filter_map(|source| projects.get(source)).collect();
        if linked_projects.len() == 1 {
            facts.push((SEGMENT_PROJECT, MetadataValue::text(*linked_projects.first().expect("one project"))));
        }
        facts.push((KEY_SUBJECT_OF, MetadataValue::EntityRefs(reverse.into_iter().collect())));
        for (relationship, sources) in relationships.into_iter().flatten() {
            facts.push((reverse_key(*relationship), MetadataValue::EntityRefs(sources.iter().cloned().collect())));
        }
        facts
    };
    for (subject, record) in &selected {
        let status = record.status.as_ref();
        if !links.contains_key(subject) {
            continue;
        }
        let Some(target) = subject_entity(&forge_specs, subject) else { continue };
        let mut facts = identity_facts(subject);
        facts.extend(change_request_facts(status, landed.contains(subject)));
        if orphaned.contains(subject) {
            facts.push((KEY_SUBJECT_ORPHANED, MetadataValue::Bool(true)));
        }
        assert_subject(catalog, target, facts);
    }
    for record in &observations.issues {
        let subject = Subject {
            kind: SubjectKind::Issue,
            source: IssueSource { service: record.spec.service.clone(), scope: record.spec.scope.clone() },
            id: record.spec.number.to_string(),
        };
        if !links.contains_key(&subject) {
            continue;
        }
        let Some(target) = subject_entity(&forge_specs, &subject) else { continue };
        let mut facts = identity_facts(&subject);
        if let Some(status) = &record.status {
            observe(&mut facts, KEY_ISSUE_TITLE, KEY_ISSUE_TITLE_OBSERVED_AT, &status.title);
            observe(&mut facts, KEY_ISSUE_STATE, KEY_ISSUE_STATE_OBSERVED_AT, &status.state);
            observe(&mut facts, KEY_ISSUE_LABELS, KEY_ISSUE_LABELS_OBSERVED_AT, &status.labels);
            observe(&mut facts, KEY_ISSUE_ASSIGNEES, KEY_ISSUE_ASSIGNEES_OBSERVED_AT, &status.assignees);
            observe(&mut facts, KEY_ISSUE_UPDATED_AT, KEY_ISSUE_UPDATED_AT_OBSERVED_AT, &status.updated_at);
        }
        assert_subject(catalog, target, facts);
    }
}

/// Observation facts shared by the subject catalog and convoy explain.
pub fn change_request_facts(status: Option<&flotilla_resources::ChangeRequestStatus>, landed: bool) -> Vec<(&'static str, MetadataValue)> {
    let mut facts = vec![(
        KEY_CHANGE_REQUEST_READINESS,
        MetadataValue::text(
            status.map(|status| super::change_request_readiness(status, landed)).unwrap_or(super::ChangeRequestReadiness::Unknown).as_str(),
        ),
    )];
    if let Some(status) = status {
        observe(&mut facts, KEY_CHANGE_REQUEST_TITLE, KEY_CHANGE_REQUEST_TITLE_OBSERVED_AT, &status.title);
        observe(&mut facts, KEY_CHANGE_REQUEST_AUTHOR, KEY_CHANGE_REQUEST_AUTHOR_OBSERVED_AT, &status.author);
        observe(&mut facts, KEY_CHANGE_REQUEST_STATE, KEY_CHANGE_REQUEST_STATE_OBSERVED_AT, &status.state);
        observe(&mut facts, KEY_CHANGE_REQUEST_CHECKS, KEY_CHANGE_REQUEST_CHECKS_OBSERVED_AT, &status.checks);
        observe(&mut facts, KEY_CHANGE_REQUEST_MERGEABLE, KEY_CHANGE_REQUEST_MERGEABLE_OBSERVED_AT, &status.mergeable);
        observe(&mut facts, KEY_CHANGE_REQUEST_HEAD_SHA, KEY_CHANGE_REQUEST_HEAD_SHA_OBSERVED_AT, &status.head_sha);
        observe(
            &mut facts,
            KEY_CHANGE_REQUEST_REVIEW_ACTIONABLE_AT_HEAD,
            KEY_CHANGE_REQUEST_REVIEW_ACTIONABLE_AT_HEAD_OBSERVED_AT,
            &status.review.actionable_at_head,
        );
        observe(&mut facts, KEY_CHANGE_REQUEST_REVIEW_DECISION, KEY_CHANGE_REQUEST_REVIEW_DECISION_OBSERVED_AT, &status.review_decision);
        observe(
            &mut facts,
            KEY_CHANGE_REQUEST_REVIEW_REQUESTED_FROM_OWNER,
            KEY_CHANGE_REQUEST_REVIEW_REQUESTED_FROM_OWNER_OBSERVED_AT,
            &status.review_requested_from_owner,
        );
    }
    facts
}

// Raw records, not an inbox policy: surfaces choose ordering and salience.
fn project_inbox(catalog: &mut Catalog, input: &CatalogInput<'_>, records: &SubjectCatalogInput) {
    use crate::keys::{KEY_CONVOY_HELD, KEY_CONVOY_HOLDS, KEY_CONVOY_LEDGER, KEY_CONVOY_PENDING_MESSAGES, KEY_CONVOY_STUCK_MESSAGES};
    use flotilla_resources::{MessagePhase, MessageReference};
    for row in input.convoys {
        let namespace = &row.resource.namespace;
        let name = &row.resource.name;
        let Some(target) = super::convoy_entity(row) else { continue };
        // Inbox facts may introduce a convoy absent from awareness. Seed its
        // canonical identity before other projections test whether it exists.
        if !catalog.facts.contains_key(&MetadataTarget::Entity(target.clone())) {
            catalog.assert_entity(target.clone(), super::convoy_identity_facts(row, &target), None);
        }
        let resource = records.convoys.iter().find(|record| record.metadata.namespace == *namespace && record.metadata.name == *name);
        let holds: Vec<_> = resource
            .and_then(|record| record.status.as_ref())
            .into_iter()
            .flat_map(|status| status.turn_delivery_holds())
            .map(|hold| serde_json::to_string(&hold).expect("hold facts"))
            .collect();
        let mut facts =
            vec![(KEY_CONVOY_HELD, MetadataValue::Bool(!holds.is_empty())), (KEY_CONVOY_HOLDS, MetadataValue::StringList(holds))];
        if let Some(artifact) = records
            .artifacts
            .iter()
            .filter(|record| {
                record.metadata.namespace == *namespace
                    && record.spec.convoy == *name
                    && record.spec.kind == "decision-ledger"
                    && record.spec.subject == *name
            })
            .max_by_key(|record| (record.spec.recorded_at.unwrap_or(record.metadata.creation_timestamp), &record.metadata.name))
        {
            facts.push((KEY_CONVOY_LEDGER, MetadataValue::text(format!("artifact/{}", artifact.metadata.name))));
        }
        let mut pending = BTreeMap::new();
        let mut stuck = BTreeMap::new();
        for record in &records.messages {
            if record.metadata.namespace != *namespace {
                continue;
            }
            let address_matches = |address: &str| address.split('/').nth(1) == Some(name.as_str()) && address.split('/').count() == 4;
            let associated = address_matches(&record.spec.sender)
                || address_matches(&record.spec.receiver)
                || record.spec.references.iter().any(|reference| {
                    matches!(reference, MessageReference::ControlRecord { resource, .. }
                    if resource.kind == "Convoy" && resource.namespace == *namespace && resource.name == *name)
                });
            if !associated {
                continue;
            }
            let status = record.status.clone().unwrap_or_default();
            let value = serde_json::json!({"name": record.metadata.name, "sender": record.spec.sender,
                "receiver": record.spec.receiver, "phase": status.phase, "reason": status.reason, "since": status.since,
                "references": record.spec.references, "expectation": record.spec.expectation})
            .to_string();
            if !status.phase.is_terminal() {
                pending.insert(record.metadata.name.clone(), value.clone());
            }
            if status.phase == MessagePhase::DeadLettered
                || (status.phase.is_waiting() && status.reason.is_some())
                || (!status.phase.is_terminal() && status.retry.is_some())
            {
                stuck.insert(record.metadata.name.clone(), value);
            }
        }
        facts.push((KEY_CONVOY_PENDING_MESSAGES, MetadataValue::StringList(pending.into_values().collect())));
        facts.push((KEY_CONVOY_STUCK_MESSAGES, MetadataValue::StringList(stuck.into_values().collect())));
        catalog.assert_entity(target, facts, None);
    }
}

#[cfg(test)]
mod tests {
    use flotilla_protocol::IssueSource;
    use flotilla_resources::{ForgeKind, ForgeSpec};

    use super::*;

    // A declared issue subject on a Forge installed under a path is stored with
    // its location encoded in the service; it must still name the Forge.
    #[test]
    fn an_encoded_installation_spelling_resolves_to_its_forge() {
        let forge = ForgeSpec::builder()
            .forge_id("lab".into())
            .kind(ForgeKind::Forgejo)
            .hosts(["git.example.com".into()].into_iter().collect())
            .https_url("https://git.example.com/forgejo".into())
            .git_ssh_host("git.example.com".into())
            .build();
        let subject = Subject {
            kind: SubjectKind::Issue,
            source: IssueSource { service: "git.example.com%2fforgejo".into(), scope: "team/cleat".into() },
            id: "7".into(),
        };
        assert_eq!(subject_entity(&[forge], &subject).expect("resolves").id, "forge:lab/repo:team%2Fcleat/issue:7");
        assert_eq!(subject_entity(&[], &subject), None, "no declared Forge, no address");
    }
}
