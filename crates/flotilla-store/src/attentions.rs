//! The attentions of a backend the engine's attentions are not installed on: every call is
//! refused. Attentions are attention Messages on the engine; there is no other store for them.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use flotilla_resources::attentions::{Attention, Attentions, OpenAttention, Sender, Subject};

const UNCOMPOSED: &str = "the engine's attentions are not composed on this daemon";

/// Refuses every attention call: the backend has no engine attentions installed.
pub(crate) struct Uncomposed;

#[async_trait]
impl Attentions for Uncomposed {
    async fn raise(&self, _: &str, _: &Attention) -> Result<(), String> {
        Err(UNCOMPOSED.to_owned())
    }

    async fn clear(&self, _: &str, _: Sender, _: &Subject) -> Result<(), String> {
        Err(UNCOMPOSED.to_owned())
    }

    async fn clear_within(&self, _: &str, _: &Subject) -> Result<(), String> {
        Err(UNCOMPOSED.to_owned())
    }

    async fn open(&self, _: &str, _: Option<Sender>, _: Option<&Subject>) -> Result<Vec<OpenAttention>, String> {
        Err(UNCOMPOSED.to_owned())
    }

    async fn acknowledged(&self, _: &str, _: Sender, _: &Subject, _: DateTime<Utc>) -> Result<Option<String>, String> {
        Err(UNCOMPOSED.to_owned())
    }
}
