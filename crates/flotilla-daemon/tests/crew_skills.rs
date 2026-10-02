//! Generated scenarios for the declaration cascade, with an oracle from policy.
use std::collections::BTreeMap;

use flotilla_resources::{resolve_skills, skill_layers, CrewDefaultsSpec, SkillCatalogEntry};
use hegel::generators as gs;

#[hegel::test]
fn project_removal_never_leaks_back_through_other_role_defaults(tc: hegel::TestCase) {
    // Intended: explicit removal wins over fleet and role imports, while dispatch
    // can re-import a removed skill. Another role's declarations never leak in.
    let role = tc.draw(gs::integers::<usize>().min_value(0).max_value(2));
    let reimport = tc.draw(gs::integers::<usize>().min_value(0).max_value(1)) == 1;
    let roles = ["coder", "governor", "reviewer"];
    let catalog = ["testing", "research", "implement", "wayfinder", "review"]
        .into_iter()
        .map(|name| SkillCatalogEntry {
            source: "source".into(),
            repository: "owner/repo".into(),
            revision: "1".repeat(40),
            name: name.into(),
            path: format!("skills/{name}"),
        })
        .collect::<Vec<_>>();
    let defaults = CrewDefaultsSpec {
        skills: BTreeMap::from([
            ("*".into(), vec!["research".into(), "testing".into()]),
            ("coder".into(), vec!["implement".into()]),
            ("governor".into(), vec!["wayfinder".into()]),
            ("reviewer".into(), vec!["review".into()]),
        ]),
    };
    let project = BTreeMap::from([(roles[role].into(), vec!["-testing".into()])]);
    let dispatch = if reimport { vec!["testing".into()] } else { Vec::new() };
    let selected = resolve_skills(&catalog, &skill_layers(&defaults, &project, roles[role], &dispatch)).expect("valid generated policy");
    let names = selected.selected.iter().map(|entry| entry.name.as_str()).collect::<std::collections::BTreeSet<_>>();
    let mut expected = std::collections::BTreeSet::from(["research", ["implement", "wayfinder", "review"][role]]);
    if reimport {
        expected.insert("testing");
    }
    assert_eq!(names, expected);
}
