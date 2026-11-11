//! Attentions for the crew tests still on the old Message store, which never look at them. Goes
//! when crew compares engine addresses (#3108 gate task); every other test uses the engine's.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use flotilla_resources::attentions::{Attention, Attentions, OpenAttention, Sender, Subject};

/// Holds nothing and takes every raise and clear.
pub struct NoAttentions;

#[async_trait]
impl Attentions for NoAttentions {
    async fn raise(&self, _: &str, _: &Attention) -> Result<(), String> {
        Ok(())
    }

    async fn clear(&self, _: &str, _: Sender, _: &Subject) -> Result<(), String> {
        Ok(())
    }

    async fn clear_within(&self, _: &str, _: &Subject) -> Result<(), String> {
        Ok(())
    }

    async fn open(&self, _: &str, _: Option<Sender>, _: Option<&Subject>) -> Result<Vec<OpenAttention>, String> {
        Ok(Vec::new())
    }

    async fn acknowledged(&self, _: &str, _: Sender, _: &Subject, _: DateTime<Utc>) -> Result<Option<String>, String> {
        Ok(None)
    }
}
