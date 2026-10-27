//! Built-in definitions come from the code (#3824 layer 2a, owner ruling 2026-10-26): the default
//! workflow templates and the stdlib image layers are read-only, versioned with the binary and
//! never stored. Readers see them beside the stored definitions; a stored definition of a
//! built-in's name is refused, and a stored copy an earlier generation seeded is shadowed until
//! the import drops it. A project wanting a variant declares its own definition under a new name.

use std::{collections::BTreeMap, sync::OnceLock};

use serde_json::Value;

use crate::{builtin_workflow_templates, stdlib::stdlib_image_layers, ImageLayerResourceSpec, MANAGED_BY_LABEL};

/// The `managed-by` label a built-in workflow template carries.
pub const BUILTIN_MANAGER: &str = "builtin";
/// The `managed-by` label a stdlib image layer carries.
pub const STDLIB_MANAGER: &str = "stdlib";

/// One built-in definition: its resource name, its labels and its spec as the store reads it.
#[derive(Clone, Debug, PartialEq)]
pub struct BuiltinDefinition {
    pub name: String,
    pub labels: BTreeMap<String, String>,
    pub spec: Value,
}

fn definition(name: String, manager: &str, spec: Value) -> BuiltinDefinition {
    BuiltinDefinition { name, labels: BTreeMap::from([(MANAGED_BY_LABEL.to_string(), manager.to_string())]), spec }
}

/// The built-in definitions of `kind` (by its API kind name), in name order; none for a kind with
/// no built-ins. Refused with the cause when the release's built-ins of the kind do not load.
pub fn builtin_definitions(kind: &str) -> Result<&'static [BuiltinDefinition], String> {
    static TEMPLATES: OnceLock<Vec<BuiltinDefinition>> = OnceLock::new();
    static LAYERS: OnceLock<Vec<BuiltinDefinition>> = OnceLock::new();
    match kind {
        "WorkflowTemplate" => Ok(TEMPLATES.get_or_init(|| {
            let mut templates: Vec<_> = builtin_workflow_templates()
                .into_iter()
                .map(|(name, spec)| {
                    definition(name.to_string(), BUILTIN_MANAGER, serde_json::to_value(spec).expect("a template serializes"))
                })
                .collect();
            templates.sort_by(|left, right| left.name.cmp(&right.name));
            templates
        })),
        "ImageLayer" => {
            let stdlib = stdlib_image_layers().map_err(|error| format!("the shipped stdlib image layers do not parse: {error}"))?;
            Ok(LAYERS.get_or_init(|| {
                let mut layers: Vec<_> = stdlib
                    .iter()
                    .map(|layer| {
                        let spec = ImageLayerResourceSpec::from(layer.clone());
                        definition(layer.resource_name(), STDLIB_MANAGER, serde_json::to_value(spec).expect("a layer serializes"))
                    })
                    .collect();
                layers.sort_by(|left, right| left.name.cmp(&right.name));
                layers
            }))
        }
        _ => Ok(&[]),
    }
}

/// Whether `name` is a built-in definition of `kind`. A stdlib layer's name is one whatever the
/// release's layers load, so a write can never take one.
pub fn is_builtin(kind: &str, name: &str) -> bool {
    match builtin_definitions(kind) {
        Ok(definitions) => definitions.iter().any(|definition| definition.name == name),
        Err(_) => kind == "ImageLayer" && name.starts_with("stdlib-"),
    }
}
