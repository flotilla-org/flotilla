//! Shared Aggregator state used for replay and fleet-replica export.

use std::{collections::HashMap, sync::Arc};

use flotilla_protocol::{
    panel::{
        IntentDefinition, IntentKind, PanelColumn, PanelField, PanelId, PanelRow, PanelScope, PanelSnapshot, PanelSource, PanelView,
        ResourceRef, TabView,
    },
    HostName,
};
use tokio::sync::{RwLock, RwLockWriteGuard};

pub const CONVOY_TAB_ID: &str = "convoys";
pub const CONVOY_PANEL_ID: &str = "convoys";

#[derive(Default, Debug, Clone)]
pub struct AggregatorView {
    pub local_rows: HashMap<ResourceRef, PanelRow>,
    pub replica_rows: HashMap<HostName, HashMap<ResourceRef, PanelRow>>,
    pub seq: u64,
}

impl AggregatorView {
    pub fn snapshot(&self) -> PanelSnapshot {
        let mut rows: Vec<_> = self.local_rows.values().chain(self.replica_rows.values().flat_map(|rows| rows.values())).cloned().collect();
        rows.sort_by(|left, right| {
            (&left.resource.namespace, &left.resource.name, &left.resource.host).cmp(&(
                &right.resource.namespace,
                &right.resource.name,
                &right.resource.host,
            ))
        });
        panel_snapshot(self.seq, rows)
    }

    pub fn local_snapshot(&self) -> PanelSnapshot {
        let mut rows: Vec<_> = self.local_rows.values().cloned().collect();
        rows.sort_by(|left, right| (&left.resource.namespace, &left.resource.name).cmp(&(&right.resource.namespace, &right.resource.name)));
        panel_snapshot(self.seq, rows)
    }
}

fn panel_snapshot(seq: u64, rows: Vec<PanelRow>) -> PanelSnapshot {
    PanelSnapshot {
        seq,
        tab: TabView {
            id: CONVOY_TAB_ID.to_string(),
            title: "Convoys".to_string(),
            panels: vec![PanelView {
                id: PanelId::new(CONVOY_PANEL_ID),
                title: "Convoys".to_string(),
                source: PanelSource::resource("flotilla.work/v1", "Convoy"),
                scope: PanelScope::Fleet,
                columns: vec![
                    PanelColumn::new("name", "Convoy", PanelField::Name),
                    PanelColumn::new("workflow_ref", "Workflow", PanelField::Workflow),
                    PanelColumn::new("phase", "State", PanelField::Phase),
                    PanelColumn::new("repo", "Repository", PanelField::Repository),
                ],
                intents: vec![
                    IntentDefinition::new("attach", "Attach", IntentKind::Attach),
                    IntentDefinition::new("complete-leg", "Complete leg", IntentKind::CompleteLeg),
                ],
                rows,
            }],
        },
    }
}

#[derive(Debug, Default, Clone)]
pub struct AggregatorProjectionState {
    inner: Arc<RwLock<AggregatorView>>,
}

impl AggregatorProjectionState {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn write(&self) -> RwLockWriteGuard<'_, AggregatorView> {
        self.inner.write().await
    }

    pub async fn snapshot(&self) -> PanelSnapshot {
        self.inner.read().await.snapshot()
    }

    pub async fn local_snapshot(&self) -> PanelSnapshot {
        self.inner.read().await.local_snapshot()
    }
}
