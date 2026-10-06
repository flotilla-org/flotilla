//! Pre-roll gate: candidate catalog, exported CrewDefaults (manifest or list),
//! Projects list, and optionally the FleetDesignation manifest.
use std::{collections::BTreeMap, error::Error, fs};

use flotilla_resources::{
    crew_defaults::validate_catalog, role_cascade::validate_cascade_skills, CrewDefaultsSpec, FleetDesignationSpec, ProjectSpec,
    SkillCatalogEntry,
};

fn main() -> Result<(), Box<dyn Error>> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if !(3..=4).contains(&args.len()) {
        return Err("expected candidate catalog, CrewDefaults manifest/list, Project list, and optional FleetDesignation".into());
    }
    let catalog: Vec<SkillCatalogEntry> = serde_json::from_str(&fs::read_to_string(&args[0])?)?;
    let manifest = serde_json::from_str(&fs::read_to_string(std::path::Path::new(&args[0]).with_file_name(".flotilla-sources.json"))?)?;
    validate_catalog(&catalog, &manifest)?;
    let defaults: serde_json::Value = serde_json::from_str(&fs::read_to_string(&args[1])?)?;
    let default_documents = defaults.get("items").and_then(serde_json::Value::as_array).cloned().unwrap_or_else(|| vec![defaults]);
    let defaults = default_documents
        .iter()
        .map(|document| -> Result<_, Box<dyn Error>> {
            Ok((
                document["metadata"]["name"].as_str().unwrap_or("fleet").to_string(),
                serde_json::from_value::<CrewDefaultsSpec>(document.get("spec").unwrap_or(document).clone())?,
            ))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let projects: serde_json::Value = serde_json::from_str(&fs::read_to_string(&args[2])?)?;
    let projects = projects.get("items").and_then(serde_json::Value::as_array).ok_or("projects must be a resource list with items")?;
    let project_specs = projects
        .iter()
        .map(|project| -> Result<_, Box<dyn Error>> {
            Ok((
                project["metadata"]["name"].as_str().ok_or("project name missing")?.to_string(),
                serde_json::from_value::<ProjectSpec>(project.get("spec").ok_or("project spec missing")?.clone())?,
            ))
        })
        .collect::<Result<BTreeMap<_, _>, _>>()?;
    let fleet = args
        .get(3)
        .map(|path| -> Result<_, Box<dyn Error>> {
            let document: serde_json::Value = serde_json::from_str(&fs::read_to_string(path)?)?;
            Ok(serde_json::from_value::<FleetDesignationSpec>(document.get("spec").unwrap_or(&document).clone())?.project)
        })
        .transpose()?;
    validate_cascade_skills(&catalog, &project_specs, &defaults, fleet)?;
    println!("CrewDefaults and all {} registered projects resolve against the candidate catalog", projects.len());
    Ok(())
}
