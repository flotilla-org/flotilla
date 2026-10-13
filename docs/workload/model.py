"""Derived checks for WORKLOAD.md.

Run with `uv run --with marimo marimo edit docs/workload/model.py`
(or `marimo run` for a read-only view). Driver defaults mirror the Target
column of the root WORKLOAD.md; move the sliders to the Ceiling to check it.
"""

import marimo

__generated_with = "0.25.1"
app = marimo.App(width="medium")


@app.cell
def _():
    import marimo as mo

    return (mo,)


@app.cell
def _(mo):
    mo.md("""
    # Flotilla workload model: derived checks

    Each cell computes one check from the scale drivers in `WORKLOAD.md`.
    Entity sizes are kiwi measurements (2026-10-12); counts are owner-set.
    When a driver in `WORKLOAD.md` changes, update the default here and rerun.
    """)
    return


@app.cell
def _(mo):
    running_convoys = mo.ui.slider(10, 10_000, value=1_000, label="running_convoys", show_value=True)
    view_clients = mo.ui.slider(1, 500, value=50, label="view_clients", show_value=True)
    hosts = mo.ui.slider(3, 500, value=50, label="hosts", show_value=True)
    change_events = mo.ui.slider(10, 10_000, value=1_000, label="change_events per s", show_value=True)
    history_window_days = mo.ui.slider(1, 30, value=7, label="history_window (days)", show_value=True)
    navigator_share = mo.ui.slider(0.0, 1.0, value=0.2, step=0.05, label="share of convoys that are navigator sessions", show_value=True)
    mo.vstack([running_convoys, view_clients, hosts, change_events, history_window_days, navigator_share])
    return (
        change_events,
        history_window_days,
        hosts,
        navigator_share,
        running_convoys,
        view_clients,
    )


@app.cell
def _():
    # Entity sizes (bytes, typed in memory is roughly 1.5-2x JSON text; use 2x).
    KB = 1024
    MB = 1024 * KB
    convoy_bytes = 21 * KB
    vessel_bytes = 5 * KB
    session_bytes = 20 * KB
    host_bytes = 5 * KB
    typed_factor = 2.0
    # Vessels per convoy (mean).
    vessels_regimented = 2
    vessels_navigator = 10
    # Measured costs on the current store (kiwi, 2026-10-12).
    decode_ms_per_mb = 7.4  # five-pass JSON decode
    heartbeat_wal_bytes = 91 * KB  # per Host status write today
    heartbeat_interval_s = 5
    convoys_finished_per_day_per_running = 0.5  # turnover estimate
    return (
        MB,
        convoy_bytes,
        convoys_finished_per_day_per_running,
        decode_ms_per_mb,
        heartbeat_interval_s,
        heartbeat_wal_bytes,
        host_bytes,
        session_bytes,
        typed_factor,
        vessel_bytes,
        vessels_navigator,
        vessels_regimented,
    )


@app.cell
def _(
    MB,
    convoy_bytes,
    convoys_finished_per_day_per_running,
    history_window_days,
    host_bytes,
    hosts,
    mo,
    navigator_share,
    running_convoys,
    session_bytes,
    typed_factor,
    vessel_bytes,
    vessels_navigator,
    vessels_regimented,
):
    mean_vessels = (1 - navigator_share.value) * vessels_regimented + navigator_share.value * vessels_navigator
    per_convoy = convoy_bytes + mean_vessels * (vessel_bytes + session_bytes)
    live = running_convoys.value * per_convoy
    recent_finished = running_convoys.value * convoys_finished_per_day_per_running * history_window_days.value * per_convoy
    hosts_bytes = hosts.value * host_bytes
    resident_text = live + recent_finished + hosts_bytes
    resident_typed = resident_text * typed_factor
    mo.md(
        f"""
        ## Resident memory

        Mean vessels per convoy: **{mean_vessels:.1f}**.
        Live work: **{live / MB:,.0f} MB** JSON text; finished within the window: **{recent_finished / MB:,.0f} MB**.
        Resident typed state: **{resident_typed / MB:,.0f} MB** (budget: under 1 GB at the ceiling).
        """
    )
    return mean_vessels, per_convoy, resident_text


@app.cell
def _(
    MB,
    change_events,
    decode_ms_per_mb,
    mo,
    per_convoy,
    resident_text,
    running_convoys,
):
    # The naive shape: every wake lists and decodes a whole kind from the store.
    whole_kind_mb = resident_text / MB
    naive_ms_per_wake = whole_kind_mb * decode_ms_per_mb
    naive_cpu_cores = change_events.value * naive_ms_per_wake / 1000
    indexed_ms_per_wake = per_convoy / MB * decode_ms_per_mb  # one object, if it were even decoded
    mo.md(
        f"""
        ## Controller wake: naive versus resident

        A whole-kind list-and-decode per wake costs **{naive_ms_per_wake:,.0f} ms**.
        At `change_events` = {change_events.value:,}/s that is **{naive_cpu_cores:,.0f} CPU cores** of decode alone.
        One indexed resident lookup touches one object (about {indexed_ms_per_wake:.2f} ms even if decoded, ~0 when resident and typed),
        so wake cost stops depending on `running_convoys` ({running_convoys.value:,}).
        """
    )
    return


@app.cell
def _(change_events, mo, view_clients):
    match_fraction = 0.05  # share of events a filtered client subscribes to
    deliveries_per_s = change_events.value * view_clients.value * match_fraction
    naive_store_reads_per_s = change_events.value * view_clients.value  # one read per event per watcher today
    mo.md(
        f"""
        ## Delivery fan-out

        Filtered deltas: **{deliveries_per_s:,.0f} deliveries/s** at a {match_fraction:.0%} match fraction.
        The current per-watcher read path would make **{naive_store_reads_per_s:,.0f} store reads/s**; the target is zero.
        """
    )
    return


@app.cell
def _(MB, convoys_finished_per_day_per_running, history_window_days, mean_vessels, mo, running_convoys):
    # Compact resident form: frozen workflow snapshots shared by content hash,
    # typed live records without embedded snapshots, finished convoys as summaries.
    KB_ = 1024
    live_convoy_compact = 4 * KB_  # typed record; snapshot referenced, not embedded
    vessel_compact = 1 * KB_
    session_compact = 1 * KB_
    finished_summary = 1 * KB_  # subject, outcome, timestamps, links; full body stays in the store
    distinct_snapshots = 200  # workflow templates x revisions in use
    snapshot_bytes = 16 * KB_
    compact = (
        running_convoys.value * (live_convoy_compact + mean_vessels * (vessel_compact + session_compact))
        + running_convoys.value * convoys_finished_per_day_per_running * history_window_days.value * finished_summary
        + distinct_snapshots * snapshot_bytes
    )
    mo.md(
        f"""
        ## Compact resident form

        Sharing snapshots by content hash and keeping finished convoys as summaries
        gives **{compact / MB:,.0f} MB** resident for `running_convoys` = {running_convoys.value:,},
        against the full-body copy above. The resident representation is a design choice, not the stored JSON.
        """
    )
    return


@app.cell
def _(heartbeat_interval_s, heartbeat_wal_bytes, hosts, mo):
    GB = 1024**3
    wal_per_day = hosts.value * (86_400 / heartbeat_interval_s) * heartbeat_wal_bytes
    mo.md(
        f"""
        ## Heartbeat write amplification

        With today's ~91 KB of WAL per Host status write, `hosts` = {hosts.value} heartbeating every {heartbeat_interval_s} s
        writes **{wal_per_day / GB:,.1f} GB of WAL per day**. Heartbeats belong in observations (latest only, not durable history).
        """
    )
    return


if __name__ == "__main__":
    app.run()
