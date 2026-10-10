use std::collections::BTreeMap;

use flotilla_resources::role_cascade::*;

use crate::ResolvedCascadeStoreExt;
use crate::*;

#[cfg(test)]
mod tests {
    use hegel::generators as gs;

    use super::*;
    use crate::{FleetDesignation, FleetDesignationSpec, InMemoryBackend, InputMeta};

    // #2719: nearest declared value wins independently per field, with the
    // winning layer named. Generate omissions at all four override positions.
    #[hegel::test]
    fn nearest_declared_role_setting_wins(tc: hegel::TestCase) {
        let mut layers = vec![RoleCascadeLayer {
            name: "fleet".into(),
            workflow: Some("work".into()),
            skills: BTreeMap::new(),
            roles: BTreeMap::from([(
                "governor".into(),
                RoleDefinition {
                    agent: Some("claude".into()),
                    model: Some("fleet".into()),
                    workflow: Some("govern".into()),
                    brief_template: Some("fleet template".into()),
                    ..Default::default()
                },
            )]),
        }];
        let mut winner = "fleet";
        for name in ["parent", "project", "convoy", "dispatch"] {
            let model = tc.draw(gs::booleans()).then(|| name.to_string());
            if model.is_some() {
                winner = name;
            }
            layers.push(RoleCascadeLayer {
                name: name.into(),
                workflow: None,
                skills: BTreeMap::new(),
                roles: BTreeMap::from([("governor".into(), RoleDefinition { model, ..Default::default() })]),
            });
        }
        let resolved = ResolvedCascade::resolve(&layers);
        assert_eq!(resolved.roles["governor"].model.as_deref(), Some(winner));
        assert_eq!(resolved.settings["roles.governor.model"].layer, winner);
        assert_eq!(resolved.roles["governor"].agent.as_deref(), Some("claude"));
        assert_eq!(resolved.workflow(Some("governor")).value, "govern");
        assert_eq!(resolved.roles["governor"].brief_template.as_deref(), Some("fleet template"));
        assert!(resolved.charter.is_empty(), "role shape creates no local charter or presence");
    }

    // #2719: a fleet layer reaches an implicit child, ancestors beat the fleet,
    // and local content stays local. Real in-memory Definitions exercise the
    // shared graph rather than a duplicate parent-walk implementation.
    #[tokio::test]
    async fn hierarchy_load_inherits_shape_but_not_contents() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let projects = backend.definitions::<Project>("test");
        let fleet = ProjectSpec::builder()
            .display_name("Fleet".into())
            .role_definitions(BTreeMap::from([(
                "governor".into(),
                RoleDefinition {
                    agent: Some("claude".into()),
                    model: Some("fleet".into()),
                    workflow: Some("govern".into()),
                    brief_template: Some("fleet template".into()),
                    ..Default::default()
                },
            )]))
            .charter_prose(BTreeMap::from([("*".into(), "Fleet-only prose".into())]))
            .build();
        projects.apply(&InputMeta::builder().name("fleet".into()).build(), &fleet).await.expect("fleet");
        backend
            .definitions::<FleetDesignation>("test")
            .apply(&InputMeta::builder().name("fleet".into()).build(), &FleetDesignationSpec { project: "fleet".into(), image_cache: None })
            .await
            .expect("designation");
        let mut child = ProjectSpec::builder().display_name("Child".into()).build();
        projects.apply(&InputMeta::builder().name("child".into()).build(), &child).await.expect("child");
        let inherited = ResolvedCascade::load(&backend, "test", "child", &child).await.expect("cascade");
        assert_eq!(inherited.roles["governor"].model.as_deref(), Some("fleet"));
        assert_eq!(inherited.settings["roles.governor.model"].layer, "project:fleet");
        assert!(inherited.charter.is_empty());
        let parent = ProjectSpec::builder()
            .display_name("Parent".into())
            .role_definitions(BTreeMap::from([("governor".into(), RoleDefinition { model: Some("parent".into()), ..Default::default() })]))
            .build();
        projects.apply(&InputMeta::builder().name("parent".into()).build(), &parent).await.expect("parent");
        child.parent = Some("parent".into());
        // #2721's source pointer and #2719's delivered prose coexist; the
        // cascade must neither reinterpret nor overwrite source authority.
        child.charter = Some(crate::CharterPointer::Repository {
            repo: "https://github.com/example/ops".into(),
            branch: "main".into(),
            path: "charters/child".into(),
        });
        child.charter_prose.insert("governor".into(), "Child charter".into());
        let encoded = serde_json::to_value(&child).expect("serialize Project");
        let decoded: ProjectSpec = serde_json::from_value(encoded).expect("decode Project");
        assert_eq!(decoded.charter, child.charter);
        assert_eq!(decoded.charter_prose, child.charter_prose);
        let meta = InputMeta::builder()
            .name("child".into())
            .annotations(BTreeMap::from([("flotilla.work/project-bootstrap-commit".into(), "abc123".into())]))
            .build();
        projects.apply(&meta, &child).await.expect("parented child");
        let stale = ProjectSpec::builder().display_name("Stale child".into()).build();
        let inherited = ResolvedCascade::load(&backend, "test", "child", &stale).await.expect("cascade");
        assert_eq!(inherited.roles["governor"].model.as_deref(), Some("parent"));
        assert_eq!(inherited.settings["roles.governor.model"].layer, "project:parent");
        assert_eq!(inherited.charter["governor"], "Child charter");
        let stored = projects.get("child").await.expect("stored Project");
        assert_eq!(stored.spec.charter, child.charter);
        assert_eq!(stored.spec.charter_prose, child.charter_prose);
        assert_eq!(inherited.charter_commit.as_deref(), Some("abc123"));
        child.role_definitions.insert("governor".into(), RoleDefinition { model: Some("child".into()), ..Default::default() });
        projects.apply(&meta, &child).await.expect("override");
        let local = ResolvedCascade::load(&backend, "test", "child", &child).await.expect("cascade");
        assert_eq!(local.roles["governor"].model.as_deref(), Some("child"));
        assert_eq!(local.settings["roles.governor.model"].layer, "project:child");
    }

    // #2719 and ADR 0052: imports/removals run wildcard before role at each
    // ancestor. Generate parent and dispatch removals to cover re-addition.
    #[hegel::test]
    fn ancestor_skill_layers_keep_order_and_provenance(tc: hegel::TestCase) {
        let parent_removes = tc.draw(gs::booleans());
        let project_adds = tc.draw(gs::booleans());
        let dispatch_removes = tc.draw(gs::booleans());
        let layer = |name: &str, skills| RoleCascadeLayer { name: name.into(), workflow: None, roles: BTreeMap::new(), skills };
        let cascade = ResolvedCascade::resolve(&[
            layer("fleet", BTreeMap::from([("*".into(), vec!["testing".into()])])),
            layer("parent", BTreeMap::from([("coder".into(), if parent_removes { vec!["-testing".into()] } else { Vec::new() })])),
            layer("project", BTreeMap::from([("*".into(), if project_adds { vec!["testing".into()] } else { Vec::new() })])),
        ]);
        let catalog = vec![crate::SkillCatalogEntry::builder()
            .source("test".into())
            .repository("test/repo".into())
            .revision("1".repeat(40))
            .name("testing".into())
            .path("skills/testing".into())
            .build()];
        let dispatch = if dispatch_removes { vec!["-testing".into()] } else { Vec::new() };
        let resolved = crate::resolve_skills(&catalog, &cascade.skills_for("coder", &dispatch)).expect("resolve");
        assert_eq!(!resolved.selected.is_empty(), (!parent_removes || project_adds) && !dispatch_removes);
        if parent_removes {
            assert!(resolved
                .provenance
                .iter()
                .any(|decision| decision.layer == SkillLayer::Cascade { project: "parent".into(), role: "coder".into() }
                    && decision.outcome == crate::SkillOutcome::Removed));
        }
        let other_role = crate::resolve_skills(&catalog, &cascade.skills_for("governor", &[])).expect("other role");
        assert_eq!(other_role.selected.len(), 1, "parent coder removal must not affect governors");
    }

    // Pre-roll uses the same resolver: missing ancestor imports refuse the
    // candidate, while defaults bound to distinct projects may coexist.
    #[test]
    fn candidate_validation_accepts_distinct_bound_defaults_and_uses_ancestors() {
        let projects = BTreeMap::from([
            ("fleet".into(), ProjectSpec::builder().display_name("Fleet".into()).build()),
            (
                "parent".into(),
                ProjectSpec::builder()
                    .display_name("Parent".into())
                    .skills(BTreeMap::from([("coder".into(), vec!["missing".into()])]))
                    .build(),
            ),
            ("child".into(), ProjectSpec::builder().display_name("Child".into()).parent("parent".into()).build()),
        ]);
        let defaults = vec![
            ("root".into(), CrewDefaultsSpec::default()),
            ("parent-defaults".into(), CrewDefaultsSpec::builder().project_ref("parent".into()).build()),
        ];
        let error = validate_cascade_skills(&[], &projects, &defaults, Some("fleet".into())).expect_err("ancestor missing import");
        assert!(error.contains("missing") && error.contains("project:parent"));
        let mut valid = projects;
        valid.get_mut("parent").expect("parent").skills.clear();
        validate_cascade_skills(&[], &valid, &defaults, Some("fleet".into())).expect("distinct layers");
    }

    // Runtime isolates unrelated layers; candidate validation still checks all
    // bindings and duplicates, including a sibling not in the admitted chain.
    #[test]
    fn unrelated_defaults_do_not_block_admission_but_candidate_refuses_them() {
        let project = ProjectSpec::builder().display_name("Child".into()).build();
        let projects = BTreeMap::from([("child".into(), project.clone()), ("sibling".into(), project.clone())]);
        let hierarchy = ProjectHierarchy::new(BTreeMap::from([("child".into(), None), ("sibling".into(), None)]), None).unwrap();
        let mut defaults = vec![
            (
                "sibling-one".into(),
                CrewDefaultsSpec::builder().project_ref("sibling".into()).default_workflow_ref("sibling-work".into()).build(),
            ),
            ("sibling-two".into(), CrewDefaultsSpec::builder().project_ref("sibling".into()).build()),
        ];
        let resolved =
            ResolvedCascade::from_specs(&hierarchy, &projects, &defaults, "child", &project).expect("unrelated duplicates ignored");
        assert_eq!(resolved.workflow(None).value, "single-agent");
        assert!(validate_cascade_skills(&[], &projects, &defaults, None).unwrap_err().contains("at most one CrewDefaults"));
        defaults.pop();
        defaults.push(("stray".into(), CrewDefaultsSpec::builder().project_ref("undeclared".into()).build()));
        ResolvedCascade::from_specs(&hierarchy, &projects, &defaults, "child", &project).expect("stray outside chain ignored");
        assert!(validate_cascade_skills(&[], &projects, &defaults, None).unwrap_err().contains("undeclared Project"));
    }

    // A fleet designation cannot validate without its declared fleet Project;
    // empty pre-bootstrap input without a designation remains valid.
    #[test]
    fn candidate_empty_projects_with_fleet_refuses_missing_root() {
        validate_cascade_skills(&[], &BTreeMap::new(), &[], None).expect("pre-bootstrap");
        assert!(validate_cascade_skills(&[], &BTreeMap::new(), &[], Some("fleet".into())).unwrap_err().contains("not declared"));
    }

    // Empty and duplicate layers: builtin workflow is usable before bootstrap;
    // multiple defaults bound to the same project are refused, never order-selected.
    #[tokio::test]
    async fn duplicate_defaults_are_refused_and_empty_cascade_has_builtin() {
        assert_eq!(ResolvedCascade::default().workflow(None).value, "single-agent", "Default also seeds the builtin workflow");
        assert_eq!(ResolvedCascade::resolve(&[]).workflow(None).value, "single-agent");
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let project = ProjectSpec::builder().display_name("Local".into()).build();
        for name in ["one", "two"] {
            backend
                .definitions::<CrewDefaults>("test")
                .apply(&InputMeta::builder().name(name.into()).build(), &CrewDefaultsSpec::default())
                .await
                .expect("defaults");
        }
        assert!(ResolvedCascade::load(&backend, "test", "local", &project)
            .await
            .expect_err("duplicate refusal")
            .to_string()
            .contains("at most one CrewDefaults"));
    }
}
