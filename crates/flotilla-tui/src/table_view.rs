//! Surface-agnostic table view model for curated query families (ADR 0012).
//!
//! This module contains no ratatui or input-event types. Typed row registries
//! project query rows into semantic cells and intents; surfaces decide how to
//! render and invoke them.

use std::collections::{HashMap, HashSet};

use flotilla_protocol::{HostName, IndependentRow, RepoKey, SessionPhase, ViewAddress};

use crate::convoy_model::{ConvoyPhase, ConvoySummary, VesselSummary, WorkPhase};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RowId(String);

impl RowId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TableState {
    selected: Option<RowId>,
    pub filter: String,
    pub scroll_offset: usize,
}

impl TableState {
    pub fn selected(&self) -> Option<&RowId> {
        self.selected.as_ref()
    }

    pub fn selected_index(&self, view: &TableView) -> Option<usize> {
        let selected = self.selected.as_ref()?;
        view.rows.iter().position(|row| &row.id == selected)
    }

    pub fn reconcile(&mut self, view: &TableView) {
        let selected_exists = self.selected.as_ref().is_some_and(|selected| view.rows.iter().any(|row| &row.id == selected));
        if !selected_exists {
            self.selected = view.rows.first().map(|row| row.id.clone());
        }
        self.scroll_offset = self.scroll_offset.min(view.rows.len().saturating_sub(1));
    }

    pub fn select_delta(&mut self, view: &TableView, delta: isize) {
        if view.rows.is_empty() {
            self.selected = None;
            self.scroll_offset = 0;
            return;
        }
        let current = self.selected.as_ref().and_then(|selected| view.rows.iter().position(|row| &row.id == selected)).unwrap_or(0);
        let next = (current as isize + delta).clamp(0, (view.rows.len() - 1) as isize) as usize;
        self.selected = Some(view.rows[next].id.clone());
    }

    pub fn select_index(&mut self, view: &TableView, index: usize) {
        if let Some(row) = view.rows.get(index) {
            self.selected = Some(row.id.clone());
        }
    }

    pub fn ensure_selected_visible(&mut self, view: &TableView, visible_rows: usize) {
        if visible_rows == 0 || view.rows.is_empty() {
            self.scroll_offset = 0;
            return;
        }
        let Some(selected) = self.selected_index(view) else {
            self.scroll_offset = 0;
            return;
        };
        if selected < self.scroll_offset {
            self.scroll_offset = selected;
        } else if selected >= self.scroll_offset.saturating_add(visible_rows) {
            self.scroll_offset = selected + 1 - visible_rows;
        }
        self.scroll_offset = self.scroll_offset.min(view.rows.len().saturating_sub(visible_rows));
    }

    pub fn selected_row<'a>(&self, view: &'a TableView) -> Option<&'a ProjectedRow> {
        let selected = self.selected.as_ref()?;
        view.rows.iter().find(|row| &row.id == selected)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Alignment {
    Left,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WidthHint {
    Fixed(u16),
    Flexible { minimum: u16, weight: u16 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellTone {
    Plain,
    Muted,
    Success,
    Warning,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CellValue {
    pub text: String,
    pub tone: CellTone,
}

impl CellValue {
    fn plain(text: impl Into<String>) -> Self {
        Self { text: text.into(), tone: CellTone::Plain }
    }

    fn toned(text: impl Into<String>, tone: CellTone) -> Self {
        Self { text: text.into(), tone }
    }
}

pub struct ColumnSpec<R> {
    pub id: &'static str,
    pub label: &'static str,
    pub width: WidthHint,
    pub alignment: Alignment,
    extract: fn(&R) -> CellValue,
}

pub struct ActionSpec<R> {
    pub id: &'static str,
    pub label: &'static str,
    pub key: char,
    resolve: fn(&R) -> Option<TableIntent>,
}

struct RowSpec<R> {
    id: fn(&R) -> RowId,
    drill: fn(&R) -> Option<ViewAddress>,
    describe: fn(&R) -> Vec<DetailField>,
}

struct TableSpec<R: 'static> {
    columns: &'static [ColumnSpec<R>],
    actions: &'static [ActionSpec<R>],
    row: RowSpec<R>,
}

impl<R: 'static> TableSpec<R> {
    fn project(&self, title: String, rows: impl IntoIterator<Item = R>) -> TableView {
        let columns = self
            .columns
            .iter()
            .map(|column| ProjectedColumn { id: column.id, label: column.label, width: column.width, alignment: column.alignment })
            .collect();
        let rows = rows
            .into_iter()
            .map(|row| ProjectedRow {
                id: (self.row.id)(&row),
                cells: self.columns.iter().map(|column| (column.extract)(&row)).collect(),
                drill: (self.row.drill)(&row),
                describe: (self.row.describe)(&row),
                actions: self
                    .actions
                    .iter()
                    .filter_map(|action| {
                        (action.resolve)(&row).map(|intent| AvailableAction { id: action.id, label: action.label, key: action.key, intent })
                    })
                    .collect(),
            })
            .collect();
        TableView { title, columns, rows }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, bon::Builder)]
pub struct ProjectedColumn {
    pub id: &'static str,
    pub label: &'static str,
    pub width: WidthHint,
    pub alignment: Alignment,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetailField {
    pub label: &'static str,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TableIntent {
    AttachWorkspace { workspace_ref: String, host: HostName, repo_hint: Option<RepoKey> },
    AttachPane { reference: String, host: HostName },
    ForceCompleteWork { convoy: String, vessel: String, host: HostName },
}

#[derive(Debug, Clone, PartialEq, Eq, bon::Builder)]
pub struct AvailableAction {
    pub id: &'static str,
    pub label: &'static str,
    pub key: char,
    pub intent: TableIntent,
}

#[derive(Debug, Clone, PartialEq, Eq, bon::Builder)]
pub struct ProjectedRow {
    pub id: RowId,
    pub cells: Vec<CellValue>,
    pub drill: Option<ViewAddress>,
    pub describe: Vec<DetailField>,
    pub actions: Vec<AvailableAction>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableView {
    pub title: String,
    pub columns: Vec<ProjectedColumn>,
    pub rows: Vec<ProjectedRow>,
}

impl TableView {
    pub fn filtered(mut self, filter: &str) -> Self {
        let filter = filter.trim().to_lowercase();
        if !filter.is_empty() {
            self.rows.retain(|row| row.cells.iter().any(|cell| cell.text.to_lowercase().contains(&filter)));
        }
        self
    }
}

#[derive(Clone)]
struct VesselProjection {
    namespace: String,
    convoy: String,
    repo_hint: Option<RepoKey>,
    vessel: VesselSummary,
}

/// Typed query rows currently available to the table registry. Surfaces build
/// this once from their query caches; adding a query family grows this input
/// without teaching the reusable widget about that family.
#[derive(Default)]
pub struct TableRows<'a> {
    pub convoys: Vec<&'a ConvoySummary>,
    pub independents: Vec<&'a IndependentRow>,
}

pub fn project(address: &ViewAddress, data: &TableRows<'_>) -> Result<TableView, String> {
    match address {
        ViewAddress::Convoys { namespace } => {
            let rows = data.convoys.iter().copied().filter(|convoy| &convoy.namespace == namespace).cloned();
            Ok(convoy_spec().project(format!("Convoys · {namespace}"), rows))
        }
        ViewAddress::Independents => {
            let mut rows = data.independents.clone();
            rows.sort_by(|left, right| {
                (&left.name, left.host.as_str(), &left.resource.namespace).cmp(&(
                    &right.name,
                    right.host.as_str(),
                    &right.resource.namespace,
                ))
            });
            Ok(independent_spec().project("Independents".to_string(), rows.into_iter().cloned()))
        }
        ViewAddress::Project { namespace, name } => {
            let rows = data
                .convoys
                .iter()
                .copied()
                .filter(|convoy| &convoy.namespace == namespace && convoy.project_ref.as_deref() == Some(name))
                .cloned();
            Ok(convoy_spec().project(format!("Project · {name}"), rows))
        }
        ViewAddress::Convoy { namespace, name } => {
            let convoy = find_convoy(&data.convoys, namespace, name)?;
            let rows = stable_topological_vessels(&convoy.vessels).into_iter().map(|vessel| VesselProjection {
                namespace: namespace.clone(),
                convoy: name.clone(),
                repo_hint: convoy.repo_hint.clone(),
                vessel: vessel.clone(),
            });
            Ok(vessel_spec().project(format!("Convoy · {name}"), rows))
        }
        ViewAddress::Vessel { namespace, convoy, vessel } => {
            let convoy_row = find_convoy(&data.convoys, namespace, convoy)?;
            let vessel_row = convoy_row
                .vessels
                .iter()
                .find(|candidate| &candidate.name == vessel)
                .ok_or_else(|| format!("vessel not found: {namespace}/{convoy}/{vessel}"))?;
            Ok(vessel_spec().project(format!("Vessel · {vessel}"), [VesselProjection {
                namespace: namespace.clone(),
                convoy: convoy.clone(),
                repo_hint: convoy_row.repo_hint.clone(),
                vessel: vessel_row.clone(),
            }]))
        }
        ViewAddress::Overview | ViewAddress::Repo { .. } => Err(format!("view is not table-backed: {address}")),
    }
}

fn find_convoy<'a>(convoys: &'a [&ConvoySummary], namespace: &str, name: &str) -> Result<&'a ConvoySummary, String> {
    convoys
        .iter()
        .copied()
        .find(|convoy| convoy.namespace == namespace && convoy.name == name)
        .ok_or_else(|| format!("convoy not found: {namespace}/{name}"))
}

fn stable_topological_vessels(vessels: &[VesselSummary]) -> Vec<&VesselSummary> {
    let indices: HashMap<&str, usize> = vessels.iter().enumerate().map(|(index, vessel)| (vessel.name.as_str(), index)).collect();
    let mut indegree = vec![0usize; vessels.len()];
    let mut dependants = vec![Vec::new(); vessels.len()];
    for (index, vessel) in vessels.iter().enumerate() {
        for dependency in &vessel.depends_on {
            if let Some(&dependency_index) = indices.get(dependency.as_str()) {
                indegree[index] += 1;
                dependants[dependency_index].push(index);
            }
        }
    }

    let mut emitted = HashSet::new();
    let mut ordered = Vec::with_capacity(vessels.len());
    while let Some(index) = (0..vessels.len()).find(|index| indegree[*index] == 0 && !emitted.contains(index)) {
        emitted.insert(index);
        ordered.push(&vessels[index]);
        for dependant in &dependants[index] {
            indegree[*dependant] = indegree[*dependant].saturating_sub(1);
        }
    }
    // Invalid/cyclic snapshots must remain visible and diagnosable.
    ordered.extend(vessels.iter().enumerate().filter(|(index, _)| !emitted.contains(index)).map(|(_, vessel)| vessel));
    ordered
}

fn convoy_spec() -> TableSpec<ConvoySummary> {
    TableSpec { columns: &CONVOY_COLUMNS, actions: &[], row: RowSpec { id: convoy_id, drill: convoy_drill, describe: convoy_description } }
}

fn vessel_spec() -> TableSpec<VesselProjection> {
    TableSpec {
        columns: &VESSEL_COLUMNS,
        actions: &VESSEL_ACTIONS,
        row: RowSpec { id: vessel_id, drill: vessel_drill, describe: vessel_description },
    }
}

fn independent_spec() -> TableSpec<IndependentRow> {
    TableSpec {
        columns: &INDEPENDENT_COLUMNS,
        actions: &INDEPENDENT_ACTIONS,
        row: RowSpec { id: independent_id, drill: |_| None, describe: independent_description },
    }
}

static CONVOY_COLUMNS: [ColumnSpec<ConvoySummary>; 6] = [
    ColumnSpec {
        id: "name",
        label: "CONVOY",
        width: WidthHint::Flexible { minimum: 12, weight: 2 },
        alignment: Alignment::Left,
        extract: |row| CellValue::plain(&row.name),
    },
    ColumnSpec {
        id: "workflow",
        label: "WORKFLOW",
        width: WidthHint::Flexible { minimum: 10, weight: 1 },
        alignment: Alignment::Left,
        extract: |row| CellValue::plain(&row.workflow_ref),
    },
    ColumnSpec { id: "phase", label: "PHASE", width: WidthHint::Fixed(12), alignment: Alignment::Left, extract: convoy_phase },
    ColumnSpec { id: "vessels", label: "VESSELS", width: WidthHint::Fixed(9), alignment: Alignment::Right, extract: convoy_progress },
    ColumnSpec {
        id: "scope",
        label: "PROJECT / REPO",
        width: WidthHint::Flexible { minimum: 12, weight: 1 },
        alignment: Alignment::Left,
        extract: convoy_scope,
    },
    ColumnSpec {
        id: "message",
        label: "MESSAGE",
        width: WidthHint::Flexible { minimum: 16, weight: 2 },
        alignment: Alignment::Left,
        extract: |row| CellValue::plain(row.message.as_deref().unwrap_or_default()),
    },
];

static VESSEL_COLUMNS: [ColumnSpec<VesselProjection>; 6] = [
    ColumnSpec {
        id: "depends_on",
        label: "↳",
        width: WidthHint::Fixed(2),
        alignment: Alignment::Left,
        extract: |row| CellValue::toned(if row.vessel.depends_on.is_empty() { "•" } else { "↳" }, CellTone::Muted),
    },
    ColumnSpec {
        id: "name",
        label: "VESSEL",
        width: WidthHint::Flexible { minimum: 12, weight: 2 },
        alignment: Alignment::Left,
        extract: |row| CellValue::plain(&row.vessel.name),
    },
    ColumnSpec {
        id: "crew",
        label: "CREW",
        width: WidthHint::Flexible { minimum: 8, weight: 1 },
        alignment: Alignment::Left,
        extract: vessel_crew,
    },
    ColumnSpec { id: "phase", label: "PHASE", width: WidthHint::Fixed(10), alignment: Alignment::Left, extract: vessel_phase },
    ColumnSpec {
        id: "host",
        label: "HOST",
        width: WidthHint::Flexible { minimum: 8, weight: 1 },
        alignment: Alignment::Left,
        extract: |row| CellValue::plain(row.vessel.host.as_ref().map(ToString::to_string).unwrap_or_default()),
    },
    ColumnSpec {
        id: "message",
        label: "MESSAGE",
        width: WidthHint::Flexible { minimum: 16, weight: 2 },
        alignment: Alignment::Left,
        extract: |row| CellValue::plain(row.vessel.message.as_deref().unwrap_or_default()),
    },
];

static VESSEL_ACTIONS: [ActionSpec<VesselProjection>; 2] =
    [ActionSpec { id: "attach", label: "Attach workspace", key: 'a', resolve: attach_vessel }, ActionSpec {
        id: "force_complete",
        label: "Force-complete work",
        key: 'x',
        resolve: force_complete_vessel,
    }];

static INDEPENDENT_COLUMNS: [ColumnSpec<IndependentRow>; 5] = [
    ColumnSpec {
        id: "name",
        label: "NAME",
        width: WidthHint::Flexible { minimum: 14, weight: 2 },
        alignment: Alignment::Left,
        extract: |row| CellValue::plain(&row.name),
    },
    ColumnSpec {
        id: "repo",
        label: "REPO",
        width: WidthHint::Flexible { minimum: 14, weight: 2 },
        alignment: Alignment::Left,
        extract: |row| CellValue::plain(row.repo.as_ref().map(|repo| repo.0.as_str()).unwrap_or("—")),
    },
    ColumnSpec {
        id: "host",
        label: "HOST",
        width: WidthHint::Flexible { minimum: 8, weight: 1 },
        alignment: Alignment::Left,
        extract: |row| CellValue::plain(row.host.to_string()),
    },
    ColumnSpec { id: "phase", label: "PHASE", width: WidthHint::Fixed(9), alignment: Alignment::Left, extract: independent_phase },
    ColumnSpec {
        id: "attach",
        label: "ATTACH",
        width: WidthHint::Fixed(11),
        alignment: Alignment::Left,
        extract: |row| {
            if row.attach.is_some() {
                CellValue::toned("available", CellTone::Success)
            } else {
                CellValue::toned("unavailable", CellTone::Muted)
            }
        },
    },
];

static INDEPENDENT_ACTIONS: [ActionSpec<IndependentRow>; 1] =
    [ActionSpec { id: "attach", label: "Attach temporarily", key: 'a', resolve: attach_independent }];

fn convoy_id(row: &ConvoySummary) -> RowId {
    RowId::new(row.id.as_str())
}

fn convoy_drill(row: &ConvoySummary) -> Option<ViewAddress> {
    Some(ViewAddress::Convoy { namespace: row.namespace.clone(), name: row.name.clone() })
}

fn convoy_phase(row: &ConvoySummary) -> CellValue {
    if row.initializing && !row.phase.is_terminal() {
        return CellValue::toned("initializing", CellTone::Warning);
    }
    let tone = match row.phase {
        ConvoyPhase::Pending => CellTone::Muted,
        ConvoyPhase::Active => CellTone::Plain,
        ConvoyPhase::Completed => CellTone::Success,
        ConvoyPhase::Failed => CellTone::Error,
        ConvoyPhase::Cancelled => CellTone::Muted,
    };
    CellValue::toned(row.phase.label(), tone)
}

fn convoy_progress(row: &ConvoySummary) -> CellValue {
    let complete = row.vessels.iter().filter(|vessel| vessel.phase == WorkPhase::Complete).count();
    CellValue::plain(format!("{complete}/{}", row.vessels.len()))
}

fn convoy_scope(row: &ConvoySummary) -> CellValue {
    CellValue::plain(row.project_ref.as_deref().or_else(|| row.repo_hint.as_ref().map(|repo| repo.0.as_str())).unwrap_or_default())
}

fn convoy_description(row: &ConvoySummary) -> Vec<DetailField> {
    vec![
        DetailField { label: "Namespace", value: row.namespace.clone() },
        DetailField { label: "Convoy", value: row.name.clone() },
        DetailField { label: "Workflow", value: row.workflow_ref.clone() },
        DetailField { label: "Phase", value: convoy_phase(row).text },
        DetailField { label: "Message", value: row.message.clone().unwrap_or_default() },
        DetailField { label: "Vessels", value: convoy_progress(row).text },
    ]
}

fn vessel_id(row: &VesselProjection) -> RowId {
    let host = row.vessel.host.as_ref().map(ToString::to_string).unwrap_or_default();
    RowId::new(format!("{}/{}/{}@{host}", row.namespace, row.convoy, row.vessel.name))
}

fn vessel_drill(row: &VesselProjection) -> Option<ViewAddress> {
    Some(ViewAddress::Vessel { namespace: row.namespace.to_string(), convoy: row.convoy.to_string(), vessel: row.vessel.name.clone() })
}

fn vessel_crew(row: &VesselProjection) -> CellValue {
    CellValue::plain(row.vessel.crew.iter().map(|member| member.role.as_str()).collect::<Vec<_>>().join(", "))
}

fn vessel_phase(row: &VesselProjection) -> CellValue {
    let (label, tone) = match row.vessel.phase {
        WorkPhase::Pending => ("pending", CellTone::Muted),
        WorkPhase::Ready => ("ready", CellTone::Warning),
        WorkPhase::Launching => ("launching", CellTone::Warning),
        WorkPhase::Running => ("running", CellTone::Plain),
        WorkPhase::Complete => ("complete", CellTone::Success),
        WorkPhase::Failed => ("failed", CellTone::Error),
        WorkPhase::Cancelled => ("cancelled", CellTone::Muted),
    };
    CellValue::toned(label, tone)
}

fn vessel_description(row: &VesselProjection) -> Vec<DetailField> {
    vec![
        DetailField { label: "Namespace", value: row.namespace.to_string() },
        DetailField { label: "Convoy", value: row.convoy.to_string() },
        DetailField { label: "Vessel", value: row.vessel.name.clone() },
        DetailField { label: "Depends on", value: row.vessel.depends_on.join(", ") },
        DetailField { label: "Phase", value: vessel_phase(row).text },
        DetailField { label: "Crew", value: vessel_crew(row).text },
        DetailField { label: "Host", value: row.vessel.host.as_ref().map(ToString::to_string).unwrap_or_default() },
        DetailField { label: "Message", value: row.vessel.message.clone().unwrap_or_default() },
    ]
}

fn attach_vessel(row: &VesselProjection) -> Option<TableIntent> {
    Some(TableIntent::AttachWorkspace {
        workspace_ref: row.vessel.workspace_ref.clone()?,
        host: row.vessel.host.clone()?,
        repo_hint: row.repo_hint.clone(),
    })
}

fn force_complete_vessel(row: &VesselProjection) -> Option<TableIntent> {
    let target = row.vessel.completion_target.as_ref()?;
    Some(TableIntent::ForceCompleteWork { convoy: target.convoy.clone(), vessel: target.vessel.clone(), host: target.host.clone() })
}

fn independent_id(row: &IndependentRow) -> RowId {
    RowId::new(format!("{}/{}/{}/{}@{}", row.resource.api_version, row.resource.kind, row.resource.namespace, row.resource.name, row.host))
}

fn independent_phase(row: &IndependentRow) -> CellValue {
    let tone = match row.phase {
        SessionPhase::Starting => CellTone::Warning,
        SessionPhase::Running => CellTone::Success,
        SessionPhase::Stopped => CellTone::Muted,
        SessionPhase::Failed => CellTone::Error,
    };
    CellValue::toned(row.phase.to_string(), tone)
}

fn independent_description(row: &IndependentRow) -> Vec<DetailField> {
    vec![
        DetailField { label: "Namespace", value: row.resource.namespace.clone() },
        DetailField { label: "Name", value: row.name.clone() },
        DetailField { label: "Repository", value: row.repo.as_ref().map(|repo| repo.0.clone()).unwrap_or_else(|| "—".to_string()) },
        DetailField { label: "Host", value: row.host.to_string() },
        DetailField { label: "Phase", value: row.phase.to_string() },
        DetailField { label: "Attach", value: if row.attach.is_some() { "available" } else { "unavailable" }.to_string() },
    ]
}

fn attach_independent(row: &IndependentRow) -> Option<TableIntent> {
    Some(TableIntent::AttachPane { reference: row.attach.clone()?, host: row.host.clone() })
}

#[cfg(test)]
mod tests {
    use flotilla_protocol::ResourceRef;

    use super::*;
    use crate::convoy_model::{ConvoyId, ProcessSummary, WorkCompletionTarget};

    fn project_convoys(address: &str, convoys: &[&ConvoySummary]) -> Result<TableView, String> {
        project(&address.parse().expect("valid address"), &TableRows { convoys: convoys.to_vec(), independents: vec![] })
    }

    fn vessel(name: &str, depends_on: &[&str], phase: WorkPhase) -> VesselSummary {
        VesselSummary {
            name: name.into(),
            depends_on: depends_on.iter().map(ToString::to_string).collect(),
            phase,
            crew: vec![ProcessSummary { role: "coder".into(), command_preview: "codex".into() }],
            host: Some(HostName::new("kiwi")),
            checkout: None,
            workspace_ref: None,
            completion_target: None,
            ready_at: None,
            started_at: None,
            finished_at: None,
            message: None,
        }
    }

    fn convoy(vessels: Vec<VesselSummary>) -> ConvoySummary {
        ConvoySummary {
            id: ConvoyId::new("dev", "tables"),
            namespace: "dev".into(),
            name: "tables".into(),
            workflow_ref: "implement-review".into(),
            phase: ConvoyPhase::Active,
            message: None,
            repo_hint: None,
            project_ref: Some("flotilla".into()),
            vessels,
            started_at: None,
            finished_at: None,
            observed_workflow_ref: None,
            initializing: false,
        }
    }

    fn independent(name: &str, host: &str, attach: Option<&str>) -> IndependentRow {
        IndependentRow::builder()
            .resource(ResourceRef::new("flotilla.work/v1", "TerminalSession", "dev", name))
            .name(name)
            .repo(RepoKey("flotilla-org/flotilla".to_string()))
            .host(HostName::new(host))
            .maybe_attach(attach.map(ToString::to_string))
            .phase(SessionPhase::Running)
            .build()
    }

    #[test]
    fn convoy_projection_exposes_honest_phase_message_and_drill_target() {
        let mut row = convoy(vec![vessel("implement", &[], WorkPhase::Running)]);
        row.phase = ConvoyPhase::Failed;
        row.message = Some("workspace launch failed: disk full".into());
        let view = project_convoys("convoys/dev", &[&row]).expect("project table");

        assert_eq!(view.columns.iter().map(|column| column.id).collect::<Vec<_>>(), vec![
            "name", "workflow", "phase", "vessels", "scope", "message"
        ]);
        assert_eq!(view.rows[0].cells[2], CellValue::toned("failed", CellTone::Error));
        assert_eq!(view.rows[0].cells[5].text, "workspace launch failed: disk full");
        assert_eq!(view.rows[0].drill, Some("convoy/dev/tables".parse().expect("valid address")));
    }

    #[test]
    fn vessel_projection_is_stably_topological_and_keeps_dependency_glyphs() {
        let row = convoy(vec![
            vessel("review", &["implement"], WorkPhase::Pending),
            vessel("docs", &[], WorkPhase::Ready),
            vessel("implement", &[], WorkPhase::Running),
        ]);
        let view = project_convoys("convoy/dev/tables", &[&row]).expect("project table");

        let names = view.rows.iter().map(|row| row.cells[1].text.as_str()).collect::<Vec<_>>();
        assert_eq!(names, vec!["docs", "implement", "review"]);
        assert_eq!(view.rows[0].cells[0].text, "•");
        assert_eq!(view.rows[2].cells[0].text, "↳");
    }

    #[test]
    fn project_and_vessel_addresses_scope_rows_without_changing_the_widget_contract() {
        let in_project = convoy(vec![vessel("implement", &[], WorkPhase::Running), vessel("review", &["implement"], WorkPhase::Pending)]);
        let mut elsewhere = convoy(vec![]);
        elsewhere.id = ConvoyId::new("dev", "elsewhere");
        elsewhere.name = "elsewhere".into();
        elsewhere.project_ref = Some("other".into());

        let project_view = project_convoys("project/dev/flotilla", &[&in_project, &elsewhere]).expect("project table");
        assert_eq!(project_view.rows.iter().map(|row| row.cells[0].text.as_str()).collect::<Vec<_>>(), vec!["tables"]);

        let vessel = project_convoys("vessel/dev/tables/review", &[&in_project, &elsewhere]).expect("vessel table");
        assert_eq!(vessel.rows.len(), 1);
        assert_eq!(vessel.rows[0].cells[1].text, "review");
    }

    #[test]
    fn vessel_actions_are_resolved_only_when_capability_fields_allow_them() {
        let mut actionable = vessel("implement", &[], WorkPhase::Running);
        actionable.workspace_ref = Some("workspace-1".into());
        actionable.completion_target =
            Some(WorkCompletionTarget { convoy: "tables".into(), vessel: "implement".into(), host: HostName::new("kiwi") });
        let row = convoy(vec![actionable, vessel("review", &["implement"], WorkPhase::Pending)]);
        let view = project_convoys("convoy/dev/tables", &[&row]).expect("project table");

        assert_eq!(view.rows[0].actions.iter().map(|action| action.id).collect::<Vec<_>>(), vec!["attach", "force_complete"]);
        assert!(view.rows[1].actions.is_empty(), "unavailable actions must not render");
    }

    #[test]
    fn independent_projection_has_typed_columns_and_truthful_attach_action() {
        let available = independent("scratch", "kiwi", Some("terminal-scratch"));
        let unavailable = independent("wedged", "feta", None);
        let view = project(&ViewAddress::Independents, &TableRows { convoys: vec![], independents: vec![&unavailable, &available] })
            .expect("independents table");

        assert_eq!(view.columns.iter().map(|column| column.id).collect::<Vec<_>>(), vec!["name", "repo", "host", "phase", "attach"]);
        assert_eq!(view.rows.iter().map(|row| row.cells[0].text.as_str()).collect::<Vec<_>>(), vec!["scratch", "wedged"]);
        assert_eq!(view.rows[0].cells[4], CellValue::toned("available", CellTone::Success));
        assert_eq!(view.rows[0].actions[0].intent, TableIntent::AttachPane {
            reference: "terminal-scratch".to_string(),
            host: HostName::new("kiwi")
        });
        assert_eq!(view.rows[1].cells[4], CellValue::toned("unavailable", CellTone::Muted));
        assert!(view.rows[1].actions.is_empty(), "an unavailable attach must not render as an action");
    }

    #[test]
    fn table_state_preserves_identity_and_clamps_when_rows_disappear() {
        let first = convoy(vec![]);
        let mut second = convoy(vec![]);
        second.id = ConvoyId::new("dev", "other");
        second.name = "other".into();
        let address = "convoys/dev".parse().expect("valid address");
        let view = project(&address, &TableRows { convoys: vec![&first, &second], independents: vec![] }).expect("project table");
        let mut state = TableState::default();
        state.reconcile(&view);
        state.select_delta(&view, 1);
        assert_eq!(state.selected_row(&view).map(|row| row.cells[0].text.as_str()), Some("other"));

        let changed = project(&address, &TableRows { convoys: vec![&first], independents: vec![] }).expect("project table");
        state.reconcile(&changed);
        assert_eq!(state.selected_row(&changed).map(|row| row.cells[0].text.as_str()), Some("tables"));
    }

    #[test]
    fn table_state_scrolls_just_enough_to_keep_the_cursor_visible() {
        let mut rows = Vec::new();
        for name in ["one", "two", "three", "four"] {
            let mut row = convoy(vec![]);
            row.id = ConvoyId::new("dev", name);
            row.name = name.into();
            rows.push(row);
        }
        let refs = rows.iter().collect::<Vec<_>>();
        let view = project_convoys("convoys/dev", &refs).expect("project table");
        let mut state = TableState::default();
        state.reconcile(&view);
        state.select_delta(&view, 3);
        state.ensure_selected_visible(&view, 2);
        assert_eq!(state.scroll_offset, 2);

        state.select_delta(&view, -2);
        state.ensure_selected_visible(&view, 2);
        assert_eq!(state.scroll_offset, 1);
    }

    #[test]
    fn table_filter_matches_any_projected_cell() {
        let mut failed = convoy(vec![]);
        failed.phase = ConvoyPhase::Failed;
        failed.message = Some("disk full".into());
        let mut active = convoy(vec![]);
        active.id = ConvoyId::new("dev", "other");
        active.name = "other".into();
        let view = project_convoys("convoys/dev", &[&failed, &active]).expect("project table").filtered("DISK");

        assert_eq!(view.rows.len(), 1);
        assert_eq!(view.rows[0].cells[0].text, "tables");
    }
}
