//! One cached projection per surface shape. Input events invalidate it; frames,
//! selection and animation reuse it. Navigation/search replace the sole entry,
//! so visiting addresses does not retain an unbounded history of tables.
use std::{cell::RefCell, rc::Rc};

use flotilla_protocol::ViewAddress;

use super::{table_rows, NamespaceMap, QueryTableCache, TuiModel};
use crate::table_view::{self, ProjectPanel, ProjectTableState, TableView};

type Entry<T> = Option<((ViewAddress, Option<String>), Result<Rc<T>, String>)>;

type Filtered<T> = Option<(Rc<T>, Vec<String>, Rc<T>)>;

#[derive(Default)]
pub(super) struct ProjectionCache {
    table: RefCell<Entry<TableView>>,
    panels: RefCell<Entry<Vec<ProjectPanel>>>,
    filtered_table: RefCell<Filtered<TableView>>,
    filtered_panels: RefCell<Filtered<Vec<ProjectPanel>>>,
}

impl ProjectionCache {
    pub(super) fn invalidate(&mut self) {
        *self.table.get_mut() = None;
        *self.panels.get_mut() = None;
        *self.filtered_table.get_mut() = None;
        *self.filtered_panels.get_mut() = None;
    }

    fn get<T>(
        &self,
        entry: &RefCell<Entry<T>>,
        address: &ViewAddress,
        search: Option<&str>,
        build: impl FnOnce() -> Result<T, String>,
    ) -> Result<Rc<T>, String> {
        let mut entry = entry.borrow_mut();
        let key = (address.clone(), search.map(str::to_owned));
        if entry.as_ref().is_none_or(|(cached, _)| cached != &key) {
            *entry = Some((key, build().map(Rc::new)));
        }
        entry.as_ref().expect("projection just populated").1.clone()
    }
    fn filtered<T>(&self, entry: &RefCell<Filtered<T>>, raw: Rc<T>, filters: Vec<String>, build: impl FnOnce(&T) -> T) -> Rc<T> {
        let mut entry = entry.borrow_mut();
        if entry.as_ref().is_none_or(|(cached, prior, _)| !Rc::ptr_eq(cached, &raw) || prior != &filters) {
            let result = Rc::new(build(&raw));
            *entry = Some((raw, filters, result));
        }
        entry.as_ref().expect("filtered projection just populated").2.clone()
    }
}

impl QueryTableCache {
    pub(crate) fn project_table(
        &self,
        address: &ViewAddress,
        model: &TuiModel,
        namespaces: &NamespaceMap,
        search: Option<&str>,
        filter: &str,
    ) -> Result<Rc<TableView>, String> {
        let raw = self
            .projection
            .get(&self.projection.table, address, search, || table_view::project(address, &table_rows(model, namespaces, self, search)))?;
        if filter.is_empty() {
            return Ok(raw);
        }
        Ok(self.projection.filtered(&self.projection.filtered_table, raw, vec![filter.to_owned()], |view| view.clone().filtered(filter)))
    }

    pub(crate) fn project_panels(
        &self,
        address: &ViewAddress,
        model: &TuiModel,
        namespaces: &NamespaceMap,
        state: &ProjectTableState,
    ) -> Result<Rc<Vec<ProjectPanel>>, String> {
        let search = state.issue_source_search();
        let raw = self.projection.get(&self.projection.panels, address, search, || {
            table_view::project_panels(address, &table_rows(model, namespaces, self, search))
        })?;
        let filters: Vec<_> = raw.iter().map(|panel| state.table(panel.kind).filter.clone()).collect();
        if filters.iter().all(String::is_empty) {
            return Ok(raw);
        }
        Ok(self.projection.filtered(&self.projection.filtered_panels, raw, filters, |panels| {
            panels
                .iter()
                .cloned()
                .map(|mut panel| {
                    panel.table = panel.table.filtered(&state.table(panel.kind).filter);
                    panel
                })
                .collect()
        }))
    }
}

#[cfg(test)]
mod tests {
    use flotilla_protocol::{DaemonEvent, QueryChanges, QueryScope, ResultDelta, ResultSetState};
    use ratatui::{backend::TestBackend, Terminal};

    use crate::{
        app::{test_support::stub_app, App, OpenViews},
        widgets::{InteractiveWidget, RenderContext},
    };

    fn render(app: &mut App, terminal: &mut Terminal<TestBackend>) {
        terminal
            .draw(|frame| {
                let mut ctx = RenderContext {
                    model: &app.model,
                    views: &mut app.views,
                    ui: &mut app.ui,
                    theme: &app.theme,
                    keymap: &app.keymap,
                    in_flight: &app.in_flight,
                    namespaces: &app.namespaces,
                    query_tables: &app.query_tables,
                };
                app.screen.render(frame, frame.area(), &mut ctx);
            })
            .expect("render");
    }

    // Frames, selection and filters do not change projection inputs. Authoritative
    // result deltas (including metadata-only deltas) invalidate cached tables.
    #[test]
    fn idle_frames_do_no_projection_work() {
        // Exhaust the small domain: both surface shapes, 1..8 repeated frames.
        for project in [false, true] {
            for frames in 1..=8 {
                crate::table_view::PROJECTION_CALLS.with(|count| count.set(0));
                let expected = if project { 4 } else { 1 };
                let mut app = stub_app();
                app.views =
                    OpenViews::scoped(if project { "project/flotilla/example" } else { "convoys/flotilla" }.parse().expect("address"));
                let mut terminal = Terminal::new(TestBackend::new(100, 30)).expect("terminal");
                render(&mut app, &mut terminal);
                assert_eq!(crate::table_view::PROJECTION_CALLS.with(|count| count.get()), expected);
                for _ in 0..frames {
                    render(&mut app, &mut terminal);
                    assert_eq!(
                        crate::table_view::PROJECTION_CALLS.with(|count| count.get()),
                        expected,
                        "idle frame must reuse the projection"
                    );
                }
                if project {
                    app.views.active_project_table_state_mut().table_mut(crate::table_view::ProjectPanelKind::Issues).filter =
                        "find".into();
                } else {
                    app.views.active_table_state_mut().filter = "find".into();
                }
                render(&mut app, &mut terminal);
                render(&mut app, &mut terminal);
                assert_eq!(
                    crate::table_view::PROJECTION_CALLS.with(|count| count.get()),
                    expected,
                    "filtering reuses raw projection inputs"
                );
                app.handle_daemon_event(DaemonEvent::ResultDelta(Box::new(ResultDelta {
                    seq: 1,
                    changes: QueryChanges::Issues {
                        scope: QueryScope::new("flotilla", "example"),
                        search: None,
                        label: None,
                        changed: vec![],
                        removed: vec![],
                    },
                    state: Some(ResultSetState::default()),
                })));
                render(&mut app, &mut terminal);
                assert_eq!(crate::table_view::PROJECTION_CALLS.with(|count| count.get()), expected * 2, "new input must project once");
                render(&mut app, &mut terminal);
                assert_eq!(crate::table_view::PROJECTION_CALLS.with(|count| count.get()), expected * 2);
            }
        }
    }
}
