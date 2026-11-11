//! Acceptance policy for terminal inputs and engine turns.
//! A possible write is held and never retyped while held; fresh receiver
//! evidence settles it. Time alone is not receipt evidence.

pub const DELIVERY_HOLD_FOR: chrono::Duration = chrono::Duration::minutes(5);
