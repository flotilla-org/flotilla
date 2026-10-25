//! Generate the candidate schema from typed declarations without opening or accessing its tables.
use flotilla_provisioning::stored::{
    self, checkout::CheckoutRequest, clone::CloneRequest, environment::EnvironmentRequest, image_build::ImageBuildRequest,
    terminal_session::TerminalSessionRequest,
};
use flotilla_schema::{render_view, CodeEnum, Columns, KindRegistry, KindSpec, Layout, State, TableSet};
use flotilla_schema_sqlite::{schema_ddl, DdlMode};
use flotilla_state_machines::machines::{
    lifecycle::{schema_review::LifecycleState, LifecycleEdgeKind},
    vessel::VesselSpec,
};

// Reuse each real request's fields, indexes, kind/code and child tables; only its state declaration changes.
fn candidate<S: KindSpec>() -> TableSet {
    let mut layout = Layout::root(S::KIND, S::CODE);
    S::layout_row(&mut layout);
    State::<LifecycleState>::layout(&mut layout, "state", true);
    S::layout_indexes(&mut layout);
    layout.edges(LifecycleEdgeKind::CODES, LifecycleEdgeKind::NAMES);
    layout.finish()
}

fn candidate_registry(current: &KindRegistry) -> KindRegistry {
    let mut registry = KindRegistry::new();
    for set in current.iter() {
        let next = match set.kind.as_str() {
            "Checkout" => candidate::<CheckoutRequest>(),
            "Clone" => candidate::<CloneRequest>(),
            "Environment" => candidate::<EnvironmentRequest>(),
            "ImageBuild" => candidate::<ImageBuildRequest>(),
            "TerminalSession" => candidate::<TerminalSessionRequest>(),
            "Vessel" => candidate::<VesselSpec>(),
            _ => set.clone(),
        };
        registry.register_table_set(next).expect("candidate descriptor");
    }
    registry
}

/// Generated review artifacts share the production requests, but are never registered by the runtime.
pub fn artifacts() -> Vec<(String, String)> {
    let mut files = Vec::new();
    for (name, current) in [("provisioning", stored::registry().expect("registry")), ("crew", stored::crew_registry().expect("registry"))] {
        let registry = candidate_registry(&current);
        files.push((format!("{name}.sql"), schema_ddl(&registry, DdlMode::Create)));
        files.push((
            format!("{name}_schema_view.md"),
            format!(
                "# Candidate {name} store schema view\n\nGenerated from typed review declarations; not registered by the runtime.\n\n{}",
                render_view(registry.iter())
            ),
        ));
    }
    files
}
