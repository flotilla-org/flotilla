"""Derived checks for WORKLOAD.md.

Run with `uv run --with marimo==0.25.1 marimo edit docs/workload/model.py`
(or `marimo run` for a read-only view). The driver sliders are read from the
Scale drivers table of the root WORKLOAD.md: each defaults to its Target and
slides up to its Ceiling, so the notebook and the table cannot drift apart.
Assumptions that are not drivers are listed under Derived checks in
WORKLOAD.md; keep that list and the constants cell here in step.
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
    Driver sliders are read from its Scale drivers table: each defaults to its
    Target and slides up to its Ceiling. Entity sizes are kiwi measurements
    (2026-10-12); counts are owner-set.
    """)
    return


@app.cell
def _(mo):
    import re

    def read_drivers(path):
        """Return {driver: (target, ceiling)} from the Scale drivers table."""
        drivers = {}
        in_section = False
        for line in path.read_text().splitlines():
            if line.startswith("## "):
                in_section = line.strip() == "## Scale drivers"
                continue
            if not (in_section and line.startswith("| `")):
                continue
            cells = [cell.strip() for cell in line.strip("|").split("|")]
            named = re.match(r"`([a-z_]+)`", cells[0])
            if not named or len(cells) < 4:
                raise ValueError(f"unrecognised Scale drivers row in {path}: {line}")
            name = named.group(1)
            target, ceiling = (parse_count(cell) for cell in cells[2:4])
            if target is not None and ceiling is not None:
                drivers[name] = (target, ceiling)
        return drivers

    def parse_count(cell):
        """First number in a cell: "1,000 sustained" -> 1000, "100k" -> 100000, "1M" -> 1000000."""
        found = re.search(r"(\d[\d,]*)([kM]?)", cell)
        if not found:
            return None
        return int(found.group(1).replace(",", "")) * {"": 1, "k": 1_000, "M": 1_000_000}[found.group(2)]

    workload_md = mo.notebook_dir().parents[1] / "WORKLOAD.md"
    drivers = read_drivers(workload_md)
    expected = {"running_convoys", "view_clients", "hosts", "change_events", "history_window"}
    missing = expected - drivers.keys()
    if missing:
        raise ValueError(f"{workload_md} Scale drivers table lacks numeric Target/Ceiling for: {sorted(missing)}")
    return (drivers,)


@app.cell
def _(drivers, mo):
    def driver_slider(name, start, label=None, stop=None):
        target, ceiling = drivers[name]
        return mo.ui.slider(start, stop or ceiling, value=target, label=label or name, show_value=True)

    running_convoys = driver_slider("running_convoys", 10)
    view_clients = driver_slider("view_clients", 1)
    hosts = driver_slider("hosts", 3)
    change_events = driver_slider("change_events", 10, label="change_events per s")
    # The window is fixed by policy (Target = Ceiling); slide past it to explore the policy.
    history_window_days = driver_slider("history_window", 1, label="history_window (days)", stop=30)
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
    match_fraction = 0.05  # share of events a filtered client subscribes to
    # Compact resident form: frozen workflow snapshots shared by content hash,
    # typed live records without embedded snapshots, finished convoys as summaries.
    live_convoy_compact = 4 * KB  # typed record; snapshot referenced, not embedded
    vessel_compact = 1 * KB
    session_compact = 1 * KB
    finished_summary = 1 * KB  # subject, outcome, timestamps, links; full body stays in the store
    distinct_snapshots = 200  # workflow templates x revisions in use
    snapshot_bytes = 16 * KB
    return (
        MB,
        convoy_bytes,
        convoys_finished_per_day_per_running,
        decode_ms_per_mb,
        distinct_snapshots,
        finished_summary,
        heartbeat_interval_s,
        heartbeat_wal_bytes,
        host_bytes,
        live_convoy_compact,
        match_fraction,
        session_bytes,
        session_compact,
        snapshot_bytes,
        typed_factor,
        vessel_bytes,
        vessel_compact,
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
        ## Resident memory, naive full-body form

        Mean vessels per convoy: **{mean_vessels:.1f}**.
        Live work: **{live / MB:,.0f} MB** JSON text; finished within the window: **{recent_finished / MB:,.0f} MB**.
        Holding today's full stored bodies as typed state: **{resident_typed / MB:,.0f} MB**.
        This is the naive form and is expected to exceed the budget (under 1 GB at the ceiling);
        the compact form below is the one measured against it.
        """
    )
    return mean_vessels, resident_text


@app.cell
def _(
    MB,
    change_events,
    decode_ms_per_mb,
    mo,
    resident_text,
    running_convoys,
):
    # The naive shape: every wake lists and decodes a whole kind from the store.
    whole_kind_mb = resident_text / MB
    naive_ms_per_wake = whole_kind_mb * decode_ms_per_mb
    naive_cpu_cores = change_events.value * naive_ms_per_wake / 1000
    mo.md(
        f"""
        ## Controller wake: naive versus resident

        A whole-kind list-and-decode per wake costs **{naive_ms_per_wake:,.0f} ms**.
        At `change_events` = {change_events.value:,}/s that is **{naive_cpu_cores:,.0f} CPU cores** of decode alone.
        An indexed lookup into typed resident state touches one object and decodes nothing,
        so wake cost stops depending on `running_convoys` ({running_convoys.value:,}).
        """
    )
    return


@app.cell
def _(change_events, match_fraction, mo, view_clients):
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
def _(
    MB,
    convoys_finished_per_day_per_running,
    distinct_snapshots,
    finished_summary,
    history_window_days,
    live_convoy_compact,
    mean_vessels,
    mo,
    running_convoys,
    session_compact,
    snapshot_bytes,
    vessel_compact,
):
    compact = (
        running_convoys.value * (live_convoy_compact + mean_vessels * (vessel_compact + session_compact))
        + running_convoys.value * convoys_finished_per_day_per_running * history_window_days.value * finished_summary
        + distinct_snapshots * snapshot_bytes
    )
    mo.md(
        f"""
        ## Compact resident form

        Sharing snapshots by content hash and keeping finished convoys as summaries
        gives **{compact / MB:,.0f} MB** resident for `running_convoys` = {running_convoys.value:,}
        (budget: under 1 GB at the ceiling), against the naive full-body copy above. The resident representation is a design choice, not the stored JSON.
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
