//! Shared checkout path selection from durable and observed facts.
use crate::{CheckoutSpec, CheckoutStatus};

/// Prefer the published path, then a declared target, then the observed path.
pub fn checkout_path_from_status_and_spec<'a>(status: Option<&'a CheckoutStatus>, spec: &'a CheckoutSpec) -> Option<&'a str> {
    status.as_ref().and_then(|status| status.path.as_deref()).or_else(|| spec.target_path()).or(match spec {
        CheckoutSpec::Observed(spec) => Some(spec.path.as_str()),
        _ => None,
    })
}
