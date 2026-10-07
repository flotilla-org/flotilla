//! Mission normalization and membership over one complete tracker observation.
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
};

use flotilla_protocol::{ClassOfService, DispatchBoardRepository, DispatchScore, Issue, IssueRef, MissionAttributes, MissionFields};
use flotilla_resources::DispatchPolicy;

pub fn issue_ref_from_url(url: &str) -> Result<IssueRef, String> {
    let url = url::Url::parse(url).map_err(|e| e.to_string())?;
    let parts = url.path_segments().ok_or("issue URL lacks path")?.collect::<Vec<_>>();
    if parts.len() != 4 || parts[2] != "issues" {
        return Err("invalid native issue URL".into());
    }
    Ok(IssueRef {
        source: flotilla_protocol::IssueSource { service: url.origin().ascii_serialization(), scope: format!("{}/{}", parts[0], parts[1]) },
        id: parts[3].into(),
    })
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

fn class(value: &str) -> Result<ClassOfService, String> {
    match value.to_ascii_lowercase().as_str() {
        "expedite" => Ok(ClassOfService::Expedite),
        "standard" => Ok(ClassOfService::Standard),
        "background" => Ok(ClassOfService::Background),
        _ => Err(format!("invalid class of service: {value}")),
    }
}

/// Preserve missing fields for per-attribute fallback. Invalid/duplicate known
/// fields are errors; unrelated organization fields are deliberately ignored.
pub fn parse_mission_fields(values: &[serde_json::Value]) -> Result<MissionFields, String> {
    let mut fields = MissionFields::default();
    let mut seen = BTreeSet::new();
    for value in values {
        let name = value["issue_field_name"].as_str().ok_or("issue field lacks name")?;
        if !matches!(name, "Value" | "Class of service" | "Crew limit") {
            continue;
        }
        if !seen.insert(name) {
            return Err(format!("duplicate mission field {name}"));
        }
        if value["value"].is_null() {
            continue;
        }
        match name {
            "Value" => fields.value = Some(value["value"].as_f64().ok_or("mission Value must be numeric")?.try_into()?),
            "Crew limit" => {
                fields.crew_limit = Some(
                    value["value"]
                        .as_f64()
                        .filter(|v| *v >= 0.0 && *v <= f64::from(u32::MAX) && v.fract() == 0.0)
                        .map(|v| v as u32)
                        .ok_or("mission Crew limit must be a nonnegative u32")?,
                )
            }
            _ => {
                fields.class_of_service = Some(class(
                    value["single_select_option"]["name"]
                        .as_str()
                        .or_else(|| value["value"].as_str())
                        .ok_or("mission class lacks option name")?,
                )?)
            }
        }
    }
    Ok(fields)
}

pub fn normalize_attributes(
    fields: &MissionFields,
    labels: &[String],
    defaults: &MissionAttributes,
) -> Result<(MissionAttributes, BTreeMap<String, String>), String> {
    let mut fallback = MissionFields::default();
    let mut seen = BTreeSet::new();
    for label in labels {
        let Some((name, value)) = label.split_once(':') else { continue };
        if !matches!(name, "value" | "cos" | "crew-limit") {
            continue;
        }
        // A field overrides even a stale/invalid label for that attribute.
        if matches!(name, "value") && fields.value.is_some()
            || matches!(name, "cos") && fields.class_of_service.is_some()
            || matches!(name, "crew-limit") && fields.crew_limit.is_some()
        {
            continue;
        }
        if !seen.insert(name) {
            return Err(format!("duplicate mission label {name}"));
        }
        match name {
            "value" => fallback.value = Some(value.parse::<f64>().map_err(|_| "invalid value label")?.try_into()?),
            "cos" => fallback.class_of_service = Some(class(value)?),
            _ => fallback.crew_limit = Some(value.parse().map_err(|_| format!("invalid crew-limit label crew-limit:{value}"))?),
        }
    }
    let sources = [
        ("value", fields.value.is_some(), fallback.value.is_some()),
        ("class_of_service", fields.class_of_service.is_some(), fallback.class_of_service.is_some()),
        ("crew_limit", fields.crew_limit.is_some(), fallback.crew_limit.is_some()),
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
            value: fields.value.or(fallback.value).unwrap_or(defaults.value),
            class_of_service: fields.class_of_service.or(fallback.class_of_service).unwrap_or(defaults.class_of_service),
            crew_limit: fields.crew_limit.or(fallback.crew_limit).or(defaults.crew_limit),
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
    pub rebuilds: usize,
    pub updated_issues: usize,
}

struct MissionScope {
    sources: BTreeSet<flotilla_protocol::IssueSource>,
    inputs: BTreeMap<flotilla_protocol::IssueSource, MissionSourceSnapshot>,
    board: Result<Arc<MissionBoard>, String>,
}

impl MissionBoardIndex {
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
                if issue.issue_type.as_ref().is_some_and(|kind| kind.eq_ignore_ascii_case("map"))
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
                if issue.issue_type.as_ref().is_some_and(|kind| kind.eq_ignore_ascii_case("map"))
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
        let (attributes, attribute_sources) = normalize_attributes(
            &tracking.as_ref().and_then(|i| self.fields.get(i)).cloned().unwrap_or_default(),
            tracking.as_ref().and_then(|i| self.labels.get(i)).map(Vec::as_slice).unwrap_or_default(),
            &defaults,
        )?;
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

    // #2783: each term dominates every later term, globally across Projects.
    // Generator covers each priority axis, all classes, zeros and large penalties;
    // fair-share inputs deliberately do not alter work-queue rank (#2785).
    #[hegel::test]
    fn lexicographic_rank_and_breakdown(tc: hegel::TestCase) {
        use hegel::generators as gs;
        let axis = tc.draw(gs::integers::<usize>().min_value(0).max_value(4));
        let penalty = tc.draw(gs::integers::<u64>().min_value(1).max_value(100000));
        let mut better = row("z");
        let mut worse = row("a");
        let left = better.score.as_mut().expect("score");
        let right = worse.score.as_mut().expect("score");
        // All later terms favor the worse row, independently of the selected axis.
        if axis < 4 {
            left.conflict_penalty = penalty;
        }
        if axis < 3 {
            worse.ready_observed_at -= chrono::Duration::days(1);
        }
        if axis < 2 {
            right.unblock_count = 100;
        }
        if axis < 1 {
            right.attributes.value = 100.0.try_into().expect("value");
        }
        match axis {
            0 => {
                left.attributes.class_of_service = ClassOfService::Expedite;
                right.attributes.class_of_service = ClassOfService::Background;
            }
            1 => {
                left.attributes.value = 2.5.try_into().expect("value");
                right.attributes.value = 2.0.try_into().expect("value");
            }
            2 => left.unblock_count = 101,
            3 => better.ready_observed_at -= chrono::Duration::days(2),
            _ => right.conflict_penalty = penalty,
        }
        left.project_active_crews = 1000;
        right.project_share = 1000;
        assert_eq!(compare_dispatch_rows(&better, &worse), Ordering::Less);
        assert_eq!(compare_dispatch_rows(&worse, &better), Ordering::Greater);
        assert_eq!(compare_dispatch_rows(&better, &better), Ordering::Equal);
        let mut rows = [worse, better.clone()];
        rows.sort_by(compare_dispatch_rows);
        assert_eq!(rows[0], better);
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

    // #2783: each attribute independently selects field, label, then charter;
    // generators cover every source, zero/max limits, negative and decimal values.
    #[hegel::test]
    fn sources_normalize_per_attribute(tc: hegel::TestCase) {
        use hegel::generators as gs;
        let value_source = tc.draw(gs::integers::<usize>().min_value(0).max_value(2));
        let class_source = tc.draw(gs::integers::<usize>().min_value(0).max_value(2));
        let limit_source = tc.draw(gs::integers::<usize>().min_value(0).max_value(2));
        let value = f64::from(tc.draw(gs::integers::<i32>().min_value(-100).max_value(100))) / 4.0;
        let class_index = tc.draw(gs::integers::<usize>().min_value(0).max_value(2));
        let class_names = ["expedite", "standard", "background"];
        let classes = [ClassOfService::Expedite, ClassOfService::Standard, ClassOfService::Background];
        let limit = if tc.draw(gs::booleans()) { 0 } else { u32::MAX };
        let defaults = MissionAttributes {
            value: (-999.0).try_into().expect("value"),
            class_of_service: ClassOfService::Background,
            crew_limit: Some(4),
        };
        let fields = parse_mission_fields(&[
            serde_json::json!({"issue_field_name":"Value", "value": if value_source == 0 { Some(value) } else { None }}),
            serde_json::json!({"issue_field_name":"Class of service", "value": if class_source == 0 { Some(1) } else { None }, "single_select_option":{"name":class_names[class_index]}}),
            serde_json::json!({"issue_field_name":"Crew limit", "value": if limit_source == 0 { Some(limit) } else { None }}),
        ]).expect("documented issue field shape");
        let labels = [
            (value_source, format!("value:{value}")),
            (class_source, format!("cos:{}", class_names[class_index])),
            (limit_source, format!("crew-limit:{limit}")),
        ]
        .into_iter()
        .filter_map(|(source, label)| (source < 2).then_some(label))
        .collect::<Vec<_>>();
        let (actual, sources) = normalize_attributes(&fields, &labels, &defaults).expect("normalized");
        assert_eq!(f64::from(actual.value), if value_source < 2 { value } else { -999.0 });
        assert_eq!(actual.class_of_service, if class_source < 2 { classes[class_index] } else { ClassOfService::Background });
        assert_eq!(actual.crew_limit, Some(if limit_source < 2 { limit } else { 4 }));
        for (name, source) in [("value", value_source), ("class_of_service", class_source), ("crew_limit", limit_source)] {
            assert_eq!(sources[name], ["issue_field", "label", "charter"][source]);
        }
    }

    // #2783: native map ancestry wins over ordered lane rules, whose first match
    // wins over routine. DAG descendants count unique tickets, not diamond paths.
    #[test]
    fn membership_and_descendants_follow_native_graph() {
        let board = board(vec![
            board_issue("map", None, &["wayfinder:map", "value:9"], &[]),
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
            .lanes(vec![DispatchLane { mission: "stability".into(), labels: BTreeSet::from(["bug".into()]) }, DispatchLane {
                mission: "other".into(),
                labels: BTreeSet::new(),
            }])
            .build();
        let score = board.score(&issue("1"), &policy).expect("map member");
        assert_eq!(score.mission_issue, Some(reference("map")));
        assert_eq!(score.membership, "map_sub_issue");
        assert_eq!(f64::from(score.attributes.value), 9.0);
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
                        vec![board_issue("map", None, &["map", "value:4"], &[]), board_issue("1", Some("map"), &[], &[])]
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

    // Invalid explicit inputs are unavailable evidence; duplicate inputs cannot
    // silently make priority depend on tracker array ordering.
    #[test]
    fn malformed_attributes_are_errors() {
        for labels in [
            vec!["value:nope".into()],
            vec!["cos:urgent".into()],
            vec!["crew-limit:-1".into()],
            vec!["value:1".into(), "value:2".into()],
            vec!["value:NaN".into()],
        ] {
            assert!(normalize_attributes(&MissionFields::default(), &labels, &MissionAttributes::default()).is_err());
        }
        let fields = MissionFields { value: Some(3.0.try_into().expect("value")), ..Default::default() };
        assert_eq!(
            normalize_attributes(&fields, &["value:nope".into()], &MissionAttributes::default())
                .expect("field overrides stale label")
                .0
                .value,
            fields.value.expect("value")
        );
        assert!(parse_mission_fields(&[serde_json::json!({"issue_field_name":"Value","value":"high"})]).is_err());
    }
}
