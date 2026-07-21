//! Surface-agnostic table view model for curated query families (ADR 0012).
//!
//! This module contains no ratatui or input-event types. Typed row registries
//! project query rows into semantic cells and intents; surfaces decide how to
//! render and invoke them.

use std::collections::{HashMap, HashSet};

use flotilla_protocol::{
    result_set::Timestamp, CheckoutRow, HostName, IndependentRow, IssueRef, IssueRow, QueryId, QueryScope, RepoKey, ResultSetCondition,
    ResultSetState, SessionPhase, ViewAddress,
};

use crate::convoy_model::{ConvoyPhase, ConvoySummary, VesselSummary, WorkPhase};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RowId(String);

impl RowId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, bon::Builder)]
pub struct TableState {
    selected: Option<RowId>,
    pub filter: String,
    /// Ephemeral provider-side search for demand-backed issue tables. This is
    /// deliberately View state rather than part of the persisted address.
    pub source_search: Option<String>,
    pub scroll_offset: usize,
}

impl TableState {
    pub fn selected(&self) -> Option<&RowId> {
        self.selected.as_ref()
    }

    pub fn clear_selection(&mut self) {
        self.selected = None;
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

/// Cursor and transient search state for the fixed Project composition. Each
/// panel keeps the same tab-local state a standalone table owns; the page
/// cursor selects which panel participates in keyboard actions.
#[derive(Debug, Clone, PartialEq, Eq, bon::Builder)]
pub struct ProjectTableState {
    active: ProjectPanelKind,
    header_focused: bool,
    convoys: TableState,
    checkouts: TableState,
    issues: TableState,
    independents: TableState,
    scroll_offset: usize,
}

impl Default for ProjectTableState {
    fn default() -> Self {
        Self::builder()
            .active(ProjectPanelKind::Convoys)
            .header_focused(false)
            .convoys(TableState::default())
            .checkouts(TableState::default())
            .issues(TableState::default())
            .independents(TableState::default())
            .scroll_offset(0)
            .build()
    }
}

impl ProjectTableState {
    pub fn active(&self) -> ProjectPanelKind {
        self.active
    }

    pub fn set_active(&mut self, kind: ProjectPanelKind) {
        self.active = kind;
    }

    pub fn header_focused(&self) -> bool {
        self.header_focused
    }

    pub fn focus_header(&mut self) {
        self.header_focused = true;
    }

    pub fn focus_rows(&mut self) {
        self.header_focused = false;
    }

    pub fn scroll_offset(&self) -> usize {
        self.scroll_offset
    }

    pub fn set_scroll_offset(&mut self, offset: usize) {
        self.scroll_offset = offset;
    }

    pub fn table(&self, kind: ProjectPanelKind) -> &TableState {
        match kind {
            ProjectPanelKind::Convoys => &self.convoys,
            ProjectPanelKind::Checkouts => &self.checkouts,
            ProjectPanelKind::Issues => &self.issues,
            ProjectPanelKind::Independents => &self.independents,
        }
    }

    pub fn table_mut(&mut self, kind: ProjectPanelKind) -> &mut TableState {
        match kind {
            ProjectPanelKind::Convoys => &mut self.convoys,
            ProjectPanelKind::Checkouts => &mut self.checkouts,
            ProjectPanelKind::Issues => &mut self.issues,
            ProjectPanelKind::Independents => &mut self.independents,
        }
    }

    pub fn active_table(&self) -> &TableState {
        self.table(self.active)
    }

    pub fn active_table_mut(&mut self) -> &mut TableState {
        self.table_mut(self.active)
    }

    pub fn issue_source_search(&self) -> Option<&str> {
        self.issues.source_search.as_deref()
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
        TableView { title, columns, rows, meta: TableMeta::default() }
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
    DeleteConvoy { namespace: String, name: String, host: Option<HostName> },
    ForceCompleteWork { convoy: String, vessel: String, host: HostName },
    StartConvoy { namespace: String, project: String, issue: IssueRef },
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

#[derive(Debug, Clone, PartialEq, Eq, bon::Builder)]
pub struct TableView {
    pub title: String,
    pub columns: Vec<ProjectedColumn>,
    pub rows: Vec<ProjectedRow>,
    pub meta: TableMeta,
}

/// One fixed panel in the Project composite View. The panel owns no layout or
/// input policy: it is the same curated table projection a single-kind View
/// renders, with the address its header expands into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectPanelKind {
    Convoys,
    Checkouts,
    Issues,
    Independents,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectPanel {
    pub kind: ProjectPanelKind,
    pub target: ViewAddress,
    pub table: TableView,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TableAvailability {
    #[default]
    Ready,
    Loading,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, bon::Builder)]
pub struct TableMeta {
    pub as_of: Option<Timestamp>,
    pub has_more: bool,
    pub conditions: Vec<String>,
    pub availability: TableAvailability,
}

impl TableView {
    pub fn filtered(mut self, filter: &str) -> Self {
        let filter = filter.trim().to_lowercase();
        if !filter.is_empty() {
            self.rows.retain(|row| row.cells.iter().any(|cell| fuzzy_matches(&cell.text, &filter)));
        }
        self
    }
}

fn fuzzy_matches(value: &str, pattern: &str) -> bool {
    let mut pattern = pattern.chars();
    let mut next = pattern.next();
    for candidate in value.chars().flat_map(char::to_lowercase) {
        if next == Some(candidate) {
            next = pattern.next();
            if next.is_none() {
                return true;
            }
        }
    }
    next.is_none()
}

#[derive(Clone)]
struct VesselProjection {
    namespace: String,
    convoy: String,
    repo_hint: Option<RepoKey>,
    vessel: VesselSummary,
}

#[derive(Clone)]
struct ScopedIssueProjection {
    scope: QueryScope,
    row: IssueRow,
}

/// Typed query rows currently available to the table registry. Surfaces build
/// this once from their query caches; adding a query family grows this input
/// without teaching the reusable widget about that family.
#[derive(Default, bon::Builder)]
pub struct TableRows<'a> {
    pub convoys: Vec<&'a ConvoySummary>,
    pub independent_results: Vec<QueryRows<'a, IndependentRow>>,
    pub issue_results: Vec<QueryRows<'a, IssueRow>>,
    pub checkout_results: Vec<QueryRows<'a, CheckoutRow>>,
    pub source_search: Option<&'a str>,
}

pub struct QueryRows<'a, R> {
    pub query: &'a QueryId,
    pub rows: &'a [R],
    pub state: &'a ResultSetState,
}

/// Query identity owned by a single-family table address. Composite Views
/// add their other queries in `app::view_kind`, but use this same mapping for
/// curated table families to prevent subscription/projection drift.
pub fn query_for(address: &ViewAddress, source_search: Option<&str>) -> Option<QueryId> {
    match address {
        ViewAddress::Issues { scope } => {
            Some(QueryId::Issues { scope: scope.clone(), search: source_search.filter(|search| !search.is_empty()).map(str::to_owned) })
        }
        ViewAddress::Checkouts { scope } => Some(QueryId::Checkouts { scope: scope.clone() }),
        ViewAddress::Independents { scope } => Some(QueryId::Independents { scope: scope.clone() }),
        _ => None,
    }
}

pub fn project(address: &ViewAddress, data: &TableRows<'_>) -> Result<TableView, String> {
    match address {
        ViewAddress::Convoys { namespace } => {
            let rows = data.convoys.iter().copied().filter(|convoy| &convoy.namespace == namespace).cloned();
            Ok(convoy_spec().project(format!("Convoys · {namespace}"), rows))
        }
        ViewAddress::Independents { scope } => {
            let query = query_for(address, data.source_search).expect("independents address has a query");
            let result = data
                .independent_results
                .iter()
                .find(|result| *result.query == query)
                .ok_or_else(|| format!("result set not available: {query}"))?;
            let title = scope
                .as_ref()
                .map_or_else(|| "Independents · fleet".to_string(), |scope| format!("Independents · {}/{}", scope.namespace, scope.name));
            let mut rows = result.rows.to_vec();
            rows.sort_by(|left, right| {
                (&left.name, left.host.as_str(), &left.resource.namespace).cmp(&(
                    &right.name,
                    right.host.as_str(),
                    &right.resource.namespace,
                ))
            });
            let mut view = independent_spec().project(title, rows);
            view.meta = result_set_meta(result.state);
            Ok(view)
        }
        ViewAddress::Project { .. } => Err(format!("project views are composite: {address}")),
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
        ViewAddress::Issues { scope } => {
            let query = query_for(address, data.source_search).expect("issues address has a query");
            let result = data
                .issue_results
                .iter()
                .find(|result| *result.query == query)
                .ok_or_else(|| format!("result set not available: {query}"))?;
            let mut view = issue_spec().project(
                format!("Issues · {}/{}", scope.namespace, scope.name),
                result.rows.iter().cloned().map(|row| ScopedIssueProjection { scope: scope.clone(), row }),
            );
            view.meta = result_set_meta(result.state);
            Ok(view)
        }
        ViewAddress::Checkouts { scope } => {
            let query = query_for(address, data.source_search).expect("checkouts address has a query");
            let result = data
                .checkout_results
                .iter()
                .find(|result| *result.query == query)
                .ok_or_else(|| format!("result set not available: {query}"))?;
            let title = scope
                .as_ref()
                .map_or_else(|| "Checkouts · fleet".to_string(), |scope| format!("Checkouts · {}/{}", scope.namespace, scope.name));
            let mut view = checkout_spec().project(title, result.rows.iter().cloned());
            view.meta = result_set_meta(result.state);
            Ok(view)
        }
        ViewAddress::Overview | ViewAddress::Repo { .. } => Err(format!("view is not table-backed: {address}")),
    }
}

/// The fixed v1 Project composition. Each entry deliberately reuses the
/// standalone family projection so columns, row actions, result conditions,
/// and future registry changes stay identical in the embedded panel.
pub fn project_panels(address: &ViewAddress, data: &TableRows<'_>) -> Result<Vec<ProjectPanel>, String> {
    let ViewAddress::Project { namespace, name } = address else {
        return Err(format!("view is not project-backed: {address}"));
    };
    let scope = QueryScope::new(namespace, name);
    let convoys = convoy_spec().project(
        "Convoys".to_string(),
        data.convoys
            .iter()
            .copied()
            .filter(|convoy| &convoy.namespace == namespace && convoy.project_ref.as_deref() == Some(name))
            .cloned(),
    );
    let checkouts_address = ViewAddress::Checkouts { scope: Some(scope.clone()) };
    let issues_address = ViewAddress::Issues { scope: scope.clone() };
    let independents_address = ViewAddress::Independents { scope: Some(scope) };
    let mut checkouts = project(&checkouts_address, data).unwrap_or_else(|_| pending_project_table(&checkouts_address));
    let mut issues = project(&issues_address, data).unwrap_or_else(|_| pending_project_table(&issues_address));
    let mut independents = project(&independents_address, data).unwrap_or_else(|_| pending_project_table(&independents_address));
    checkouts.title = "Checkouts".to_string();
    issues.title = "Issues".to_string();
    independents.title = "Independents".to_string();

    Ok(vec![
        ProjectPanel { kind: ProjectPanelKind::Convoys, target: ViewAddress::Convoys { namespace: namespace.clone() }, table: convoys },
        ProjectPanel { kind: ProjectPanelKind::Checkouts, target: checkouts_address, table: checkouts },
        ProjectPanel { kind: ProjectPanelKind::Issues, target: issues_address, table: issues },
        ProjectPanel { kind: ProjectPanelKind::Independents, target: independents_address, table: independents },
    ])
}

fn pending_project_table(address: &ViewAddress) -> TableView {
    let mut table = match address {
        ViewAddress::Checkouts { scope } => checkout_spec().project(
            scope
                .as_ref()
                .map_or_else(|| "Checkouts · fleet".to_string(), |scope| format!("Checkouts · {}/{}", scope.namespace, scope.name)),
            std::iter::empty::<CheckoutRow>(),
        ),
        ViewAddress::Issues { scope } => {
            issue_spec().project(format!("Issues · {}/{}", scope.namespace, scope.name), std::iter::empty::<ScopedIssueProjection>())
        }
        ViewAddress::Independents { scope } => independent_spec().project(
            scope
                .as_ref()
                .map_or_else(|| "Independents · fleet".to_string(), |scope| format!("Independents · {}/{}", scope.namespace, scope.name)),
            std::iter::empty::<IndependentRow>(),
        ),
        _ => unreachable!("only scoped project panels can be pending"),
    };
    table.meta.availability = TableAvailability::Loading;
    table
}

fn result_set_meta(state: &ResultSetState) -> TableMeta {
    TableMeta::builder()
        .maybe_as_of(state.demand.as_ref().map(|metadata| metadata.as_of))
        .has_more(state.demand.as_ref().is_some_and(|metadata| metadata.has_more))
        .conditions(
            state
                .conditions
                .iter()
                .map(|condition| match condition {
                    ResultSetCondition::IssueSourceUnavailable { message, .. }
                    | ResultSetCondition::QueryScopeUnavailable { message, .. } => message.clone(),
                })
                .collect(),
        )
        .availability(TableAvailability::Ready)
        .build()
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
    TableSpec {
        columns: &CONVOY_COLUMNS,
        actions: &CONVOY_ACTIONS,
        row: RowSpec { id: convoy_id, drill: convoy_drill, describe: convoy_description },
    }
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

fn issue_spec() -> TableSpec<ScopedIssueProjection> {
    TableSpec {
        columns: &ISSUE_COLUMNS,
        actions: &ISSUE_ACTIONS,
        row: RowSpec { id: scoped_issue_id, drill: |_| None, describe: scoped_issue_description },
    }
}

fn checkout_spec() -> TableSpec<CheckoutRow> {
    TableSpec {
        columns: &CHECKOUT_COLUMNS,
        actions: &[],
        row: RowSpec { id: checkout_id, drill: |_| None, describe: checkout_description },
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

static CONVOY_ACTIONS: [ActionSpec<ConvoySummary>; 1] =
    [ActionSpec { id: "delete", label: "Delete convoy", key: 'd', resolve: delete_convoy }];

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

static ISSUE_COLUMNS: [ColumnSpec<ScopedIssueProjection>; 4] = [
    ColumnSpec {
        id: "issue",
        label: "ISSUE",
        width: WidthHint::Fixed(14),
        alignment: Alignment::Left,
        extract: |row| CellValue::plain(&row.row.reference.id),
    },
    ColumnSpec {
        id: "title",
        label: "TITLE",
        width: WidthHint::Flexible { minimum: 24, weight: 3 },
        alignment: Alignment::Left,
        extract: |row| CellValue::plain(&row.row.issue.title),
    },
    ColumnSpec {
        id: "source",
        label: "SOURCE",
        width: WidthHint::Flexible { minimum: 16, weight: 1 },
        alignment: Alignment::Left,
        extract: |row| CellValue::plain(format!("{} / {}", row.row.reference.source.service, row.row.reference.source.scope)),
    },
    ColumnSpec {
        id: "updated",
        label: "UPDATED",
        width: WidthHint::Fixed(20),
        alignment: Alignment::Left,
        extract: |row| CellValue::toned(row.row.issue.as_of.format("%Y-%m-%d %H:%M").to_string(), CellTone::Muted),
    },
];

static ISSUE_ACTIONS: [ActionSpec<ScopedIssueProjection>; 1] =
    [ActionSpec { id: "start", label: "Start convoy", key: 'c', resolve: start_scoped_issue }];

static CHECKOUT_COLUMNS: [ColumnSpec<CheckoutRow>; 5] = [
    ColumnSpec {
        id: "host",
        label: "HOST",
        width: WidthHint::Flexible { minimum: 10, weight: 1 },
        alignment: Alignment::Left,
        extract: |row| CellValue::plain(row.host.to_string()),
    },
    ColumnSpec {
        id: "path",
        label: "PATH",
        width: WidthHint::Flexible { minimum: 22, weight: 3 },
        alignment: Alignment::Left,
        extract: |row| CellValue::plain(&row.path),
    },
    ColumnSpec {
        id: "branch",
        label: "BRANCH",
        width: WidthHint::Flexible { minimum: 14, weight: 2 },
        alignment: Alignment::Left,
        extract: |row| CellValue::plain(&row.branch),
    },
    ColumnSpec {
        id: "repository",
        label: "REPOSITORY",
        width: WidthHint::Flexible { minimum: 14, weight: 2 },
        alignment: Alignment::Left,
        extract: |row| CellValue::plain(row.repo.to_string()),
    },
    ColumnSpec {
        id: "authority",
        label: "AUTHORITY",
        width: WidthHint::Fixed(10),
        alignment: Alignment::Left,
        extract: |row| CellValue::toned(row.authority.as_label_value(), CellTone::Muted),
    },
];

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

fn delete_convoy(row: &ConvoySummary) -> Option<TableIntent> {
    Some(TableIntent::DeleteConvoy { namespace: row.namespace.clone(), name: row.name.clone(), host: row.origin_host.clone() })
}

fn scoped_issue_id(row: &ScopedIssueProjection) -> RowId {
    RowId::new(format!(
        "issue:{}:{}:{}:{}:{}",
        row.scope.namespace, row.scope.name, row.row.reference.source.service, row.row.reference.source.scope, row.row.reference.id
    ))
}

fn scoped_issue_description(row: &ScopedIssueProjection) -> Vec<DetailField> {
    vec![
        DetailField { label: "Namespace", value: row.scope.namespace.clone() },
        DetailField { label: "Project", value: row.scope.name.clone() },
        DetailField { label: "Issue", value: row.row.reference.id.clone() },
        DetailField { label: "Title", value: row.row.issue.title.clone() },
        DetailField { label: "Source", value: format!("{} / {}", row.row.reference.source.service, row.row.reference.source.scope) },
        DetailField { label: "As of", value: row.row.issue.as_of.to_rfc3339() },
    ]
}

fn start_scoped_issue(row: &ScopedIssueProjection) -> Option<TableIntent> {
    Some(TableIntent::StartConvoy {
        namespace: row.scope.namespace.clone(),
        project: row.scope.name.clone(),
        issue: row.row.reference.clone(),
    })
}

fn checkout_id(row: &CheckoutRow) -> RowId {
    RowId::new(format!("checkout:{}/{}/{}/{}@{}", row.resource.api_version, row.resource.namespace, row.resource.name, row.path, row.host))
}

fn checkout_description(row: &CheckoutRow) -> Vec<DetailField> {
    vec![
        DetailField { label: "Host", value: row.host.to_string() },
        DetailField { label: "Path", value: row.path.clone() },
        DetailField { label: "Branch", value: row.branch.clone() },
        DetailField { label: "Repository", value: row.repo.to_string() },
        DetailField { label: "Authority", value: row.authority.as_label_value().to_string() },
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
    use flotilla_protocol::{
        test_support::TestIssue, DemandBackedMetadata, LifecycleAuthority, QueryId, QueryScope, RepositoryKey, ResourceRef,
        ResultSetCondition, ResultSetState,
    };

    use super::*;
    use crate::convoy_model::{ConvoyId, ProcessSummary, WorkCompletionTarget};

    fn project_convoys(address: &str, convoys: &[&ConvoySummary]) -> Result<TableView, String> {
        project(&address.parse().expect("valid address"), &TableRows { convoys: convoys.to_vec(), ..TableRows::default() })
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
            origin_host: None,
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
    fn convoy_projection_exposes_host_routed_delete_action() {
        let mut row = convoy(vec![]);
        row.origin_host = Some(HostName::new("kiwi"));
        let view = project_convoys("convoys/dev", &[&row]).expect("project table");

        assert_eq!(view.rows[0].actions, vec![AvailableAction {
            id: "delete",
            label: "Delete convoy",
            key: 'd',
            intent: TableIntent::DeleteConvoy { namespace: "dev".into(), name: "tables".into(), host: Some(HostName::new("kiwi")) },
        }]);
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
    fn vessel_address_scopes_rows_without_changing_the_widget_contract() {
        let in_project = convoy(vec![vessel("implement", &[], WorkPhase::Running), vessel("review", &["implement"], WorkPhase::Pending)]);
        let mut elsewhere = convoy(vec![]);
        elsewhere.id = ConvoyId::new("dev", "elsewhere");
        elsewhere.name = "elsewhere".into();
        elsewhere.project_ref = Some("other".into());

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
        let query = QueryId::Independents { scope: None };
        let state = ResultSetState::default();
        let view = project(&ViewAddress::Independents { scope: None }, &TableRows {
            independent_results: vec![QueryRows { query: &query, rows: &[unavailable, available], state: &state }],
            ..TableRows::default()
        })
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
    fn project_panels_compose_four_scoped_tables_with_independents_last() {
        let scope = QueryScope::new("flotilla", "roadmap");
        let mut convoy = convoy(vec![vessel("implement", &[], WorkPhase::Running)]);
        convoy.id = ConvoyId::new("flotilla", "tables");
        convoy.namespace = "flotilla".into();
        convoy.project_ref = Some("roadmap".into());
        let checkout = CheckoutRow::builder()
            .resource(ResourceRef::new("flotilla.work/v1", "Checkout", "flotilla", "roadmap"))
            .repo(RepositoryKey("repo_flotilla".into()))
            .path("/work/flotilla")
            .branch("main")
            .host(HostName::new("kiwi"))
            .authority(LifecycleAuthority::Observed)
            .build();
        let issue = TestIssue::new("Composite project issue").id("ENG-42").build();
        let issue_row = IssueRow { reference: issue.reference.clone(), issue };
        let independent = independent("governor", "feta", Some("terminal-governor"));
        let issue_query = QueryId::Issues { scope: scope.clone(), search: None };
        let checkout_query = QueryId::Checkouts { scope: Some(scope.clone()) };
        let independent_query = QueryId::Independents { scope: Some(scope.clone()) };
        let state = ResultSetState::default();
        let address = ViewAddress::Project { namespace: "flotilla".into(), name: "roadmap".into() };

        let panels = project_panels(&address, &TableRows {
            convoys: vec![&convoy],
            issue_results: vec![QueryRows { query: &issue_query, rows: std::slice::from_ref(&issue_row), state: &state }],
            checkout_results: vec![QueryRows { query: &checkout_query, rows: std::slice::from_ref(&checkout), state: &state }],
            independent_results: vec![QueryRows { query: &independent_query, rows: std::slice::from_ref(&independent), state: &state }],
            ..TableRows::default()
        })
        .expect("project panels");

        assert_eq!(panels.iter().map(|panel| panel.kind).collect::<Vec<_>>(), vec![
            ProjectPanelKind::Convoys,
            ProjectPanelKind::Checkouts,
            ProjectPanelKind::Issues,
            ProjectPanelKind::Independents,
        ]);
        assert_eq!(panels.iter().map(|panel| panel.target.to_string()).collect::<Vec<_>>(), vec![
            "convoys/flotilla",
            "checkouts?project=flotilla%2Froadmap",
            "issues?project=flotilla%2Froadmap",
            "independents?project=flotilla%2Froadmap",
        ]);
        assert_eq!(panels[0].table.rows[0].cells[0].text, "tables");
        assert_eq!(panels[1].table.rows[0].cells[1].text, "/work/flotilla");
        assert_eq!(panels[2].table.rows[0].actions[0].intent, TableIntent::StartConvoy {
            namespace: "flotilla".into(),
            project: "roadmap".into(),
            issue: issue_row.reference,
        });
        assert_eq!(panels[3].table.rows[0].actions[0].intent, TableIntent::AttachPane {
            reference: "terminal-governor".into(),
            host: HostName::new("feta"),
        });
    }

    #[test]
    fn project_panels_keep_available_content_while_scoped_results_are_loading() {
        let mut convoy = convoy(vec![vessel("implement", &[], WorkPhase::Running)]);
        convoy.id = ConvoyId::new("flotilla", "tables");
        convoy.namespace = "flotilla".into();
        convoy.project_ref = Some("roadmap".into());
        let address = ViewAddress::Project { namespace: "flotilla".into(), name: "roadmap".into() };

        let panels = project_panels(&address, &TableRows { convoys: vec![&convoy], ..TableRows::default() }).expect("project panels");

        assert_eq!(panels[0].table.rows[0].cells[0].text, "tables");
        assert_eq!(panels[1].table.meta.availability, TableAvailability::Loading);
        assert_eq!(panels[2].table.meta.availability, TableAvailability::Loading);
        assert_eq!(panels[3].table.meta.availability, TableAvailability::Loading);
    }

    #[test]
    fn scoped_issue_table_projects_rows_freshness_conditions_and_actions() {
        let issue = TestIssue::new("Start convoy from scoped issue").id("ENG-42").build();
        let row = IssueRow { reference: issue.reference.clone(), issue };
        let scope = QueryScope::new("flotilla", "roadmap");
        let query = QueryId::Issues { scope: scope.clone(), search: None };
        let state = ResultSetState {
            demand: Some(DemandBackedMetadata { as_of: "2026-07-20T12:00:00Z".parse().expect("timestamp"), has_more: true }),
            conditions: vec![ResultSetCondition::IssueSourceUnavailable { source: None, message: "one source is unavailable".into() }],
        };
        let address: ViewAddress = "issues?project=flotilla%2Froadmap".parse().expect("issues address");

        let view = project(&address, &TableRows {
            convoys: vec![],
            issue_results: vec![QueryRows { query: &query, rows: std::slice::from_ref(&row), state: &state }],
            ..TableRows::default()
        })
        .expect("scoped issue table");

        assert_eq!(view.rows.len(), 1);
        assert_eq!(view.rows[0].cells[0].text, "ENG-42");
        assert_eq!(view.rows[0].actions[0].intent, TableIntent::StartConvoy {
            namespace: "flotilla".into(),
            project: "roadmap".into(),
            issue: row.reference,
        });
        assert_eq!(view.meta.as_of, state.demand.as_ref().map(|metadata| metadata.as_of));
        assert!(view.meta.has_more);
        assert_eq!(view.meta.conditions, vec!["one source is unavailable"]);
    }

    #[test]
    fn fleet_checkout_table_projects_typed_rows() {
        let row = CheckoutRow::builder()
            .resource(ResourceRef::new("flotilla.work/v1", "Checkout", "flotilla", "widgets-api"))
            .repo(RepositoryKey("repo_widgets".into()))
            .path("/work/widgets-api")
            .branch("feature/scoped-tabs")
            .host(HostName::new("kiwi"))
            .authority(LifecycleAuthority::Observed)
            .build();
        let query = QueryId::Checkouts { scope: None };
        let state = ResultSetState::default();
        let view = project(&ViewAddress::Checkouts { scope: None }, &TableRows {
            checkout_results: vec![QueryRows { query: &query, rows: std::slice::from_ref(&row), state: &state }],
            ..TableRows::default()
        })
        .expect("checkout table");

        assert_eq!(view.columns.iter().map(|column| column.id).collect::<Vec<_>>(), vec![
            "host",
            "path",
            "branch",
            "repository",
            "authority",
        ]);
        assert_eq!(view.rows[0].cells.iter().map(|cell| cell.text.as_str()).collect::<Vec<_>>(), vec![
            "kiwi",
            "/work/widgets-api",
            "feature/scoped-tabs",
            "repo_widgets",
            "observed",
        ]);
        assert!(view.rows[0].actions.is_empty());
    }

    #[test]
    fn table_state_preserves_identity_and_clamps_when_rows_disappear() {
        let first = convoy(vec![]);
        let mut second = convoy(vec![]);
        second.id = ConvoyId::new("dev", "other");
        second.name = "other".into();
        let address = "convoys/dev".parse().expect("valid address");
        let view = project(&address, &TableRows { convoys: vec![&first, &second], ..TableRows::default() }).expect("project table");
        let mut state = TableState::default();
        state.reconcile(&view);
        state.select_delta(&view, 1);
        assert_eq!(state.selected_row(&view).map(|row| row.cells[0].text.as_str()), Some("other"));

        let changed = project(&address, &TableRows { convoys: vec![&first], ..TableRows::default() }).expect("project table");
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
        let view = project_convoys("convoys/dev", &[&failed, &active]).expect("project table").filtered("DSK F");

        assert_eq!(view.rows.len(), 1);
        assert_eq!(view.rows[0].cells[0].text, "tables");
    }
}
