use std::{collections::BTreeSet, fs, path::Path};

use clap::{error::ErrorKind, CommandFactory, FromArgMatches};
use flotilla_tui::cli::args::{Cli, SubCommand};

fn check_file(path: &Path) -> usize {
    let source = fs::read_to_string(path).expect("read stock skill reference");
    let mut count = 0;
    for (index, line) in source.lines().enumerate() {
        let line = line.trim();
        if !line.starts_with("flotilla ") {
            continue;
        }
        // Examples use single-token uppercase substitutions, so every documented
        // argument reaches clap unchanged without executing a shell or daemon.
        let args = line.split_whitespace().collect::<Vec<_>>();
        let result = Cli::command().try_get_matches_from(args);
        match result {
            Ok(matches) => {
                count += 1;
                let cli = Cli::from_arg_matches(&matches).expect("construct parsed CLI");
                if let Some(SubCommand::Domain(noun)) = cli.command {
                    noun.resolve().unwrap_or_else(|error| panic!("{}:{}: {line}: {error}", path.display(), index + 1));
                }
            }
            Err(error) if error.kind() == ErrorKind::DisplayHelp => {}
            Err(error) => panic!("{}:{}: {line}: {error}", path.display(), index + 1),
        }
    }
    count
}

// Issue #2982: every stock command example must agree with the real clap tree,
// including required arguments and value parsers. Check every area on disk so
// adding a reference extends coverage without maintaining a second command list.
// Keep examples as single-line commands in unindented fenced blocks, without
// list prefixes or shell continuations; this scanner does not join shell lines.
#[test]
fn stock_skill_commands_match_clap_tree() {
    let skill = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../skills/flotilla-commands");
    assert!(check_file(&skill.join("SKILL.md")) > 0, "entrypoint has crew essentials");
    for entry in fs::read_dir(skill.join("references")).expect("reference directory") {
        let path = entry.expect("reference file").path();
        if path.extension().is_some_and(|extension| extension == "md") {
            assert!(check_file(&path) > 0, "{} has executable examples", path.display());
        }
    }
}

// Issue #2982: the shipped project declaration selects both stock skills for
// ordinary crews and governors using the existing role cascade and resolver.
// Glue: these real collaborators have one declaration-to-selection path.
#[test]
fn project_selects_stock_skills_for_crews_and_governors() {
    use flotilla_core::project_declaration::parse_project_declaration;
    use flotilla_resources::{resolve_skills, ResolvedCascade, RoleCascadeLayer, SkillCatalogEntry};

    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let declaration = parse_project_declaration(&fs::read_to_string(root.join("project.yaml")).expect("project declaration"))
        .expect("valid project declaration");
    let cascade = ResolvedCascade::resolve(&[RoleCascadeLayer {
        name: declaration.name,
        workflow: declaration.default_workflow,
        roles: declaration.role_definitions,
        skills: declaration.skills,
    }]);
    let catalog = fs::read_dir(root.join("skills"))
        .expect("stock skills directory")
        .map(|entry| {
            let path = entry.expect("skill directory").path();
            let source = fs::read_to_string(path.join("SKILL.md")).expect("stock skill entrypoint");
            let mut lines = source.lines();
            assert_eq!(lines.next(), Some("---"), "stock skill has frontmatter");
            let name = lines
                .take_while(|line| *line != "---")
                .find_map(|line| line.strip_prefix("name: "))
                .expect("skill name in frontmatter")
                .to_string();
            SkillCatalogEntry {
                source: "flotilla".into(),
                repository: "flotilla-org/flotilla".into(),
                revision: "1".repeat(40),
                name,
                path: path.strip_prefix(&root).expect("repository skill path").to_string_lossy().replace('\\', "/"),
            }
        })
        .collect::<Vec<_>>();
    for role in ["coder", "reviewer", "governor"] {
        let selected = resolve_skills(&catalog, &cascade.skills_for(role, &[])).expect("resolve shipped selection");
        assert_eq!(
            selected.selected.iter().map(|entry| entry.name.as_str()).collect::<BTreeSet<_>>(),
            BTreeSet::from(["flotilla-commands", "crew-review"]),
            "stock selection for {role}"
        );
    }
}
