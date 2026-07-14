//! Per-kind, non-rendering View policy (ADR 0013), in one place.
//!
//! Adding a view kind means answering three questions here — which named
//! query it consumes, what convoy scope it implies, and which binding modes
//! it composes — plus a label in `widgets::tabs` and a rendering arm in
//! `widgets::screen` (rendering dispatch deliberately stays with the
//! widgets).

use flotilla_protocol::{QueryId, ViewAddress};

use crate::binding_table::{BindingModeId, KeyBindingMode};

/// The named query a view kind consumes — the tab set IS the subscription
/// set. Repo views ride the Plane-A repo streams and return None.
pub(crate) fn query(address: &ViewAddress) -> Option<QueryId> {
    match address {
        ViewAddress::Convoys { .. } | ViewAddress::Convoy { .. } | ViewAddress::Vessel { .. } | ViewAddress::Project { .. } => {
            Some(QueryId::Convoys)
        }
        ViewAddress::Independents => Some(QueryId::Independents),
        ViewAddress::Overview | ViewAddress::Repo(_) => None,
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
            | ViewAddress::Independents
            | ViewAddress::Project { .. }
            | ViewAddress::Convoy { .. }
            | ViewAddress::Vessel { .. },
        ) => {
            vec![BindingModeId::Convoys]
        }
        Some(ViewAddress::Repo(_)) => vec![BindingModeId::Normal],
        None => vec![],
    }
}

/// The full binding mode for the active View at the base layer.
pub(crate) fn binding_mode(address: Option<&ViewAddress>, scoped: bool) -> KeyBindingMode {
    compose_with_shell(scoped, kind_modes(address))
}
