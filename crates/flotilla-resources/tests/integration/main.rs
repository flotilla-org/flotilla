// Keep integration coverage in one executable to avoid linking the crate graph per module.
// Tests share a process: isolate state and keep process-wide mutations in separate targets.
#[path = "../common/mod.rs"]
mod common;
mod controller_loop;
mod convoy_in_memory;
mod convoy_reconcile;
mod convoy_status_patch;
mod crew_image_baseline;
mod digest;
mod field_ownership;
mod forge_identity;
mod fulfilment_kind;
mod http_wire;
mod in_memory;
mod k8s_integration;
mod landing_gate;
mod lifecycle_status_patch;
mod manifest_root;
mod message;
mod owner_gc;
mod placement_tiebreak_capacity_wait;
mod platform_vocabulary;
mod prepared_snapshot_gc;
mod principal_attention_status_patch;
mod provisioning_http_wire;
mod provisioning_resources_in_memory;
mod provisioning_status_patch;
mod repository_in_memory;
mod resource_projection;
mod review_bundle;
mod review_bundle_aggregator;
mod sqlite;
mod status_patch;
mod stored_corpus;
#[path = "../../../../tests/support/integration_modules.rs"]
mod test_module_registration;
mod workflow_template_in_memory;
mod workflow_template_validation;

mod image_layers;
mod project_hierarchy;
