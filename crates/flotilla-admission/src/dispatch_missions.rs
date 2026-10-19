//! Mission normalization and membership over one complete tracker observation.
use flotilla_issue_tracker::mission_fields::{issue_ref_from_url, priority};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
};

use flotilla_protocol::{DispatchBoardRepository, DispatchScore, Issue, IssueRef, MissionAttributes, MissionFields};
use flotilla_resources::DispatchPolicy;

use flotilla_resources::convoy_accounting::active_crew_count;

/// Shared live-crew accounting for ready scores and convoy explanations.
#[derive(Default)]
pub struct MissionOccupancy {
    pub project_active: usize,
    pub missions: BTreeMap<(String, flotilla_protocol::Priority), usize>,
}
impl MissionOccupancy {
    pub fn collect<'a>(
        board: &MissionBoard,
        policy: &DispatchPolicy,
        convoys: impl IntoIterator<Item = &'a flotilla_resources::ResourceObject<flotilla_resources::Convoy>>,
    ) -> Result<Self, String> {
        let mut result = Self::default();
        for convoy in convoys {
            let count = active_crew_count(convoy.status.as_ref());
            if count == 0 {
                continue;
            }
            result.project_active += count;
            let mut served_missions = BTreeSet::new();
            for served in &convoy.spec.issues {
                let score = board.score(&issue_from_convoy(served), policy)?;
                served_missions.insert((score.mission, score.attributes.priority));
            }
            // A multi-tier convoy contributes its full crew to each tier it
            // serves: tiers run independent stride competitions. Project count
            // remains once per convoy, and duplicate tickets never multiply it.
            for key in served_missions {
                *result.missions.entry(key).or_default() += count;
            }
        }
        Ok(result)
    }
    pub fn apply(&self, score: &mut DispatchScore) {
        score.project_active_crews = self.project_active;
        score.mission_active_crews = self.missions.get(&(score.mission.clone(), score.attributes.priority)).copied().unwrap_or(0);
    }
}

pub fn issue_from_convoy(served: &flotilla_resources::ConvoyIssue) -> Issue {
    Issue::builder()
        .reference(served.reference.clone())
        .title(served.snapshot.title.clone())
        .labels(served.snapshot.labels.clone())
        .state(served.snapshot.state)
        .as_of(served.snapshot.as_of)
        .provider_name(String::new())
        .provider_display_name(String::new())
        .build()
}

fn canonical_reference(reference: &IssueRef) -> IssueRef {
    let mut result = reference.clone();
    result.source = flotilla_resources::normalize_issue_source(&result.source);
    if matches!(result.source.service.as_str(), "github" | "github.com") {
        result.source.service = "https://github.com".into();
    }
    if result.source.service == "https://github.com" {
        result.source.scope = result.source.scope.to_ascii_lowercase();
    }
    result
}

pub fn normalize_attributes(
    fields: &MissionFields,
    labels: &[String],
    defaults: &MissionAttributes,
) -> Result<(MissionAttributes, BTreeMap<String, String>), String> {
    // Label prefixes are exact lowercase; only Priority values are case-folded.
    let mut fallback = MissionFields::default();
    let mut seen = BTreeSet::new();
    for label in labels {
        let Some((name, value)) = label.split_once(':') else { continue };
        if !matches!(name, "priority" | "weight") {
            continue;
        }
        if name == "priority" && fields.priority.is_some() || name == "weight" && fields.weight.is_some() {
            continue;
        }
        if !seen.insert(name) {
            return Err(format!("duplicate mission label {name}"));
        }
        match name {
            "priority" => fallback.priority = Some(priority(value)?),
            _ => fallback.weight = Some(value.trim().parse::<f64>().map_err(|_| "invalid weight label")?.try_into()?),
        }
    }
    let sources = [
        ("priority", fields.priority.is_some(), fallback.priority.is_some()),
        ("weight", fields.weight.is_some(), fallback.weight.is_some()),
    ]
    .into_iter()
    .map(|(name, field, label)| {
        (
            name.into(),
            if field {
                "issue_field"
            } else if label {
                "label"
            } else {
                "charter"
            }
            .into(),
        )
    })
    .collect();
    Ok((
        MissionAttributes {
            priority: fields.priority.or(fallback.priority).unwrap_or(defaults.priority),
            weight: fields.weight.or(fallback.weight).unwrap_or(defaults.weight),
        },
        sources,
    ))
}

/// An immutable cache observation. Revision changes only when issue facts do.
pub type MissionSourceSnapshot = (u64, Arc<DispatchBoardRepository>);

/// Maintains project graphs from source-cache revision deltas. Metadata-only
/// refreshes retain topology and descendant memoization, including stale facts.
#[derive(Default)]
pub struct MissionBoardIndex {
    sources: BTreeMap<flotilla_protocol::IssueSource, MissionSourceSnapshot>,
    scopes: BTreeMap<String, MissionScope>,
    rebuilds: usize,
    updated_issues: usize,
}

struct MissionScope {
    sources: BTreeSet<flotilla_protocol::IssueSource>,
    inputs: BTreeMap<flotilla_protocol::IssueSource, MissionSourceSnapshot>,
    board: Result<Arc<MissionBoard>, String>,
}

impl MissionBoardIndex {
    pub fn rebuilds(&self) -> usize {
        self.rebuilds
    }
    pub fn updated_issues(&self) -> usize {
        self.updated_issues
    }
    pub fn apply_source(&mut self, source: flotilla_protocol::IssueSource, snapshot: Option<MissionSourceSnapshot>) {
        let unchanged = match (self.sources.get(&source), &snapshot) {
            (Some(previous), Some(next)) => previous.0 == next.0,
            (None, None) => true,
            _ => false,
        };
        if unchanged {
            return;
        }
        match snapshot {
            Some(snapshot) => {
                self.sources.insert(source, snapshot);
            }
            None => {
                self.sources.remove(&source);
            }
        }
    }

    pub fn scope(&mut self, name: &str, sources: BTreeSet<flotilla_protocol::IssueSource>) -> Result<Arc<MissionBoard>, String> {
        let inputs = sources
            .iter()
            .map(|source| {
                self.sources
                    .get(source)
                    .cloned()
                    .map(|snapshot| (source.clone(), snapshot))
                    .ok_or_else(|| format!("mission source {} unavailable", source.scope))
            })
            .collect::<Result<BTreeMap<_, _>, _>>()?;
        if let Some(previous) = self.scopes.get(name) {
            if previous.sources == sources && previous.inputs.iter().all(|(source, snapshot)| inputs[source].0 == snapshot.0) {
                return previous.board.clone();
            }
        }
        let board = if let Some(previous) = self.scopes.get(name).filter(|previous| previous.board.is_ok()) {
            let mut board = previous.board.as_ref().expect("successful graph").as_ref().clone();
            let changed_sources = previous
                .sources
                .union(&sources)
                .filter(|source| previous.inputs.get(*source).map(|snapshot| snapshot.0) != inputs.get(*source).map(|snapshot| snapshot.0))
                .cloned()
                .collect::<Vec<_>>();
            let mut result = Ok(());
            for source in changed_sources {
                match board.update_source(
                    &source,
                    previous.inputs.get(&source).map(|snapshot| snapshot.1.as_ref()),
                    inputs.get(&source).map(|snapshot| snapshot.1.as_ref()),
                ) {
                    Ok(count) => self.updated_issues += count,
                    Err(error) => {
                        result = Err(error);
                        break;
                    }
                }
            }
            result.and_then(|()| board.validate_parents()).map(|()| Arc::new(board))
        } else {
            self.rebuilds += 1;
            let boards = inputs.values().map(|(_, board)| (**board).clone()).collect::<Vec<_>>();
            MissionBoard::new(&boards).map(Arc::new)
        };
        self.scopes.insert(name.into(), MissionScope { sources, inputs, board: board.clone() });
        board
    }

    pub fn retain(&mut self, scopes: &BTreeSet<String>, sources: &BTreeSet<flotilla_protocol::IssueSource>) {
        self.scopes.retain(|scope, _| scopes.contains(scope));
        let removed = self.sources.keys().filter(|source| !sources.contains(*source)).cloned().collect::<Vec<_>>();
        for source in removed {
            self.apply_source(source, None);
        }
    }
}

type CachedScore = (Vec<String>, DispatchPolicy, Result<DispatchScore, String>);

pub struct MissionBoard {
    parents: BTreeMap<IssueRef, IssueRef>,
    maps: BTreeSet<IssueRef>,
    labels: BTreeMap<IssueRef, Vec<String>>,
    fields: BTreeMap<IssueRef, MissionFields>,
    edges: BTreeMap<IssueRef, BTreeSet<IssueRef>>,
    unblock_counts: Mutex<BTreeMap<IssueRef, usize>>,
    scores: Mutex<BTreeMap<IssueRef, CachedScore>>,
}

impl Clone for MissionBoard {
    fn clone(&self) -> Self {
        Self {
            parents: self.parents.clone(),
            maps: self.maps.clone(),
            labels: self.labels.clone(),
            fields: self.fields.clone(),
            edges: self.edges.clone(),
            unblock_counts: Mutex::new(self.unblock_counts.lock().expect("unblock count cache").clone()),
            scores: Mutex::new(self.scores.lock().expect("mission score cache").clone()),
        }
    }
}

impl MissionBoard {
    pub fn new(boards: &[DispatchBoardRepository]) -> Result<Self, String> {
        let mut result = Self {
            parents: BTreeMap::new(),
            maps: BTreeSet::new(),
            labels: BTreeMap::new(),
            fields: BTreeMap::new(),
            edges: BTreeMap::new(),
            unblock_counts: Mutex::new(BTreeMap::new()),
            scores: Mutex::new(BTreeMap::new()),
        };
        let mut edges: BTreeMap<IssueRef, BTreeSet<IssueRef>> = BTreeMap::new();
        for board in boards {
            for issue in &board.issues {
                let reference = canonical_reference(&IssueRef { source: board.source.clone(), id: issue.id.clone() });
                if let Some(parent) = &issue.parent {
                    result.parents.insert(reference.clone(), canonical_reference(parent));
                }
                result.fields.insert(reference.clone(), issue.mission_fields.clone());
                if issue.issue_type.as_ref().is_some_and(|kind| kind.eq_ignore_ascii_case("map") || kind.eq_ignore_ascii_case("mission"))
                    || issue.labels.iter().any(|label| label.rsplit(':').next().is_some_and(|name| name.eq_ignore_ascii_case("map")))
                {
                    result.maps.insert(reference.clone());
                }
                result.labels.insert(reference.clone(), issue.labels.clone());
                for blocker in &issue.blocked_by {
                    edges.entry(canonical_reference(&issue_ref_from_url(&blocker.url)?)).or_default().insert(reference.clone());
                }
            }
        }
        result.edges = edges;
        result.validate_parents()?;
        Ok(result)
    }

    fn validate_parents(&self) -> Result<(), String> {
        // Reject malformed parent cycles rather than choosing an arbitrary map.
        for start in self.parents.keys() {
            let mut seen = BTreeSet::from([start]);
            let mut next = start;
            while let Some(parent) = self.parents.get(next) {
                if !seen.insert(parent) {
                    return Err("native mission parent cycle".into());
                }
                next = parent;
            }
        }
        Ok(())
    }

    fn update_source(
        &mut self,
        source: &flotilla_protocol::IssueSource,
        previous: Option<&DispatchBoardRepository>,
        next: Option<&DispatchBoardRepository>,
    ) -> Result<usize, String> {
        let old = previous.into_iter().flat_map(|board| &board.issues).map(|issue| (&issue.id, issue)).collect::<BTreeMap<_, _>>();
        let new = next.into_iter().flat_map(|board| &board.issues).map(|issue| (&issue.id, issue)).collect::<BTreeMap<_, _>>();
        let ids = old.keys().chain(new.keys()).copied().collect::<BTreeSet<_>>();
        let mut changed = 0;
        let mut changed_edges = BTreeSet::new();
        for id in ids {
            if old.get(id) == new.get(id) {
                continue;
            }
            changed += 1;
            let reference = canonical_reference(&IssueRef { source: source.clone(), id: id.clone() });
            self.parents.remove(&reference);
            self.maps.remove(&reference);
            self.labels.remove(&reference);
            self.fields.remove(&reference);
            if let Some(issue) = old.get(id) {
                for blocker in &issue.blocked_by {
                    let blocker = canonical_reference(&issue_ref_from_url(&blocker.url)?);
                    if self.edges.get_mut(&blocker).is_some_and(|edges| {
                        edges.remove(&reference);
                        edges.is_empty()
                    }) {
                        self.edges.remove(&blocker);
                    }
                    changed_edges.insert(blocker);
                }
            }
            if let Some(issue) = new.get(id) {
                if let Some(parent) = &issue.parent {
                    self.parents.insert(reference.clone(), canonical_reference(parent));
                }
                if issue.issue_type.as_ref().is_some_and(|kind| kind.eq_ignore_ascii_case("map") || kind.eq_ignore_ascii_case("mission"))
                    || issue.labels.iter().any(|label| label.rsplit(':').next().is_some_and(|name| name.eq_ignore_ascii_case("map")))
                {
                    self.maps.insert(reference.clone());
                }
                self.labels.insert(reference.clone(), issue.labels.clone());
                self.fields.insert(reference.clone(), issue.mission_fields.clone());
                for blocker in &issue.blocked_by {
                    let blocker = canonical_reference(&issue_ref_from_url(&blocker.url)?);
                    self.edges.entry(blocker.clone()).or_default().insert(reference.clone());
                    changed_edges.insert(blocker);
                }
            }
        }
        // Only cached ancestors of changed dependency edges can have a new
        // descendant count. Traverse the union (old removals are starting nodes).
        let mut affected = changed_edges;
        loop {
            let ancestors = self
                .edges
                .iter()
                .filter(|(_, children)| children.iter().any(|child| affected.contains(child)))
                .map(|(parent, _)| parent.clone())
                .collect::<Vec<_>>();
            let before = affected.len();
            affected.extend(ancestors);
            if affected.len() == before {
                break;
            }
        }
        self.unblock_counts.get_mut().expect("unblock count cache").retain(|reference, _| !affected.contains(reference));
        if changed > 0 {
            self.scores.get_mut().expect("mission score cache").clear();
        }
        Ok(changed)
    }

    fn unblock_count(&self, reference: &IssueRef) -> usize {
        let mut counts = self.unblock_counts.lock().expect("unblock count cache");
        if let Some(count) = counts.get(reference) {
            return *count;
        }
        let mut visited = BTreeSet::from([reference.clone()]);
        let mut pending = vec![reference.clone()];
        while let Some(next) = pending.pop() {
            for child in self.edges.get(&next).into_iter().flatten() {
                if visited.insert(child.clone()) {
                    pending.push(child.clone());
                }
            }
        }
        let count = visited.len().saturating_sub(1);
        counts.insert(reference.clone(), count);
        count
    }

    pub fn score(&self, issue: &Issue, policy: &DispatchPolicy) -> Result<DispatchScore, String> {
        let reference = canonical_reference(&issue.reference);
        let mut scores = self.scores.lock().expect("mission score cache");
        if let Some((labels, previous_policy, score)) = scores.get(&reference) {
            if labels == &issue.labels && previous_policy == policy {
                return score.clone();
            }
        }
        let score = self.score_uncached(issue, policy);
        scores.insert(reference, (issue.labels.clone(), policy.clone(), score.clone()));
        score
    }

    fn score_uncached(&self, issue: &Issue, policy: &DispatchPolicy) -> Result<DispatchScore, String> {
        let reference = canonical_reference(&issue.reference);
        let mut parent = self.parents.get(&reference);
        let mut map = None;
        while let Some(next) = parent {
            if self.maps.contains(next) {
                map = Some(next);
                break;
            }
            parent = self.parents.get(next);
        }
        let lane = policy.lanes.iter().find(|lane| lane.labels.iter().all(|label| issue.labels.contains(label)));
        let declared = if let Some(map) = map {
            policy.missions.iter().find(|m| m.issue.as_ref().map(canonical_reference).as_ref() == Some(map))
        } else {
            policy.missions.iter().find(|m| &m.name == lane.map_or(&policy.routine_lane, |lane| &lane.mission))
        };
        let tracking = map.cloned().or_else(|| declared.and_then(|m| m.issue.as_ref().map(canonical_reference)));
        if tracking.as_ref().is_some_and(|reference| !self.labels.contains_key(reference)) {
            return Err("mission tracking issue is outside the observed project sources".into());
        }
        let defaults = declared.map(|m| m.attributes.clone()).unwrap_or_default();
        let (mut attributes, mut attribute_sources) = normalize_attributes(
            &tracking.as_ref().and_then(|i| self.fields.get(i)).cloned().unwrap_or_default(),
            tracking.as_ref().and_then(|i| self.labels.get(i)).map(Vec::as_slice).unwrap_or_default(),
            &defaults,
        )?;
        // A ticket's own priority overrides its mission, in either direction.
        let ticket_fields = self.fields.get(&reference).cloned().unwrap_or_default();
        let (ticket, sources) = normalize_attributes(
            &MissionFields { priority: ticket_fields.priority, weight: None },
            &issue.labels.iter().filter(|label| label.starts_with("priority:")).cloned().collect::<Vec<_>>(),
            &attributes,
        )?;
        if sources["priority"] != "charter" {
            attributes.priority = ticket.priority;
            attribute_sources.insert("priority".into(), format!("ticket_{}", sources["priority"]));
        }
        Ok(DispatchScore::builder()
            .mission(declared.map(|m| m.name.clone()).unwrap_or_else(|| {
                map.map_or_else(|| policy.routine_lane.clone(), |i| format!("{}/{}#{}", i.source.service, i.source.scope, i.id))
            }))
            .maybe_mission_issue(tracking)
            .attributes(attributes)
            .membership(
                if map.is_some() {
                    "map_sub_issue"
                } else if lane.is_some() {
                    "lane_rule"
                } else {
                    "routine"
                }
                .into(),
            )
            .attribute_sources(attribute_sources)
            .unblock_count(self.unblock_count(&reference))
            .conflict_penalty(0)
            .project_share(policy.project_share)
            .project_active_crews(0)
            .mission_active_crews(0)
            .build())
    }
}

#[cfg(test)]
mod tests {
    use flotilla_issue_tracker::mission_fields::parse_mission_fields;
    use flotilla_protocol::Priority;
    use std::cmp::Ordering;

    use flotilla_protocol::{
        compare_dispatch_rows, DispatchBoardDependency, DispatchBoardIssue, DispatchQueueRow, IssueSource, IssueState,
    };
    use flotilla_resources::{DispatchLane, DispatchMission};

    use super::*;
    fn row(id: &str) -> DispatchQueueRow {
        DispatchQueueRow::builder()
            .namespace("ns".into())
            .project(id.into())
            .issue(IssueRef {
                source: flotilla_protocol::IssueSource { service: "https://github.com".into(), scope: "org/repo".into() },
                id: id.into(),
            })
            .title(id.into())
            .ready_observed_at("2026-10-06T00:00:00Z".parse().expect("time"))
            .age_seconds(0)
            .attention(false)
            .provenance("test".into())
            .score(
                DispatchScore::builder()
                    .mission("routine".into())
                    .attributes(MissionAttributes::default())
                    .membership("routine".into())
                    .attribute_sources(Default::default())
                    .unblock_count(0)
                    .conflict_penalty(0)
                    .project_share(1)
                    .project_active_crews(0)
                    .mission_active_crews(0)
                    .build(),
            )
            .build()
    }

    // A convoy with two live roles serves both tier competitions with two crews,
    // while its Project has two crews total. Duplicate tickets in a tier add none.
    #[tokio::test]
    async fn mixed_tier_convoy_counts_once_in_each_served_tier() {
        use flotilla_resources::{
            Convoy, ConvoyIssue, ConvoyPhase, ConvoySpec, ConvoyStatus, CrewWorkPhase, CrewWorkState, InputMeta, IssueSnapshot,
        };
        use flotilla_store::ResourceBackend;
        use flotilla_store_memory::InMemoryBackend;
        let backend = ResourceBackend::new(InMemoryBackend::default());
        let convoys = backend.using::<Convoy>("ns");
        let board = board(vec![board_issue("1", None, &[], &[]), board_issue("2", None, &[], &[])]);
        let mut urgent = issue("1");
        urgent.labels = vec!["priority:urgent".into()];
        let medium = issue("2");
        let served = [&urgent, &medium, &urgent]
            .into_iter()
            .map(|ticket| ConvoyIssue {
                reference: ticket.reference.clone(),
                repository_ref: None,
                snapshot: IssueSnapshot {
                    title: ticket.title.clone(),
                    body: None,
                    state: ticket.state,
                    labels: ticket.labels.clone(),
                    as_of: ticket.as_of,
                },
            })
            .collect();
        let convoy = convoys
            .create(
                &InputMeta::builder().name("mixed".into()).build(),
                &ConvoySpec::builder().workflow_ref("workflow".into()).project_ref("p".into()).issues(served).build(),
            )
            .await
            .expect("convoy");
        convoys
            .update_status(
                &convoy.metadata.name,
                &convoy.metadata.resource_version,
                &ConvoyStatus {
                    crew_work: BTreeMap::from([(
                        "work".into(),
                        ["coder", "reviewer"]
                            .into_iter()
                            .map(|role| (role.into(), CrewWorkState::builder().phase(CrewWorkPhase::Working).build()))
                            .collect(),
                    )]),
                    ..Default::default()
                },
            )
            .await
            .expect("crew state");
        let convoy = convoys.get("mixed").await.expect("live convoy");
        let policy = DispatchPolicy::builder().build();
        let occupancy = MissionOccupancy::collect(&board, &policy, [&convoy]).expect("live counts");
        assert_eq!(occupancy.project_active, 2);
        assert_eq!(occupancy.missions.len(), 2);
        for ticket in [&urgent, &medium] {
            let mut score = board.score(ticket, &policy).expect("tier score");
            occupancy.apply(&mut score);
            assert_eq!(score.mission_active_crews, 2);
            assert_eq!(score.project_active_crews, 2);
        }
        let mut terminal = convoy.clone();
        terminal.status.as_mut().expect("status").phase = ConvoyPhase::Landed;
        let released = MissionOccupancy::collect(&board, &policy, [&terminal]).expect("settled counts");
        assert_eq!(released.project_active, 0);
        assert!(released.missions.is_empty());
    }

    // #3313: strict tiers dominate share and every within-mission term.
    // Generator covers all four tiers and nonzero live occupancy.
    #[hegel::test]
    fn tiers_dominate_shares(tc: hegel::TestCase) {
        use hegel::generators as gs;
        let tiers = [Priority::Urgent, Priority::High, Priority::Medium, Priority::Low];
        let tier = tc.draw(gs::integers::<usize>().min_value(0).max_value(2));
        let live = tc.draw(gs::integers::<usize>().min_value(1).max_value(10000));
        let mut better = row("z");
        let mut worse = row("a");
        let score = better.score.as_mut().expect("score");
        score.attributes.priority = tiers[tier];
        score.attributes.weight = 0.0.try_into().expect("zero");
        score.project_active_crews = live;
        score.mission_active_crews = live;
        worse.score.as_mut().expect("score").attributes.priority = tiers[tier + 1];
        assert_eq!(compare_dispatch_rows(&better, &worse), Ordering::Less);
        assert_eq!(compare_dispatch_rows(&worse, &better), Ordering::Greater);
    }

    // Project share dominates mission share, then the existing within-mission
    // terms dominate each subsequent term. Generated counts cross zero.
    #[hegel::test]
    fn project_and_within_mission_order(tc: hegel::TestCase) {
        use hegel::generators as gs;
        let axis = tc.draw(gs::integers::<usize>().min_value(0).max_value(3));
        let count = tc.draw(gs::integers::<usize>().min_value(0).max_value(1000));
        let mut better = row("z");
        let mut worse = row("a");
        better.score.as_mut().expect("score").conflict_penalty = 100;
        if axis < 3 {
            worse.ready_observed_at -= chrono::Duration::days(1);
        }
        if axis < 2 {
            worse.score.as_mut().expect("score").unblock_count = 100;
        }
        match axis {
            0 => {
                better.score.as_mut().expect("score").project_active_crews = count;
                worse.score.as_mut().expect("score").project_active_crews = count + 1;
                better.score.as_mut().expect("score").mission_active_crews = 10000;
            }
            1 => better.score.as_mut().expect("score").unblock_count = 101,
            2 => better.ready_observed_at -= chrono::Duration::days(2),
            _ => worse.score.as_mut().expect("score").conflict_penalty = 101,
        }
        assert_eq!(compare_dispatch_rows(&better, &worse), Ordering::Less);
        assert_eq!(compare_dispatch_rows(&worse, &better), Ordering::Greater);
        assert_eq!(compare_dispatch_rows(&better, &better), Ordering::Equal);
    }

    // #3313: each next slot uses current live occupancy. Ready missions with
    // weights 2:1 split three crews 2:1, while absent missions reserve nothing.
    #[test]
    fn weighted_slots_are_work_conserving() {
        let mut rows = vec![row("a"), row("b")];
        for row in &mut rows {
            row.project = "same".into();
            row.score.as_mut().expect("score").mission = row.issue.id.clone();
            row.score.as_mut().expect("score").attributes.priority = Priority::High;
        }
        rows[0].score.as_mut().expect("score").attributes.weight = 2.0.try_into().expect("weight");
        for _ in 0..3 {
            rows.sort_by(compare_dispatch_rows);
            rows[0].score.as_mut().expect("score").mission_active_crews += 1;
            for row in &mut rows {
                row.score.as_mut().expect("score").project_active_crews += 1;
            }
        }
        rows.sort_by_key(|row| row.issue.id.clone());
        assert_eq!(rows.iter().map(|row| row.score.as_ref().expect("score").mission_active_crews).collect::<Vec<_>>(), vec![2, 1]);
        for candidate in &rows {
            let mut lone = [candidate.clone()];
            let before = lone[0].score.as_ref().expect("score").mission_active_crews;
            for _ in 0..3 {
                lone.sort_by(compare_dispatch_rows);
                lone[0].score.as_mut().expect("score").mission_active_crews += 1;
            }
            assert_eq!(lone[0].score.as_ref().expect("score").mission_active_crews, before + 3);
        }
        let mut zero = row("zero");
        zero.score.as_mut().expect("score").attributes.weight = 0.0.try_into().expect("weight");
        let mut positive = row("positive");
        positive.score.as_mut().expect("score").project_active_crews = 10000;
        assert_eq!(compare_dispatch_rows(&zero, &positive), Ordering::Greater);
        // A higher tier still wins with zero weight; empty tiers have no rows.
        positive.score.as_mut().expect("score").attributes.priority = Priority::Low;
        assert_eq!(compare_dispatch_rows(&zero, &positive), Ordering::Less);
        let mut medium_only = [row("medium")];
        medium_only.sort_by(compare_dispatch_rows);
        assert_eq!(medium_only[0].score.as_ref().expect("score").attributes.priority, Priority::Medium);
    }

    // #3313: field > label > charter independently per attribute, including
    // null fields and stale invalid labels hidden by present fields.
    #[hegel::test]
    fn sources_normalize_per_attribute(tc: hegel::TestCase) {
        use hegel::generators as gs;
        let ps = tc.draw(gs::integers::<usize>().min_value(0).max_value(2));
        let ws = tc.draw(gs::integers::<usize>().min_value(0).max_value(2));
        let weight = f64::from(tc.draw(gs::integers::<u32>().min_value(0).max_value(1000))) / 4.0;
        let fields = parse_mission_fields(&[
            serde_json::json!({"issue_field_name":"Priority","value":if ps == 0 {Some(1)} else {None},"single_select_option":{"name":" Urgent "}}),
            serde_json::json!({"issue_field_name":"Weight","value":if ws == 0 {Some(weight)} else {None}}),
        ]).expect("field shape");
        let labels = [(ps, "priority:High".into()), (ws, format!("weight:{weight}"))]
            .into_iter()
            .filter_map(|(source, label)| (source < 2).then_some(label))
            .collect::<Vec<_>>();
        let (attrs, sources) = normalize_attributes(&fields, &labels, &MissionAttributes::default()).expect("normalized");
        assert_eq!(attrs.priority, [Priority::Urgent, Priority::High, Priority::Medium][ps]);
        assert_eq!(f64::from(attrs.weight), if ws < 2 { weight } else { 1.0 });
        assert_eq!(sources["priority"], ["issue_field", "label", "charter"][ps]);
        assert_eq!(sources["weight"], ["issue_field", "label", "charter"][ws]);
        assert!(normalize_attributes(
            &MissionFields { priority: Some(Priority::High), weight: Some(1.0.try_into().expect("weight")) },
            &["priority:bad".into(), "weight:bad".into()],
            &MissionAttributes::default()
        )
        .is_ok());
    }

    // #3313: a Mission issue type defines ancestry just like Map, and a
    // ticket's own field overrides its label and its Low mission tier.
    #[test]
    fn ticket_priority_overrides_mission() {
        let mut mission = board_issue("mission", None, &["priority:low"], &[]);
        mission.issue_type = Some("Mission".into());
        let mut ticket = board_issue("1", Some("mission"), &["priority:high"], &[]);
        ticket.mission_fields.priority = Some(Priority::Urgent);
        let mission_board = board(vec![mission, ticket]);
        let mut candidate = issue("1");
        candidate.labels = vec!["priority:high".into()];
        let score = mission_board.score(&candidate, &DispatchPolicy::builder().build()).expect("score");
        assert_eq!(score.attributes.priority, Priority::Urgent);
        assert_eq!(score.attribute_sources["priority"], "ticket_issue_field");
        assert_eq!(score.mission_issue, Some(reference("mission")));
        let mut boosted = row("boosted");
        boosted.score = Some(score);
        let ordinary = row("ordinary");
        assert_eq!(compare_dispatch_rows(&boosted, &ordinary), Ordering::Less);
        let board = board(vec![board_issue("mission", None, &["map", "priority:low"], &[]), board_issue("2", Some("mission"), &[], &[])]);
        let mut candidate = issue("2");
        candidate.labels = vec!["priority:urgent".into()];
        let score = board.score(&candidate, &DispatchPolicy::builder().build()).expect("label override");
        assert_eq!(score.attributes.priority, Priority::Urgent);
        assert_eq!(score.attribute_sources["priority"], "ticket_label");
    }

    fn reference(id: &str) -> IssueRef {
        IssueRef { source: IssueSource { service: "https://github.com".into(), scope: "org/repo".into() }, id: id.into() }
    }
    fn issue(id: &str) -> Issue {
        Issue::builder()
            .reference(reference(id))
            .title(id.into())
            .state(IssueState::Open)
            .labels(vec!["bug".into()])
            .as_of("2026-10-06T00:00:00Z".parse().expect("time"))
            .provider_name(String::new())
            .provider_display_name(String::new())
            .build()
    }
    fn board_issue(id: &str, parent: Option<&str>, labels: &[&str], blockers: &[&str]) -> DispatchBoardIssue {
        DispatchBoardIssue::builder()
            .id(id.into())
            .title(id.into())
            .state(IssueState::Open)
            .url(format!("https://github.com/org/repo/issues/{id}"))
            .updated_at(String::new())
            .maybe_parent(parent.map(reference))
            .labels(labels.iter().map(|s| (*s).into()).collect())
            .blocked_by(
                blockers
                    .iter()
                    .map(|id| DispatchBoardDependency { url: format!("https://github.com/org/repo/issues/{id}"), state: IssueState::Open })
                    .collect(),
            )
            .pull_requests(vec![])
            .build()
    }
    fn board(issues: Vec<DispatchBoardIssue>) -> MissionBoard {
        MissionBoard::new(&[DispatchBoardRepository {
            source: reference("1").source,
            issues,
            pull_requests: vec![],
            observed_at: chrono::Utc::now(),
            age_seconds: 0,
            refresh_error: None,
        }])
        .expect("mission board")
    }

    // #2783: native map ancestry wins over ordered lane rules, whose first match
    // wins over routine. DAG descendants count unique tickets, not diamond paths.
    #[test]
    fn membership_and_descendants_follow_native_graph() {
        let board = board(vec![
            board_issue("map", None, &["wayfinder:map", "weight:9"], &[]),
            board_issue("1", Some("map"), &[], &[]),
            board_issue("2", Some("1"), &[], &["1"]),
            board_issue("3", None, &[], &["1"]),
            board_issue("4", None, &[], &["2", "3"]),
        ]);
        let policy = DispatchPolicy::builder()
            .missions(vec![
                DispatchMission::builder().name("stability".into()).build(),
                DispatchMission::builder().name("other".into()).build(),
            ])
            .lanes(vec![
                DispatchLane { mission: "stability".into(), labels: BTreeSet::from(["bug".into()]) },
                DispatchLane { mission: "other".into(), labels: BTreeSet::new() },
            ])
            .build();
        let score = board.score(&issue("1"), &policy).expect("map member");
        assert_eq!(score.mission_issue, Some(reference("map")));
        assert_eq!(score.membership, "map_sub_issue");
        assert_eq!(f64::from(score.attributes.weight), 9.0);
        assert_eq!(score.unblock_count, 3);
        assert_eq!(board.score(&issue("2"), &policy).expect("nested").mission_issue, Some(reference("map")));
        assert_eq!(board.score(&issue("3"), &policy).expect("lane").mission, "stability");
        let mut unmatched = issue("4");
        unmatched.labels.clear();
        assert_eq!(board.score(&unmatched, &policy).expect("catch-all lane").mission, "other");
        let empty = DispatchPolicy::builder().build();
        assert_eq!(board.score(&issue("3"), &empty).expect("routine").membership, "routine");
        assert_eq!(board.score(&issue("3"), &empty).expect("routine").mission, "routine");
    }

    fn observation(source: IssueSource, issues: Vec<DispatchBoardIssue>) -> DispatchBoardRepository {
        DispatchBoardRepository {
            source,
            issues,
            pull_requests: vec![],
            observed_at: chrono::Utc::now(),
            age_seconds: 0,
            refresh_error: None,
        }
    }

    // #2859: delta-maintained membership and descendant counts agree with a
    // fresh graph after every add/update/remove, while unrelated scopes do no work.
    #[hegel::test]
    fn mission_deltas_match_full_graph_without_unrelated_rebuilds(tc: hegel::TestCase) {
        use hegel::generators as gs;
        // Both sources, duplicate deliveries, empty boards, changed attributes,
        // parent moves and dependency edge removal appear in short sequences.
        let steps = tc.draw(gs::integers::<usize>().min_value(1).max_value(12));
        let operations = (0..steps)
            .map(|_| {
                (tc.draw(gs::integers::<usize>().min_value(0).max_value(1)), tc.draw(gs::integers::<usize>().min_value(0).max_value(5)))
            })
            .collect::<Vec<_>>();
        let sources = [reference("1").source, IssueSource { service: "https://github.com".into(), scope: "org/other".into() }];
        let mut observations = sources.iter().map(|source| observation(source.clone(), vec![])).collect::<Vec<_>>();
        let mut revisions = [1, 1];
        let mut index = MissionBoardIndex::default();
        for id in 0..2 {
            index.apply_source(sources[id].clone(), Some((revisions[id], Arc::new(observations[id].clone()))));
        }
        let policy = DispatchPolicy::builder().build();
        let scope_sources = |id: usize| BTreeSet::from([sources[id].clone()]);
        let mut graphs = (0..2).map(|id| index.scope(&id.to_string(), scope_sources(id)).expect("empty graph")).collect::<Vec<_>>();
        let combined_sources = sources.iter().cloned().collect::<BTreeSet<_>>();
        index.scope("combined", combined_sources.clone()).expect("empty combined graph");
        for (id, operation) in operations {
            let previous = observations[id].issues.clone();
            match operation {
                0 => observations[id].issues.clear(),
                1 => {
                    observations[id].issues =
                        vec![board_issue("map", None, &["map", "weight:4"], &[]), board_issue("1", Some("map"), &[], &[])]
                }
                2 => {
                    observations[id].issues =
                        vec![board_issue("1", None, &[], &[]), board_issue("2", None, &[], &["1"]), board_issue("3", None, &[], &["2"])]
                }
                3 => observations[id].issues = vec![board_issue("1", None, &[], &[]), board_issue("2", None, &[], &["1"])],
                4 => {
                    observations[id].refresh_error = Some("tracker offline".into());
                    observations[id].age_seconds += 60;
                }
                _ => {}
            }
            // Fixture references point to org/repo; rewrite native relations so
            // equal IDs on the two sources exercise identity isolation.
            for row in &mut observations[id].issues {
                if let Some(parent) = &mut row.parent {
                    parent.source = sources[id].clone();
                }
                for blocker in &mut row.blocked_by {
                    blocker.url = blocker.url.replace("org/repo", &sources[id].scope);
                }
            }
            if previous != observations[id].issues {
                revisions[id] += 1;
            }
            let work = index.updated_issues;
            index.apply_source(sources[id].clone(), Some((revisions[id], Arc::new(observations[id].clone()))));
            let next = index.scope(&id.to_string(), scope_sources(id)).expect("delta graph");
            let unrelated = index.scope(&(1 - id).to_string(), scope_sources(1 - id)).expect("unrelated graph");
            assert!(Arc::ptr_eq(&unrelated, &graphs[1 - id]));
            if previous == observations[id].issues {
                assert!(Arc::ptr_eq(&next, &graphs[id]));
                assert_eq!(index.updated_issues, work);
            }
            let combined = index.scope("combined", combined_sources.clone()).expect("combined delta graph");
            let combined_oracle = MissionBoard::new(&observations).expect("combined oracle");
            for source in &sources {
                for ticket in ["1", "2", "3"] {
                    let mut candidate = issue(ticket);
                    candidate.reference.source = source.clone();
                    assert_eq!(combined.score(&candidate, &policy), combined_oracle.score(&candidate, &policy));
                }
            }
            let oracle = MissionBoard::new(&[observations[id].clone()]).expect("oracle graph");
            for ticket in ["1", "2", "3"] {
                let mut candidate = issue(ticket);
                candidate.reference.source = sources[id].clone();
                assert_eq!(next.score(&candidate, &policy), oracle.score(&candidate, &policy));
            }
            assert_eq!(index.rebuilds, 3, "deltas must update retained graphs");
            graphs[id] = next;
        }
    }

    // Source removal and malformed ancestry invalidate only affected scopes;
    // a later valid observation recovers without inheriting a cached error.
    #[test]
    fn mission_source_removal_and_cycle_recovery_are_scoped() {
        let source = reference("1").source;
        let mut index = MissionBoardIndex::default();
        index.apply_source(source.clone(), Some((1, Arc::new(observation(source.clone(), vec![])))));
        index.scope("project", BTreeSet::from([source.clone()])).expect("initial");
        index.apply_source(source.clone(), None);
        assert!(index.scope("project", BTreeSet::from([source.clone()])).is_err());
        index.apply_source(
            source.clone(),
            Some((
                2,
                Arc::new(observation(source.clone(), vec![board_issue("1", Some("2"), &[], &[]), board_issue("2", Some("1"), &[], &[])])),
            )),
        );
        assert!(index.scope("project", BTreeSet::from([source.clone()])).is_err());
        index.apply_source(source.clone(), Some((3, Arc::new(observation(source.clone(), vec![board_issue("1", None, &[], &[])])))));
        assert!(index.scope("project", BTreeSet::from([source])).is_ok());
    }

    #[test]
    fn malformed_dependency_delta_recovers_without_partial_graph() {
        let source = reference("1").source;
        let unrelated = IssueSource { scope: "org/other".into(), ..source.clone() };
        let mut index = MissionBoardIndex::default();
        let initial = vec![board_issue("1", None, &[], &[]), board_issue("2", None, &[], &["1"])];
        index.apply_source(source.clone(), Some((1, Arc::new(observation(source.clone(), initial)))));
        index.apply_source(unrelated.clone(), Some((2, Arc::new(observation(unrelated.clone(), vec![])))));
        index.scope("project", BTreeSet::from([source.clone()])).expect("initial");
        let other = index.scope("other", BTreeSet::from([unrelated.clone()])).expect("unrelated");
        let recovered =
            vec![board_issue("1", None, &["weight:7"], &[]), board_issue("2", None, &[], &["1"]), board_issue("3", None, &[], &["2"])];
        let mut malformed = recovered.clone();
        malformed[1].blocked_by[0].url = "not an issue URL".into();
        index.apply_source(source.clone(), Some((3, Arc::new(observation(source.clone(), malformed)))));
        assert!(index.scope("project", BTreeSet::from([source.clone()])).is_err());
        assert!(Arc::ptr_eq(&other, &index.scope("other", BTreeSet::from([unrelated])).expect("unchanged")));
        let rebuilds = index.rebuilds();
        index.apply_source(source.clone(), Some((4, Arc::new(observation(source.clone(), recovered.clone())))));
        let actual = index.scope("project", BTreeSet::from([source])).expect("recovery");
        assert_eq!(index.rebuilds(), rebuilds + 1);
        let expected = board(recovered);
        let policy = DispatchPolicy::builder().build();
        for id in ["1", "2", "3"] {
            assert_eq!(actual.score(&issue(id), &policy), expected.score(&issue(id), &policy));
        }
    }

    // Invalid explicit inputs fail closed, while retired labels are ignored.
    #[test]
    fn malformed_attributes_are_errors() {
        for labels in
            [vec!["weight:-1".into()], vec!["weight:NaN".into()], vec!["priority:nope".into()], vec!["weight:1".into(), "weight:2".into()]]
        {
            assert!(normalize_attributes(&MissionFields::default(), &labels, &MissionAttributes::default()).is_err());
        }
        assert!(normalize_attributes(
            &MissionFields::default(),
            &["value:nope".into(), "cos:bad".into(), "crew-limit:-1".into()],
            &MissionAttributes::default()
        )
        .is_ok());
        assert!(parse_mission_fields(&[serde_json::json!({"issue_field_name":"Weight","value":"high"})]).is_err());
    }
}

#[cfg(test)]
mod query_contract {
    use flotilla_projections::dispatch_board::live_project_convoys_query;
    // WORKLOAD.md: retained history must not multiply decoded live-accounting
    // objects. Compare zero history to 1,000 terminal records using the real
    // indexed store; unrelated projects and missing-status convoys are included.
    #[tokio::test]
    async fn live_inventory_reads_are_flat_with_terminal_history() {
        use flotilla_resources::{Convoy, ConvoyPhase, ConvoySpec, ConvoyStatus, InputMeta};
        use flotilla_store::ResourceBackend;
        use flotilla_store_memory::InMemoryBackend;
        use flotilla_store_testkit::{assert_flat_reads, measure_reads, ReadCountsBackendExt};
        assert_flat_reads([0, 1000], |history| async move {
            let backend = ResourceBackend::new(InMemoryBackend::default()).with_read_counts();
            let convoys = backend.using::<Convoy>("ns");
            for index in 0..history + 3 {
                let record = convoys
                    .create(
                        &InputMeta::builder().name(format!("c-{index}")).build(),
                        &ConvoySpec::builder()
                            .workflow_ref("workflow".into())
                            .project_ref(if index == history + 2 { "other" } else { "p" }.into())
                            .build(),
                    )
                    .await
                    .expect("convoy");
                if index < history {
                    convoys
                        .update_status(
                            &record.metadata.name,
                            &record.metadata.resource_version,
                            &ConvoyStatus { phase: ConvoyPhase::Landed, ..Default::default() },
                        )
                        .await
                        .expect("terminal history");
                }
            }
            let (rows, reads) = measure_reads(backend.query_declared::<Convoy>("ns", &live_project_convoys_query(["p".into()]))).await;
            assert_eq!(rows.expect("live query").items.len(), 2);
            assert!(backend
                .query_declared::<Convoy>("ns", &live_project_convoys_query([]))
                .await
                .expect("empty projects")
                .items
                .is_empty());
            reads.of(&backend)
        })
        .await;
    }
}
