//! The controller owns scenario registration; core supplies private daemon setup.

use std::sync::Arc;

use super::EnsureReconciler;
use async_trait::async_trait;
use flotilla_core::convoy_ensure::ConvoyEnsureReconciler;
use flotilla_core::in_process::InProcessDaemon;
use flotilla_orchestration_testkit as ensure_scenarios;
use flotilla_orchestration_testkit::EnsureScenarioController;
use flotilla_resources::{Clock, ConvoyEnsure, ResourceBackend, ResourceObject};

struct Controller;
#[async_trait]
impl EnsureScenarioController for Controller {
    fn create(&self, backend: ResourceBackend, clock: Arc<dyn Clock>) -> Arc<dyn ConvoyEnsureReconciler> {
        Arc::new(EnsureReconciler::builder().resource_backend(backend).clock(clock).build())
    }
    async fn dependency_hash(
        &self,
        daemon: &InProcessDaemon,
        namespace: &str,
        ensure: &ResourceObject<ConvoyEnsure>,
    ) -> Result<String, String> {
        EnsureReconciler::builder()
            .resource_backend(daemon.resource_backend())
            .clock(daemon.clock_for_scenarios())
            .build()
            .ensure_admission_dependency_hash(daemon, namespace, ensure)
            .await
    }
    async fn start(&self, daemon: &InProcessDaemon, namespace: &str, ensure: &ResourceObject<ConvoyEnsure>) -> Result<String, String> {
        EnsureReconciler::builder()
            .resource_backend(daemon.resource_backend())
            .clock(daemon.clock_for_scenarios())
            .build()
            .start_ensured_convoy(daemon, namespace, ensure)
            .await
    }
}

#[tokio::test]
async fn abandoned_ensure_generation_survives_a_stale_reconcile_write_and_is_superseded() {
    ensure_scenarios::abandoned_ensure_generation_survives_a_stale_reconcile_write_and_is_superseded(&Controller).await;
}

#[tokio::test]
async fn admission_refuses_same_role_briefs_before_writing_any_artifact() {
    ensure_scenarios::admission_refuses_same_role_briefs_before_writing_any_artifact(&Controller).await;
}

#[tokio::test]
async fn admission_rejects_agent_roles_reused_across_vessels_before_writing_briefs() {
    ensure_scenarios::admission_rejects_agent_roles_reused_across_vessels_before_writing_briefs(&Controller).await;
}

#[tokio::test]
async fn changed_ensure_driver_waits_for_operator_roll_of_the_running_remote_generation() {
    ensure_scenarios::changed_ensure_driver_waits_for_operator_roll_of_the_running_remote_generation(&Controller).await;
}

#[tokio::test]
async fn changing_ensure_config_starts_a_fresh_retry_episode() {
    ensure_scenarios::changing_ensure_config_starts_a_fresh_retry_episode(&Controller).await;
}

#[tokio::test]
async fn concurrent_convoy_phase_change_prevents_operator_abandonment() {
    ensure_scenarios::concurrent_convoy_phase_change_prevents_operator_abandonment(&Controller).await;
}

#[tokio::test]
async fn concurrent_periodic_and_explicit_ensure_admission_creates_only_one_live_generation() {
    ensure_scenarios::concurrent_periodic_and_explicit_ensure_admission_creates_only_one_live_generation(&Controller).await;
}

#[tokio::test]
async fn convoy_explain_addresses_an_exact_terminal_pre_identity_record() {
    ensure_scenarios::convoy_explain_addresses_an_exact_terminal_pre_identity_record(&Controller).await;
}

#[tokio::test]
async fn convoy_explain_refuses_multiple_terminal_generations() {
    ensure_scenarios::convoy_explain_refuses_multiple_terminal_generations(&Controller).await;
}

#[tokio::test]
async fn convoy_explain_rejects_projectless_and_project_bound_role_ambiguity() {
    ensure_scenarios::convoy_explain_rejects_projectless_and_project_bound_role_ambiguity(&Controller).await;
}

#[tokio::test]
async fn convoy_explain_surfaces_queued_turn_age_and_blocker() {
    ensure_scenarios::convoy_explain_surfaces_queued_turn_age_and_blocker(&Controller).await;
}

#[tokio::test]
async fn convoy_routing_does_not_treat_a_legacy_display_name_as_role_identity() {
    ensure_scenarios::convoy_routing_does_not_treat_a_legacy_display_name_as_role_identity(&Controller).await;
}

#[tokio::test]
async fn convoy_routing_falls_back_to_a_unique_terminal_generation_and_refuses_multiple() {
    ensure_scenarios::convoy_routing_falls_back_to_a_unique_terminal_generation_and_refuses_multiple(&Controller).await;
}

#[tokio::test]
async fn convoy_routing_prefers_an_exact_terminal_pre_identity_record() {
    ensure_scenarios::convoy_routing_prefers_an_exact_terminal_pre_identity_record(&Controller).await;
}

#[tokio::test]
async fn convoy_teardown_removes_its_managed_children() {
    ensure_scenarios::convoy_teardown_removes_its_managed_children(&Controller).await;
}

#[tokio::test]
async fn declaration_refusal_staleness_uses_the_daemon_clock_at_the_24_hour_boundary() {
    ensure_scenarios::declaration_refusal_staleness_uses_the_daemon_clock_at_the_24_hour_boundary(&Controller).await;
}

#[tokio::test]
async fn declared_driver_admission_refusals_retry_indefinitely_without_strikes_or_human_gate() {
    ensure_scenarios::declared_driver_admission_refusals_retry_indefinitely_without_strikes_or_human_gate(&Controller).await;
}

#[tokio::test]
async fn declared_driver_derives_bounded_backoff_from_its_homed_generations() {
    ensure_scenarios::declared_driver_derives_bounded_backoff_from_its_homed_generations(&Controller).await;
}

#[tokio::test]
async fn driver_reconcile_now_acknowledges_recordless_teardown_and_readmits_the_ensure() {
    ensure_scenarios::driver_reconcile_now_acknowledges_recordless_teardown_and_readmits_the_ensure(&Controller).await;
}

#[tokio::test]
async fn duplicate_operational_entry_refusal_records_a_project_event() {
    ensure_scenarios::duplicate_operational_entry_refusal_records_a_project_event(&Controller).await;
}

#[tokio::test]
async fn ensure_dependency_fingerprint_tracks_replicated_placement_changes() {
    ensure_scenarios::ensure_dependency_fingerprint_tracks_replicated_placement_changes(&Controller).await;
}

#[tokio::test]
async fn ensure_drift_names_config_changes_and_invalid_roll_keeps_the_current_generation() {
    ensure_scenarios::ensure_drift_names_config_changes_and_invalid_roll_keeps_the_current_generation(&Controller).await;
}

#[tokio::test]
async fn ensure_reconciliation_recovers_a_roll_interrupted_after_retirement() {
    ensure_scenarios::ensure_reconciliation_recovers_a_roll_interrupted_after_retirement(&Controller).await;
}

#[tokio::test]
async fn ensure_roll_targets_the_running_convoy_home_over_an_explicit_other_host() {
    ensure_scenarios::ensure_roll_targets_the_running_convoy_home_over_an_explicit_other_host(&Controller).await;
}

#[tokio::test]
async fn explicit_ensure_roll_readmits_current_config_and_retains_the_previous_generation() {
    ensure_scenarios::explicit_ensure_roll_readmits_current_config_and_retains_the_previous_generation(&Controller).await;
}

#[tokio::test]
async fn forced_convoy_delete_retains_force_intent_until_checkout_finalizes() {
    ensure_scenarios::forced_convoy_delete_retains_force_intent_until_checkout_finalizes(&Controller).await;
}

#[tokio::test]
async fn foreign_statusless_generation_at_ensure_address_blocks_admission_without_labels() {
    ensure_scenarios::foreign_statusless_generation_at_ensure_address_blocks_admission_without_labels(&Controller).await;
}

#[tokio::test]
async fn generation_allocation_sees_live_and_terminal_replicated_convoys() {
    ensure_scenarios::generation_allocation_sees_live_and_terminal_replicated_convoys(&Controller).await;
}

#[tokio::test]
async fn gone_worktree_satisfies_teardown_gate_without_integration_observation() {
    ensure_scenarios::gone_worktree_satisfies_teardown_gate_without_integration_observation(&Controller).await;
}

#[tokio::test]
async fn off_home_driver_admits_an_ensure_from_replicated_project_definitions() {
    ensure_scenarios::off_home_driver_admits_an_ensure_from_replicated_project_definitions(&Controller).await;
}

#[tokio::test]
async fn operator_reap_restarts_immediately_without_burning_budget_and_past_due_retry_survives_restart() {
    ensure_scenarios::operator_reap_restarts_immediately_without_burning_budget_and_past_due_retry_survives_restart(&Controller).await;
}

#[tokio::test]
async fn orphaned_ensure_reports_its_absent_parent_project() {
    ensure_scenarios::orphaned_ensure_reports_its_absent_parent_project(&Controller).await;
}

#[tokio::test]
async fn rebooted_standing_governor_admits_one_replacement_vessel_without_a_second_convoy() {
    ensure_scenarios::rebooted_standing_governor_admits_one_replacement_vessel_without_a_second_convoy(&Controller).await;
}

#[tokio::test]
async fn reconcile_now_acknowledges_recordless_teardown_and_readmits_the_ensure() {
    ensure_scenarios::reconcile_now_acknowledges_recordless_teardown_and_readmits_the_ensure(&Controller).await;
}

#[tokio::test]
async fn reconcile_now_clears_an_active_restart_limit_and_admits_in_one_pass() {
    ensure_scenarios::reconcile_now_clears_an_active_restart_limit_and_admits_in_one_pass(&Controller).await;
}

#[tokio::test]
async fn reconcile_now_resets_backoff_and_admits_the_next_ensure_generation_immediately() {
    ensure_scenarios::reconcile_now_resets_backoff_and_admits_the_next_ensure_generation_immediately(&Controller).await;
}

#[tokio::test]
async fn reconcile_now_waits_for_periodic_backing_inspection_and_keeps_one_generation() {
    ensure_scenarios::reconcile_now_waits_for_periodic_backing_inspection_and_keeps_one_generation(&Controller).await;
}

#[tokio::test]
async fn refused_convoy_reclaim_leaves_runtime_children_untouched() {
    ensure_scenarios::refused_convoy_reclaim_leaves_runtime_children_untouched(&Controller).await;
}

#[tokio::test]
async fn replicated_ensure_is_not_reconciled_away_from_its_project_home() {
    ensure_scenarios::replicated_ensure_is_not_reconciled_away_from_its_project_home(&Controller).await;
}

#[tokio::test]
async fn resolved_default_branch_dependency_change_retries_admission_before_deadline() {
    ensure_scenarios::resolved_default_branch_dependency_change_retries_admission_before_deadline(&Controller).await;
}

#[tokio::test]
async fn standing_backing_inspection_holds_empty_evidence_after_provisioning_started() {
    ensure_scenarios::standing_backing_inspection_holds_empty_evidence_after_provisioning_started(&Controller).await;
}

#[tokio::test]
async fn standing_ensure_admission_uses_default_branch_observed_only_on_non_driver_root() {
    ensure_scenarios::standing_ensure_admission_uses_default_branch_observed_only_on_non_driver_root(&Controller).await;
}

#[tokio::test]
async fn standing_ensure_applies_agent_overrides_to_the_admitted_workflow_snapshot() {
    ensure_scenarios::standing_ensure_applies_agent_overrides_to_the_admitted_workflow_snapshot(&Controller).await;
}

#[tokio::test]
async fn standing_ensure_does_not_capture_another_projects_bare_workflow_but_accepts_a_global_builtin() {
    ensure_scenarios::standing_ensure_does_not_capture_another_projects_bare_workflow_but_accepts_a_global_builtin(&Controller).await;
}

#[tokio::test]
async fn standing_ensure_holds_after_three_failed_generations_and_resumes_when_attention_is_cleared() {
    ensure_scenarios::standing_ensure_holds_after_three_failed_generations_and_resumes_when_attention_is_cleared(&Controller).await;
}

#[tokio::test]
async fn standing_ensure_holds_failed_convoy_while_backing_is_live_then_restarts_after_verified_death() {
    ensure_scenarios::standing_ensure_holds_failed_convoy_while_backing_is_live_then_restarts_after_verified_death(&Controller).await;
}

#[tokio::test]
async fn standing_ensure_records_admitted_config_and_surfaces_drift_without_replacing_work() {
    ensure_scenarios::standing_ensure_records_admitted_config_and_surfaces_drift_without_replacing_work(&Controller).await;
}

#[tokio::test]
async fn standing_ensure_retries_convoy_that_failed_before_provisioning() {
    ensure_scenarios::standing_ensure_retries_convoy_that_failed_before_provisioning(&Controller).await;
}

#[tokio::test]
async fn standing_ensure_without_agent_overrides_preserves_the_workflow_selector() {
    ensure_scenarios::standing_ensure_without_agent_overrides_preserves_the_workflow_selector(&Controller).await;
}

#[tokio::test]
async fn standing_presence_inherits_role_shape_and_delivers_charter_artifact() {
    ensure_scenarios::standing_presence_inherits_role_shape_and_delivers_charter_artifact(&Controller).await;
}

#[tokio::test]
async fn standing_readmission_writes_a_new_brief_artifact() {
    ensure_scenarios::standing_readmission_writes_a_new_brief_artifact(&Controller).await;
}

#[tokio::test]
async fn statusless_ensured_generation_is_live_even_when_address_labels_are_missing() {
    ensure_scenarios::statusless_ensured_generation_is_live_even_when_address_labels_are_missing(&Controller).await;
}

#[tokio::test]
async fn two_origins_admit_identical_placements_without_authorship_collision() {
    ensure_scenarios::two_origins_admit_identical_placements_without_authorship_collision(&Controller).await;
}

#[tokio::test]
async fn unavailable_declared_driver_surfaces_named_admission_conditions_without_fallback() {
    ensure_scenarios::unavailable_declared_driver_surfaces_named_admission_conditions_without_fallback(&Controller).await;
}
