# Daemon load metrics

Enable the endpoint in `daemon.toml` and restart the daemon:

```toml
[http]
listen = "127.0.0.1:7878"
metrics = true
```

`metrics` defaults to false. The existing HTTP listener rejects public bind
addresses, including hostnames that resolve to any non-loopback address.
Alloy can scrape `http://127.0.0.1:7878/metrics` without credentials. Existing
Host and Origin checks still apply. The endpoint is also available through the
main daemon Unix HTTP socket when enabled.

All series have a `host` label. Other labels are limited to store operation
(`read`, `write`), compiled controller kind, read-retry consumer, and the forge
budget's fixed credential slot and budget (`REST`, `GraphQL`, `CLI`). No resource,
peer, view, PR, or authenticated user IDs are labels.

Latency histograms use seconds, cumulative inclusive buckets from the existing
microsecond bucket scheme, a `+Inf` bucket, `_sum` and `_count`. Their `_max`
series and queue peaks are high-water gauges since daemon start. Counters end
in `_total` and reset on process restart.

Key series for a load run:

- `flotilla_store_queue_wait_seconds` and `flotilla_store_execution_seconds`,
  each with separate read/write series;
- `flotilla_read_retry_retries_total` by consumer;
- `flotilla_store_contention_total`: busy read refusals returned to callers,
  counted once even if a queued read is later shed;
- `flotilla_replication_stream_failures_total` and
  `flotilla_replication_lag_resets_total`.

Controller counters, busy time, queue wait, reconcile latency, peaks, store
shedding, replication snapshots/events and all read-retry lifecycle gauges are
also exported. Forge budget window counts are gauges because they reset hourly;
matching `_total` counters accumulate over the process lifetime. Remaining budget
and retry timestamps are emitted only when known. Budget series appear after
that identity/budget has made an attempt; per-caller forge tracing is separate.

After the roll, the operator enables this option on fleet hosts and confirms
Alloy ingestion into VictoriaMetrics. Repository tests require no fleet or lab
observability access.
