//! Convoy admission, placement planning, and project operations.
pub mod admission;
pub mod admission_actions;
pub mod convoy_admission;
pub mod convoy_ensure;
pub mod dispatch_missions;
pub mod image_build;
pub mod placement_policy;
pub mod ports;
pub mod project_image_layers;
pub mod project_ops;
pub mod provider_placement;
pub mod reconcilers;
pub mod regard_lifecycle;
pub mod repository_addressing;
pub mod repository_inspection;
pub mod support;

#[cfg(test)]
mod test_support;
