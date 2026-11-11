//! Composition-root scenarios, grouped by the transaction or projection they exercise.
//! Shared test support keeps cross-area credential staging and forge observations reusable.

mod start_attach;
mod support;
pub(super) use support::{
    admit, admit_snapshot, complete_work, convoy_engine, create_identity_convoy, create_running_session, create_test_environment,
    engine_fixture, engine_messages, ensure_vessel, identity_launch, install_engine_messages, test_engine, test_meta, TestEngine,
};
mod admission;
mod branch_discovery;
mod checkout_providers;
mod credential_admission;
mod credential_handoff;
mod crew_lifecycle;
mod dispatch_board;
mod host_admission;
mod host_events;
mod image_admission;
mod messages;
mod observation_completion;
mod observation_cooldown;
mod observation_ownership;
mod observation_pagination;
mod observation_support;
mod placement;
mod repository_queries;
mod resume;
mod supervision;
mod turn_delivery;
