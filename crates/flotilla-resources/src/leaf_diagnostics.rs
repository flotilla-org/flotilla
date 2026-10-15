use crate::{LeafMaker, TurnDeliveryRule};
use chrono::{DateTime, Utc};
use flotilla_protocol::Leaf;
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeafWatcher {
    WaitCaller { connection_id: uuid::Uuid },
    ReconcilerWake { convoy: String },
    TurnDelivery { convoy: String, source: String, rule: Box<TurnDeliveryRule> },
}
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EpisodeKeyFields {
    pub source: Option<String>,
    pub convoy: Option<String>,
    pub vessel: Option<String>,
    pub role: Option<String>,
    pub subject_revision: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeafSubscriptionRow {
    pub id: uuid::Uuid,
    pub namespace: String,
    pub leaves: Vec<Leaf>,
    pub watcher: LeafWatcher,
    pub maker: LeafMaker,
    pub freshness_demand: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub episode_key: EpisodeKeyFields,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeafFiringRecord {
    pub leaf: Leaf,
    pub value: String,
    pub fired_at: DateTime<Utc>,
}
