//! Per-kind, non-rendering View policy (ADR 0013), in one place.
//!
//! Adding a view kind means answering three questions here — which named
//! query it consumes, what convoy scope it implies, and which binding modes
//! it composes — plus a label in `widgets::tabs` and a rendering arm in
//! `widgets::screen` (rendering dispatch deliberately stays with the
//! widgets).

use flotilla_protocol::{AwarenessGrouping, AwarenessLimit, QueryId, QueryScope, ViewAddress};

use crate::binding_table::{BindingModeId, KeyBindingMode};

/// The named query a view kind consumes — the tab set IS the subscription
/// set. Repo views ride the Plane-A repo streams and return None.
pub(crate) fn queries(address: &ViewAddress, source_search: Option<&str>) -> Vec<QueryId> {
    match address {
        ViewAddress::Convoys { .. } | ViewAddress::Convoy { .. } | ViewAddress::Vessel { .. } => vec![QueryId::Convoys],
        ViewAddress::Project { namespace, name } => {
            vec![
                QueryId::Awareness {
                    scope: Some(QueryScope::new(namespace, name)),
                    grouping: AwarenessGrouping::Project,
                    limit: AwarenessLimit::default(),
                },
                QueryId::Convoys,
                QueryId::Checkouts { scope: Some(QueryScope::new(namespace, name)) },
                QueryId::Issues {
                    scope: QueryScope::new(namespace, name),
                    search: source_search.filter(|search| !search.is_empty()).map(str::to_owned),
                },
                QueryId::Independents { scope: Some(QueryScope::new(namespace, name)) },
            ]
        }
        ViewAddress::Independents { .. } | ViewAddress::Issues { .. } | ViewAddress::Checkouts { .. } => {
            vec![crate::table_view::query_for(address, source_search).expect("single-family table address has a query")]
        }
        ViewAddress::Overview | ViewAddress::Repo { .. } => vec![],
    }
}

/// The shell modes every page composes: app globals always; tab-management
/// keys only when the tab bar exists (never in scoped mode — ADR 0013).
pub(crate) fn shell_modes(scoped: bool) -> Vec<BindingModeId> {
    if scoped {
        vec![BindingModeId::TabPage]
    } else {
        vec![BindingModeId::TabPage, BindingModeId::TabShell]
    }
}

/// Compose the shell layer with a page's kind-level modes.
pub(crate) fn compose_with_shell(scoped: bool, kind_modes: impl IntoIterator<Item = BindingModeId>) -> KeyBindingMode {
    let mut modes = shell_modes(scoped);
    modes.extend(kind_modes);
    KeyBindingMode::Composed(modes)
}

/// The kind-level binding modes derived from the address alone. `None`
/// (broken/dangling tabs) composes only the shell. The overview and repo
/// pages carry widget state (e.g. an active search) — their `binding_mode()`
/// stays authoritative for status-bar hints; this function mirrors them for
/// key resolution at the base layer.
pub(crate) fn kind_modes(address: Option<&ViewAddress>) -> Vec<BindingModeId> {
    match address {
        Some(ViewAddress::Overview) => vec![BindingModeId::Overview],
        Some(
            ViewAddress::Convoys { .. }
            | ViewAddress::Independents { .. }
            | ViewAddress::Convoy { .. }
            | ViewAddress::Vessel { .. }
            | ViewAddress::Checkouts { .. },
        ) => {
            vec![BindingModeId::Convoys]
        }
        Some(ViewAddress::Project { .. }) => vec![BindingModeId::Convoys, BindingModeId::DemandTable, BindingModeId::Project],
        Some(ViewAddress::Issues { .. }) => vec![BindingModeId::Convoys, BindingModeId::DemandTable],
        Some(ViewAddress::Repo { .. }) => vec![BindingModeId::Normal],
        None => vec![],
    }
}

/// The full binding mode for the active View at the base layer.
pub(crate) fn binding_mode(address: Option<&ViewAddress>, scoped: bool) -> KeyBindingMode {
    compose_with_shell(scoped, kind_modes(address))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_view_composes_store_and_demand_backed_queries() {
        let address = ViewAddress::Project { namespace: "flotilla".into(), name: "roadmap".into() };
        assert_eq!(queries(&address, None), vec![
            QueryId::Awareness {
                scope: Some(QueryScope::new("flotilla", "roadmap")),
                grouping: AwarenessGrouping::Project,
                limit: AwarenessLimit::default(),
            },
            QueryId::Convoys,
            QueryId::Checkouts { scope: Some(QueryScope::new("flotilla", "roadmap")) },
            QueryId::Issues { scope: QueryScope::new("flotilla", "roadmap"), search: None },
            QueryId::Independents { scope: Some(QueryScope::new("flotilla", "roadmap")) },
        ]);
        assert_eq!(kind_modes(Some(&address)), vec![BindingModeId::Convoys, BindingModeId::DemandTable, BindingModeId::Project]);
    }

    #[test]
    fn scoped_issue_view_subscribes_to_its_ephemeral_search_window() {
        let address: ViewAddress = "issues?project=flotilla%2Froadmap".parse().expect("address");
        assert_eq!(queries(&address, Some("widget")), vec![QueryId::Issues {
            scope: QueryScope::new("flotilla", "roadmap"),
            search: Some("widget".into()),
        }]);
    }

    #[test]
    fn only_demand_backed_issue_tables_expose_source_search_and_fetch_more() {
        let issues: ViewAddress = "issues?project=flotilla%2Froadmap".parse().expect("issues address");
        let checkouts: ViewAddress = "checkouts".parse().expect("checkouts address");

        assert_eq!(kind_modes(Some(&issues)), vec![BindingModeId::Convoys, BindingModeId::DemandTable]);
        assert_eq!(kind_modes(Some(&checkouts)), vec![BindingModeId::Convoys]);
    }
}
