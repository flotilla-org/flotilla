//! Pre-roll gate: use the candidate catalog and exported CrewDefaults/Projects.
//! `cargo run -p flotilla-resources --example check_crew_skills -- catalog.json defaults.json projects.json`
use std::{error::Error, fs};

use flotilla_resources::{
    crew_defaults::{check_skill_declarations, validate_catalog},
    CrewDefaultsSpec, ProjectSpec, SkillCatalogEntry,
};

fn main() -> Result<(), Box<dyn Error>> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.len() != 3 {
        return Err("expected candidate catalog, CrewDefaults manifest, and registered Project resource list".into());
    }
    let catalog: Vec<SkillCatalogEntry> = serde_json::from_str(&fs::read_to_string(&args[0])?)?;
    let manifest: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(std::path::Path::new(&args[0]).with_file_name(".flotilla-sources.json"))?)?;
    validate_catalog(&catalog, &manifest)?;
    let defaults: serde_json::Value = serde_json::from_str(&fs::read_to_string(&args[1])?)?;
    let defaults: CrewDefaultsSpec = serde_json::from_value(defaults.get("spec").unwrap_or(&defaults).clone())?;
    let projects: serde_json::Value = serde_json::from_str(&fs::read_to_string(&args[2])?)?;
    let projects = projects.get("items").and_then(serde_json::Value::as_array).ok_or("projects must be a resource list with items")?;
    let project_specs = projects
        .iter()
        .map(|project| {
            serde_json::from_value::<ProjectSpec>(project.get("spec").ok_or("project spec missing")?.clone())
                .map_err(|error| -> Box<dyn Error> { error.into() })
        })
        .collect::<Result<Vec<_>, Box<dyn Error>>>()?;
    check_skill_declarations(&catalog, &defaults, &project_specs)?;
    println!("CrewDefaults and all {} registered projects resolve against the candidate catalog", projects.len());
    Ok(())
}
