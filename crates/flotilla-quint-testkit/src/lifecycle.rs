//! The model groups disappearance causes; stored endings are flat.
use crate::itf::{payload, tag, text, unit, variant};
use flotilla_state_machines::machines::lifecycle::{Ending, VanishCause};
use serde_json::{json, Value};

pub fn ending_value(ending: &Ending) -> Value {
    match ending {
        Ending::Released => variant("Released", unit()),
        Ending::Lost => variant("Vanished", json!({"cause": variant("Lost", unit())})),
        Ending::Failed { reason } => variant("Vanished", json!({"cause": variant("Failed", json!(reason))})),
    }
}

pub fn vanish_cause(value: &Value) -> VanishCause {
    match tag(value) {
        "Lost" => VanishCause::Lost,
        "Failed" => VanishCause::Failed { reason: text(payload(value)).into() },
        other => panic!("vanish cause {other}"),
    }
}
