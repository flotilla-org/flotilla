//! Global interaction availability for the active View.
//!
//! Key bindings, palette entries, and contextual affordances all ask this
//! module whether an interaction is meaningful in the current View. The
//! implementation behind an interaction may vary by View; that variation is
//! deliberately not part of the user-facing interaction vocabulary.

use flotilla_protocol::ViewAddress;

use crate::{keymap::Action, table_view::RowId};

/// The selected row, projected into an interaction-relevant resource family.
///
/// The resolver deliberately keeps this typed seam even before any current
/// interaction depends on selection. Future contextual actions can therefore
/// distinguish, for example, a selected Convoy from an Issue without
/// replacing a boolean with a new model later.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InteractionSelection<'a> {
    Convoy(&'a RowId),
    Independent(&'a RowId),
    Project(&'a RowId),
    Vessel(&'a RowId),
    Issue(&'a RowId),
    Checkout(&'a RowId),
}

impl<'a> InteractionSelection<'a> {
    fn for_active_view(active_view: Option<&'a ViewAddress>, selected_row: Option<&'a RowId>) -> Option<Self> {
        let selected_row = selected_row?;
        match active_view? {
            ViewAddress::Convoys { .. } => Some(Self::Convoy(selected_row)),
            ViewAddress::Independents { .. } => Some(Self::Independent(selected_row)),
            // Project tables currently contain both Convoys and Issues. The
            // view family is retained until their row projection exposes a
            // more specific resource kind.
            ViewAddress::Project { .. } => Some(Self::Project(selected_row)),
            ViewAddress::Convoy { .. } | ViewAddress::Vessel { .. } => Some(Self::Vessel(selected_row)),
            ViewAddress::Issues { .. } => Some(Self::Issue(selected_row)),
            ViewAddress::Checkouts { .. } => Some(Self::Checkout(selected_row)),
            ViewAddress::Overview | ViewAddress::Repo { .. } => None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct InteractionContext<'a> {
    active_view: Option<&'a ViewAddress>,
    selection: Option<InteractionSelection<'a>>,
    has_repo_context: bool,
}

impl<'a> InteractionContext<'a> {
    pub fn for_active_view(active_view: Option<&'a ViewAddress>, selected_row: Option<&'a RowId>, has_repo_context: bool) -> Self {
        Self { active_view, selection: InteractionSelection::for_active_view(active_view, selected_row), has_repo_context }
    }

    pub fn is_available(self, action: Action) -> bool {
        match action {
            Action::OpenFind => match self.active_view {
                Some(ViewAddress::Repo { .. }) => self.has_repo_context,
                Some(
                    ViewAddress::Convoys { .. }
                    | ViewAddress::Independents { .. }
                    | ViewAddress::Project { .. }
                    | ViewAddress::Convoy { .. }
                    | ViewAddress::Vessel { .. }
                    | ViewAddress::Issues { .. }
                    | ViewAddress::Checkouts { .. },
                ) => true,
                Some(ViewAddress::Overview) | None => false,
            },
            _ => true,
        }
    }

    pub fn has_selection(self) -> bool {
        self.selection.is_some()
    }

    pub fn selection(self) -> Option<InteractionSelection<'a>> {
        self.selection
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_is_available_on_repo_and_table_views_but_not_on_overview() {
        let repo: ViewAddress = "repo/github.com/flotilla-org/flotilla".parse().expect("repo address");
        let convoys: ViewAddress = "convoys/flotilla".parse().expect("table address");

        assert!(InteractionContext::for_active_view(Some(&repo), None, true).is_available(Action::OpenFind));
        assert!(!InteractionContext::for_active_view(Some(&repo), None, false).is_available(Action::OpenFind));
        assert!(InteractionContext::for_active_view(Some(&convoys), None, false).is_available(Action::OpenFind));
        assert!(!InteractionContext::for_active_view(Some(&ViewAddress::Overview), None, false).is_available(Action::OpenFind));
    }

    #[test]
    fn selection_retains_the_table_resource_family() {
        let convoy: ViewAddress = "convoys/flotilla".parse().expect("convoy address");
        let issue = ViewAddress::Issues { scope: flotilla_protocol::QueryScope::new("flotilla", "roadmap") };
        let row = RowId::new("row-1");

        assert!(matches!(
            InteractionContext::for_active_view(Some(&convoy), Some(&row), false).selection(),
            Some(InteractionSelection::Convoy(_))
        ));
        assert!(matches!(
            InteractionContext::for_active_view(Some(&issue), Some(&row), false).selection(),
            Some(InteractionSelection::Issue(_))
        ));
    }
}
