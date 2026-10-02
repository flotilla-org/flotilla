//! ADR 0051: replicated observations are the sole subject fact source.
use std::collections::{BTreeMap, BTreeSet};

use flotilla_protocol::{IssueRef, IssueSource, Relationship, Subject, SubjectKind};
use flotilla_resources::{
    ForgeKind, Observation, ObservedChangeRequestState, ObservedChecks, ObservedIssueState, ObservedMergeability, ObservedReviewDecision,
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
    KEY_SUBJECT_OF_PRODUCES, KEY_SUBJECT_OF_REFERENCES, KEY_SUBJECT_OF_SUPERSEDES, KEY_SUBJECT_OF_WORKS_ON, KEY_SUBJECT_PRODUCES,
    KEY_SUBJECT_REFERENCES, KEY_SUBJECT_REPOSITORY_ALIAS, KEY_SUBJECT_SCOPE, KEY_SUBJECT_SERVICE, KEY_SUBJECT_SUPERSEDES,
    KEY_SUBJECT_WORKS_ON,
};

fn subject_entity(subject: &Subject) -> EntityRef {
    match subject.kind {
        SubjectKind::Issue => entity::issue(&IssueRef { source: subject.source.clone(), id: subject.id.clone() }),
        SubjectKind::ChangeRequest => entity::change_request(&subject.source.service, &subject.source.scope, &subject.id),
    }
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
    let current = attempts.iter().rev().find(|convoy| !convoy.phase.is_terminal()).copied();
    let convoy_ref =
        |convoy: &ConvoyRow| entity::convoy(&convoy.resource.namespace, &convoy.resource.name, &entity::resource_origin(&convoy.resource));
    let mut facts = vec![
        (KEY_ROLE_ATTEMPTS, MetadataValue::EntityRefs(attempts.iter().map(|convoy| convoy_ref(convoy)).collect())),
        (KEY_ROLE_DESIRED_STATE, MetadataValue::text("running")),
        (KEY_ROLE_STATE, MetadataValue::text(if role.hold.is_some() { "held" } else { "active" })),
        (KEY_ROLE_RESTART_COUNT, MetadataValue::Integer(i64::from(role.strikes))),
    ];
    if let Some(next) = role.next_attempt {
        facts.push((KEY_ROLE_NEXT_ATTEMPT, MetadataValue::text(next.to_rfc3339())));
    }
    if let Some(convoy) = current {
        facts.push((KEY_ROLE_CURRENT_ATTEMPT, MetadataValue::EntityRefs(vec![convoy_ref(convoy)])));
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

pub(super) fn project_subjects(catalog: &mut Catalog, input: &CatalogInput<'_>, observations: &SubjectCatalogInput) {
    let mut links: BTreeMap<Subject, BTreeMap<Relationship, BTreeSet<EntityRef>>> = BTreeMap::new();
    let mut landed = BTreeSet::new();
    let mut convoy_edges = BTreeMap::new();
    for convoy in input.convoys {
        let convoy_entity = entity::convoy(&convoy.resource.namespace, &convoy.resource.name, &entity::resource_origin(&convoy.resource));
        // A future finished_at from clock skew remains in the window until
        // the local clock catches up; it must not disappear prematurely.
        let visible = !convoy.phase.is_terminal()
            || (convoy.phase == ConvoyPhase::Landed
                && convoy.finished_at.zip(observations.now).is_some_and(|(finished, now)| (now - finished).num_seconds() < 86_400));
        let mut forward: BTreeMap<Relationship, BTreeSet<EntityRef>> = BTreeMap::new();
        for entry in &convoy.subjects {
            if convoy.phase == ConvoyPhase::Landed {
                landed.insert(entry.subject.clone());
            }
            if !visible {
                continue;
            }
            forward.entry(entry.relationship).or_default().insert(subject_entity(&entry.subject));
            links.entry(entry.subject.clone()).or_default().entry(entry.relationship).or_default().insert(convoy_entity.clone());
        }
        if !forward.is_empty() && !catalog.facts.contains_key(&MetadataTarget::Entity(convoy_entity.clone())) {
            catalog.assert_entity(convoy_entity.clone(), super::convoy_identity_facts(convoy), None);
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
        let spec = &record.spec;
        let forge = entity::forge(&spec.forge_id);
        let (kind, path) = match spec.kind {
            ForgeKind::Github => ("github", "pull"),
            ForgeKind::Forgejo => ("forgejo", "pulls"),
        };
        catalog.assert_entity(
            forge,
            vec![
                (KEY_FORGE_KIND, MetadataValue::text(kind)),
                (KEY_FORGE_WEB_URL, MetadataValue::text(spec.https_url.trim_end_matches('/'))),
                (KEY_FORGE_CHANGE_REQUEST_URL_TEMPLATE, MetadataValue::text(format!("{{web_url}}/{{scope}}/{path}/{{number}}"))),
                (KEY_FORGE_ISSUE_URL_TEMPLATE, MetadataValue::text("{web_url}/{scope}/issues/{number}")),
            ],
            None,
        );
    }

    for role in input.standing_roles {
        let role_ref = role_entity(role);
        let attempts = standing_attempts(role, input.convoys);
        let current = attempts.iter().rev().find(|convoy| !convoy.phase.is_terminal()).copied();
        let mut facts = Vec::new();
        if let Some(convoy) = current {
            let target = entity::convoy(&convoy.resource.namespace, &convoy.resource.name, &entity::resource_origin(&convoy.resource));
            if let Some(forward) = convoy_edges.get(&target) {
                facts.extend(forward.clone());
            }
            for entry in &convoy.subjects {
                links.entry(entry.subject.clone()).or_default().entry(entry.relationship).or_default().insert(role_ref.clone());
            }
        }
        catalog.assert_entity(role_ref, facts, None);
    }

    // Index once per rebuild rather than scanning context/forges for each entity.
    let mut aliases = BTreeMap::new();
    for repo in &observations.references.repositories {
        aliases.entry(&repo.source).or_insert(repo.alias.as_str());
    }
    let services: BTreeSet<_> = links.keys().map(|subject| subject.source.service.as_str()).collect();
    let forges: BTreeMap<_, _> = services
        .into_iter()
        .filter_map(|service| super::subject_forge(&observations.forges, service).map(|forge| (service, forge)))
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
            facts.push((KEY_FORGE, MetadataValue::EntityRefs(vec![entity::forge(&forge.spec.forge_id)])));
        }
        let relationships = links.get(subject);
        let reverse: BTreeSet<_> = relationships.into_iter().flat_map(|links| links.values().flatten().cloned()).collect();
        facts.push((KEY_SUBJECT_OF, MetadataValue::EntityRefs(reverse.into_iter().collect())));
        for (relationship, sources) in relationships.into_iter().flatten() {
            facts.push((reverse_key(*relationship), MetadataValue::EntityRefs(sources.iter().cloned().collect())));
        }
        facts
    };
    for record in &observations.change_requests {
        let subject = Subject {
            kind: SubjectKind::ChangeRequest,
            source: IssueSource { service: record.spec.service.clone(), scope: record.spec.scope.clone() },
            id: record.spec.number.to_string(),
        };
        let status = record.status.as_ref();
        if !links.contains_key(&subject) {
            continue;
        }
        let mut facts = identity_facts(&subject);
        facts.extend(change_request_facts(status, landed.contains(&subject)));
        catalog.assert_entity(subject_entity(&subject), facts, None);
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
        let mut facts = identity_facts(&subject);
        if let Some(status) = &record.status {
            observe(&mut facts, KEY_ISSUE_TITLE, KEY_ISSUE_TITLE_OBSERVED_AT, &status.title);
            observe(&mut facts, KEY_ISSUE_STATE, KEY_ISSUE_STATE_OBSERVED_AT, &status.state);
            observe(&mut facts, KEY_ISSUE_LABELS, KEY_ISSUE_LABELS_OBSERVED_AT, &status.labels);
            observe(&mut facts, KEY_ISSUE_ASSIGNEES, KEY_ISSUE_ASSIGNEES_OBSERVED_AT, &status.assignees);
            observe(&mut facts, KEY_ISSUE_UPDATED_AT, KEY_ISSUE_UPDATED_AT_OBSERVED_AT, &status.updated_at);
        }
        catalog.assert_entity(subject_entity(&subject), facts, None);
    }
}

/// Observation facts shared by the subject catalog and convoy explain.
pub fn change_request_facts(status: Option<&flotilla_resources::ChangeRequestStatus>, landed: bool) -> Vec<(&'static str, MetadataValue)> {
    let mut facts = vec![(
        KEY_CHANGE_REQUEST_READINESS,
        MetadataValue::text(
            status.map(|status| super::change_request_readiness(status, landed).as_str()).unwrap_or("awaiting_review_response"),
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
