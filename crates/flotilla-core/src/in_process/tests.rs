//! Composition-root scenarios, grouped by the transaction or projection they exercise.
//! Shared test support keeps cross-area credential staging and forge observations reusable.

use super::observation_cache_delay;

mod support;
pub(super) use support::{create_identity_convoy, create_running_session, create_test_environment, test_meta};
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
