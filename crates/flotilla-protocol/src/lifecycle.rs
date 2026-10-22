//! Resource lifecycle ownership shared by resource storage and query rows.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(flotilla_schema::Describe)]
pub enum LifecycleAuthority {
    Observed,
    Adopted,
    Managed,
}

impl LifecycleAuthority {
    pub fn as_label_value(self) -> &'static str {
        match self {
            Self::Observed => "observed",
            Self::Adopted => "adopted",
            Self::Managed => "managed",
        }
    }

    pub fn from_label_value(value: &str) -> Result<Self, String> {
        match value {
            "observed" => Ok(Self::Observed),
            "adopted" => Ok(Self::Adopted),
            "managed" => Ok(Self::Managed),
            other => Err(format!("invalid lifecycle authority '{other}'")),
        }
    }
}

/// How an engine-held lifecycle resource stands now (ADR 0064, #3629 layer 4): a judgement
/// computed per key at read time from its stored state, never stored. Lost and released are
/// both the stored Gone, told apart by the edge that ended it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "judgement", rename_all = "snake_case")]
pub enum LifecycleReading {
    /// Requested, with no failure since its last success: being realised.
    InProgress,
    /// Requested after `failures` consecutive failures; the next try is due at `next`.
    Retrying { failures: u32, next: chrono::DateTime<chrono::Utc>, cause: String },
    /// Requested, its attempts spent: it gives up on the next reconcile.
    Exhausted { cause: String },
    /// Realised under `identity`.
    Ready { identity: String },
    /// Gone, observed absent without a release.
    Lost,
    /// Gone, torn down on purpose.
    Released,
    /// Gone, without the entry that says how (a replica seeded from a snapshot).
    Gone,
    /// Its attempts were spent; the last failed with `cause`.
    GaveUp { cause: String },
}

impl LifecycleReading {
    /// A short label for views: the judgement's name.
    pub fn label(&self) -> &'static str {
        match self {
            Self::InProgress => "InProgress",
            Self::Retrying { .. } => "Retrying",
            Self::Exhausted { .. } => "Exhausted",
            Self::Ready { .. } => "Ready",
            Self::Lost => "Lost",
            Self::Released => "Released",
            Self::Gone => "Gone",
            Self::GaveUp { .. } => "GaveUp",
        }
    }

    /// The cause a view gives for a reading that is not ready.
    pub fn reason(&self) -> Option<String> {
        match self {
            Self::InProgress => Some("being realised".into()),
            Self::Retrying { failures, next, cause } => Some(format!("{cause} ({failures} failed; next try at {})", next.to_rfc3339())),
            Self::Exhausted { cause } => Some(format!("{cause} (attempts spent)")),
            Self::Ready { .. } => None,
            Self::Lost => Some("observed absent".into()),
            Self::Released => Some("released".into()),
            Self::Gone => Some("gone".into()),
            Self::GaveUp { cause } => Some(cause.clone()),
        }
    }
}

#[cfg(test)]
mod reading_tests {
    use super::LifecycleReading;

    // Behaviour (#3629 layer 4): the labels are the views' contract: one per judgement, stable.
    #[test]
    fn each_reading_has_its_stable_label() {
        let at = chrono::DateTime::UNIX_EPOCH;
        let readings = [
            (LifecycleReading::InProgress, "InProgress"),
            (LifecycleReading::Retrying { failures: 1, next: at, cause: "c".into() }, "Retrying"),
            (LifecycleReading::Exhausted { cause: "c".into() }, "Exhausted"),
            (LifecycleReading::Ready { identity: "i".into() }, "Ready"),
            (LifecycleReading::Lost, "Lost"),
            (LifecycleReading::Released, "Released"),
            (LifecycleReading::Gone, "Gone"),
            (LifecycleReading::GaveUp { cause: "c".into() }, "GaveUp"),
        ];
        for (reading, label) in readings {
            assert_eq!(reading.label(), label);
            assert_eq!(reading.reason().is_none(), label == "Ready", "{label}");
            let json = serde_json::to_value(&reading).expect("encodes");
            assert_eq!(
                json["judgement"],
                serde_json::json!(label.chars().fold(String::new(), |mut snake, c| {
                    if c.is_uppercase() && !snake.is_empty() {
                        snake.push('_');
                    }
                    snake.push(c.to_ascii_lowercase());
                    snake
                }))
            );
        }
    }
}
