//! `flotilla pm connect` — the manifest metadata-patch connector.
//!
//! One per PM instance, launched inside the PM session (design on
//! flotilla-org/flotilla#667, build #708). Dials the local daemon as a
//! client, subscribes to the aggregator's named queries — the one
//! replica-aware, fleet-merged source — and projects the rows into
//! group/identity-targeted metadata patches for the enclosing PM. flotillad
//! itself never touches a PM.
//!
//! Failure honesty: catalog facts are TTL'd and re-asserted, so a dead
//! daemon fades the catalog out; pane/tab stamps (no TTL) survive. The
//! connector reconnects and re-lists, and restarts are idempotent
//! (`factory.id` dedupe, same source id).

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use flotilla_core::daemon::DaemonHandle;
use flotilla_manifest::{
    keys::REASSERT_INTERVAL_MS,
    pm::PmInstance,
    projection::{project_catalog, Catalog, CatalogInput},
    recipe::{AttachOnlyRecipes, RecipeMint},
    sink::PatchSink,
    wire::MetadataPatch,
};
use flotilla_protocol::{
    result_set::{ConvoyRow, IndependentRow, QueryChanges, ResultDelta, ResultSet, Rows},
    DaemonEvent, QueryCursor, QueryId, ResourceRef,
};
use tokio::sync::broadcast::error::RecvError;
use tracing::{debug, info, warn};

#[derive(bon::Builder)]
#[builder(on(String, into))]
pub struct PmConnectOptions {
    pub zellij_bin: Option<String>,
    pub plugin_url: Option<String>,
    pub wheelhouse_socket: Option<PathBuf>,
    /// Binary name minted into materialise recipes — what the PM runs on
    /// activation, resolved in the PM's own environment.
    pub flotilla_bin: String,
}

/// Resolve the PM this connector serves: explicit configuration wins, then
/// environment detection.
pub fn resolve_pm(options: &PmConnectOptions, env: &dyn Fn(&str) -> Option<String>) -> Result<PmInstance, String> {
    if let Some(socket) = &options.wheelhouse_socket {
        return Ok(PmInstance::wheelhouse(socket));
    }
    PmInstance::detect(env)
        .map(|pm| pm.with_zellij_bin(options.zellij_bin.clone()).with_plugin_url(options.plugin_url.clone()))
        .ok_or_else(|| "no presentation manager detected: run inside one or pass --wheelhouse-socket".to_owned())
}

/// What applying one daemon event did to the connector's row state.
#[derive(Debug, PartialEq, Eq)]
pub enum Applied {
    /// Rows changed; the catalog should be rebuilt and the diff published.
    Updated,
    /// Duplicate or irrelevant event; nothing to do.
    Ignored,
    /// Sequence gap (or delta before any full set) — resubscribe; the stale
    /// cursor makes the daemon emit a fresh full [`ResultSet`].
    Gap(QueryId),
}

/// The connector's held state: fleet-merged rows per query, per-query
/// cursors, and the catalog as last published.
pub struct ConnectorState {
    convoys: HashMap<ResourceRef, ConvoyRow>,
    independents: HashMap<ResourceRef, IndependentRow>,
    seqs: HashMap<QueryId, u64>,
    catalog: Catalog,
    subscriber_id: uuid::Uuid,
}

impl Default for ConnectorState {
    fn default() -> Self {
        Self {
            convoys: HashMap::new(),
            independents: HashMap::new(),
            seqs: HashMap::new(),
            catalog: Catalog::default(),
            subscriber_id: uuid::Uuid::new_v4(),
        }
    }
}

impl ConnectorState {
    pub fn apply_event(&mut self, event: &DaemonEvent) -> Applied {
        match event {
            DaemonEvent::ResultSet(set) => self.apply_result_set(set),
            DaemonEvent::ResultDelta(delta) => self.apply_delta(delta),
            _ => Applied::Ignored,
        }
    }

    fn apply_result_set(&mut self, set: &ResultSet) -> Applied {
        let query = set.query();
        if self.seqs.get(&query).is_some_and(|&seen| set.seq < seen) {
            return Applied::Ignored;
        }
        match &set.rows {
            Rows::Convoys(rows) => self.convoys = rows.iter().map(|row| (row.resource.clone(), row.clone())).collect(),
            Rows::Independents(rows) => self.independents = rows.iter().map(|row| (row.resource.clone(), row.clone())).collect(),
            Rows::Issues { .. } | Rows::Checkouts { .. } => return Applied::Ignored,
        }
        self.seqs.insert(query, set.seq);
        Applied::Updated
    }

    fn apply_delta(&mut self, delta: &ResultDelta) -> Applied {
        let query = delta.query();
        let Some(&seen) = self.seqs.get(&query) else {
            return Applied::Gap(query);
        };
        if delta.seq <= seen {
            return Applied::Ignored;
        }
        if delta.seq != seen + 1 {
            return Applied::Gap(query);
        }
        match &delta.changes {
            QueryChanges::Convoys { changed: rows, removed } => {
                for row in rows {
                    self.convoys.insert(row.resource.clone(), row.clone());
                }
                for removed in removed {
                    self.convoys.remove(removed);
                }
            }
            QueryChanges::Independents { changed: rows, removed } => {
                for row in rows {
                    self.independents.insert(row.resource.clone(), row.clone());
                }
                for removed in removed {
                    self.independents.remove(removed);
                }
            }
            QueryChanges::Issues { .. } | QueryChanges::Checkouts { .. } => {
                return Applied::Gap(query);
            }
        }
        self.seqs.insert(query, delta.seq);
        Applied::Updated
    }

    /// Reproject the catalog from the held rows and return the patches that
    /// move the PM from the previously published catalog to the new one.
    pub fn rebuild(&mut self, mint: &dyn RecipeMint) -> Vec<MetadataPatch> {
        let convoys: Vec<ConvoyRow> = self.convoys.values().cloned().collect();
        let independents: Vec<IndependentRow> = self.independents.values().cloned().collect();
        let next = project_catalog(&CatalogInput { convoys: &convoys, independents: &independents }, mint);
        let patches = next.diff_patches(&self.catalog);
        self.catalog = next;
        patches
    }

    /// Full re-assertion of the published catalog — the TTL heartbeat.
    pub fn reassert(&self) -> Vec<MetadataPatch> {
        self.catalog.reassert_patches()
    }

    /// Resume cursors for every named query. A gapped query's cursor is
    /// stale by construction, so resubscribing with these gets it a full
    /// [`ResultSet`].
    pub fn cursors(&self) -> Vec<QueryCursor> {
        QueryId::ALWAYS_MATERIALIZED.iter().cloned().map(|query| QueryCursor { since: self.seqs.get(&query).copied(), query }).collect()
    }
}

async fn send_patches(sink: &dyn PatchSink, patches: Vec<MetadataPatch>) {
    for patch in patches {
        if let Err(error) = sink.send(&patch).await {
            warn!(%error, "failed to publish metadata patch");
        }
    }
}

/// (Re)subscribe to every named query and publish whatever changed.
async fn resubscribe(
    daemon: &dyn DaemonHandle,
    state: &mut ConnectorState,
    mint: &dyn RecipeMint,
    sink: &dyn PatchSink,
) -> Result<(), String> {
    let events = daemon.subscribe_queries(state.subscriber_id, &state.cursors()).await?;
    let mut updated = false;
    for event in &events {
        updated |= state.apply_event(event) == Applied::Updated;
    }
    if updated {
        send_patches(sink, state.rebuild(mint)).await;
    }
    Ok(())
}

/// The connector loop: subscribe → project → send, with a TTL re-assertion
/// tick and gap-triggered resubscription. Returns when the daemon
/// connection's event stream closes.
pub async fn run_connector(
    daemon: Arc<dyn DaemonHandle>,
    sink: Arc<dyn PatchSink>,
    mint: Arc<dyn RecipeMint>,
    reassert_interval: Duration,
) -> Result<(), String> {
    // Subscribe to the broadcast before the query subscription so nothing
    // emitted in between is dropped.
    let mut events = daemon.subscribe();
    let mut state = ConnectorState::default();
    resubscribe(&*daemon, &mut state, &*mint, &*sink).await?;
    info!("pm connector subscribed; publishing catalog");

    let mut tick = tokio::time::interval(reassert_interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    tick.reset(); // the first tick fires immediately otherwise; the bootstrap publish just happened

    loop {
        tokio::select! {
            _ = tick.tick() => send_patches(&*sink, state.reassert()).await,
            received = events.recv() => match received {
                Ok(event) => match state.apply_event(&event) {
                    Applied::Updated => send_patches(&*sink, state.rebuild(&*mint)).await,
                    Applied::Ignored => {}
                    Applied::Gap(query) => {
                        debug!(%query, "result stream gap; resubscribing");
                        resubscribe(&*daemon, &mut state, &*mint, &*sink).await?;
                    }
                },
                Err(RecvError::Lagged(skipped)) => {
                    warn!(skipped, "event stream lagged; resubscribing");
                    resubscribe(&*daemon, &mut state, &*mint, &*sink).await?;
                }
                Err(RecvError::Closed) => {
                    daemon.unsubscribe_queries(state.subscriber_id).await;
                    return Err("daemon event stream closed".to_owned());
                }
            }
        }
    }
}

/// CLI entry: detect the PM, then keep a connector running against the local
/// daemon, reconnecting on failure. Catalog facts fade by TTL while the
/// daemon is away and re-assert on return.
pub async fn run(
    socket_path: &Path,
    config_dir: &Path,
    config_dir_override: Option<&Path>,
    socket_override: Option<&Path>,
    options: PmConnectOptions,
) -> Result<(), String> {
    // The connector runs headless in a PM pane: structured logs to stderr.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .try_init();
    let sink = resolve_pm(&options, &|key| std::env::var(key).ok())?.sink();
    let mint: Arc<dyn RecipeMint> = Arc::new(AttachOnlyRecipes::new(options.flotilla_bin.clone()));
    loop {
        let daemon = crate::socket::connect_or_spawn(socket_path, config_dir, config_dir_override, socket_override).await?;
        if let Err(error) = run_connector(daemon, sink.clone(), mint.clone(), Duration::from_millis(REASSERT_INTERVAL_MS)).await {
            warn!(%error, "connector stopped; reconnecting");
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

#[cfg(test)]
mod tests;
