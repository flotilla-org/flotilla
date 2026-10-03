//! Warning capture runs in its own executable because tracing's callsite
//! registry is process-wide and parallel projection tests register the same
//! callsite without a subscriber while this test installs a scoped capture.

use std::collections::BTreeMap;

use flotilla_manifest::{
    entity,
    projection::{project_catalog, CatalogInput, SubjectCatalogInput},
    recipe::FlotillaRecipes,
    wire::MetadataTarget,
};
use flotilla_protocol::{ConvoyPhase, ConvoyRow, IssueRef, IssueSource, ResourceRef};
use flotilla_resources::{ChangeRequest, Issue};

// Behaviour (ADR 0051): non-built-in uncovered services produce a structured
// warning once per service per catalog rebuild, keep subjects visible, and
// omit unresolved edges.
#[test]
fn uncovered_subject_service_warns_with_service() {
    use std::sync::{Arc, Mutex};

    use flotilla_protocol::{result_set::ConvoySubjectRow, Relationship, Subject, SubjectKind};
    use flotilla_resources::{ChangeRequestSpec, InMemoryBackend, InputMeta, IssueSpec, ResourceBackend};
    use tracing::{
        field::{Field, Visit},
        span::{Attributes, Id, Record},
        Event, Metadata, Subscriber,
    };

    #[derive(Clone, Default)]
    struct Warnings(Arc<Mutex<Vec<BTreeMap<String, String>>>>);
    struct Fields(BTreeMap<String, String>);
    impl Visit for Fields {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            self.0.insert(field.name().into(), format!("{value:?}"));
        }
        fn record_str(&mut self, field: &Field, value: &str) {
            self.0.insert(field.name().into(), value.into());
        }
    }
    // Captures the diagnostic output boundary; projection and storage are real.
    impl Subscriber for Warnings {
        fn enabled(&self, metadata: &Metadata<'_>) -> bool {
            *metadata.level() == tracing::Level::WARN
        }
        fn new_span(&self, _: &Attributes<'_>) -> Id {
            Id::from_u64(1)
        }
        fn record(&self, _: &Id, _: &Record<'_>) {}
        fn record_follows_from(&self, _: &Id, _: &Id) {}
        fn event(&self, event: &Event<'_>) {
            if *event.metadata().level() == tracing::Level::WARN {
                let mut fields = Fields(BTreeMap::new());
                event.record(&mut fields);
                self.0.lock().expect("warnings").push(fields.0);
            }
        }
        fn enter(&self, _: &Id) {}
        fn exit(&self, _: &Id) {}
    }
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let runtime = tokio::runtime::Builder::new_current_thread().build().expect("runtime");
    let observations = runtime.block_on(async {
        SubjectCatalogInput {
            change_requests: vec![backend
                .using::<ChangeRequest>("dev")
                .create(
                    &InputMeta::builder().name("cr".into()).build(),
                    &ChangeRequestSpec::builder()
                        .service("forge.example".into())
                        .scope("org/repo".into())
                        .number(42)
                        .observing_authority("kiwi".into())
                        .build(),
                )
                .await
                .expect("change request")],
            issues: vec![backend
                .using::<Issue>("dev")
                .create(
                    &InputMeta::builder().name("issue".into()).build(),
                    &IssueSpec::builder()
                        .service("forge.example".into())
                        .scope("org/repo".into())
                        .number(42)
                        .observing_authority("kiwi".into())
                        .build(),
                )
                .await
                .expect("issue")],
            ..Default::default()
        }
    });
    let source = IssueSource { service: "forge.example".into(), scope: "org/repo".into() };
    let mut convoy = ConvoyRow::builder()
        .resource(ResourceRef::new("flotilla/v1", "Convoy", "dev", "work"))
        .name("work")
        .workflow_ref("dev")
        .phase(ConvoyPhase::Active)
        .build();
    convoy.subjects = [SubjectKind::ChangeRequest, SubjectKind::Issue]
        .into_iter()
        .map(|kind| ConvoySubjectRow {
            subject: Subject { kind, source: source.clone(), id: "42".into() },
            relationship: Relationship::Produces,
            declared: false,
            short: "42".into(),
            url: None,
            repository_key: None,
        })
        .collect();
    let convoys = [convoy];
    let input = CatalogInput {
        subjects: Some(&observations),
        awareness: None,
        convoys: &convoys,
        independents: &[],
        standing_roles: &[],
        project_repositories: &[],
    };
    let warnings = Warnings::default();
    let patches = tracing::subscriber::with_default(warnings.clone(), || {
        project_catalog(&input, &FlotillaRecipes::new("flotilla")).reassert_patches()
    });
    let warnings = warnings.0.lock().expect("warnings");
    assert_eq!(warnings.len(), 1);
    assert_eq!(warnings[0]["service"], "forge.example");
    for target in [entity::change_request("forge.example", "org/repo", "42"), entity::issue(&IssueRef { source, id: "42".into() })] {
        let target = MetadataTarget::Entity(target);
        let patch = patches.iter().find(|patch| patch.target == target).expect("visible subject");
        assert!(!patch.set.contains_key("flotilla.forge"));
    }
}
