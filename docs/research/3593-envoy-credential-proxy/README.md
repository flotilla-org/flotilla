# Stock Envoy per-host credential proxy: Linux experiment (#3593)

2026-10-20. **Result: stock Envoy can perform the specified proxy functions in this Linux experiment.** All nine functional/measurement criteria passed, including the optional real Codex WebSocket session. This is experimental evidence for the owner to reassess #3312, not a production design or an engine recommendation.

Version: `9e5580933739775e8bce696a57a2e1bc99368f23/1.40.0/Clean/RELEASE/BoringSSL`, official `envoy-1.40.0-linux-x86_64` release binary, without patches or contributed extensions. Ran entirely inside the assigned Docker vessel (`/.dockerenv` present), Ubuntu 24.04 userspace, Linux `7.2.9-2-cachyos`, x86_64. No host changes, system installs, or Docker socket access. The host name feta is supplied by the assignment; the container does not independently attest that host identity.

## Results

| # | Verdict | Observed evidence |
|---|---|---|
| 1 | **Pass** | Filesystem LDS added B while A streamed; B served a new intercepted request in **31.4 ms**, measured from before config generation/rename. After removing B, new connections were refused at the first probe **112.9 ms** after generation began (includes an intentional 100 ms wait). A and B each completed **13,107,200 bytes**, in **9.985 s / 9.982 s**. Envoy used a **15 s** drain deadline. A separate update added previously absent C's cluster and listeners using filesystem CDS/LDS; its authenticated request succeeded 4.5 ms after the generator returned. No xDS server or Envoy restart for these updates. |
| 2 | **Pass** | `Authorization: Bearer placeholder-a` on A became exactly `Bearer real-test-token-a` at the local TLS upstream, HTTP 200. |
| 3 | **Pass** | Stock git used a credential helper supplying `user` / `placeholder-a`. Initial push, clone, second push, and fetch succeeded. Local `git http-backend` logs show replaced bearer credentials on both `git-upload-pack` and `git-receive-pack`. No custom git authentication header was configured. |
| 4 | **Pass** | A's bearer placeholder on B returned **403**. The unique `/cross-vessel-proof` path occurred **zero** times in upstream logs. The authoritative vessel identity came from the selected internal listener's ext_authz route context, not an incoming header. |
| 5 | **Pass** | ext_proc saw the GraphQL JSON body: query returned **200**, mutation returned **403**; the unique mutation URL never reached upstream. Stopping just the authorization gRPC server caused **403**, with `ext_authz.error: 1`, zero upstream requests at `/fail-closed-proof`, and the still-running TLS upstream independently returned **200**. |
| 6 | **Pass** | Push containing **500 MiB (524,288,000 bytes)** of random data succeeded. Upstream received a **524,448,742-byte** git request. Envoy RSS baseline **78,049,280 bytes (74.43 MiB)**; peak **86,544,384 bytes (82.54 MiB)**; increase **8,495,104 bytes (8.10 MiB)**. Sampled every 20 ms, 1,057 samples over **21.37 s** covering the push. No whole-body memory buffering observed. |
| 7 | **Pass**, including Codex | `wss://echo.websocket.org` echoed `envoy-3593-test-payload` unchanged through CONNECT in **150 ms**, including connection setup and greeting. A real installed **Codex 0.160.0** session through `chatgpt.com:443` returned `ENVOY_PASSTHROUGH_OK`; targeted client logs explicitly say **successfully connected to websocket** at `wss://chatgpt.com/backend-api/codex/responses`. Envoy logged successful raw CONNECT tunnels to chatgpt.com. No TLS interception, auth hook, or body hook on these routes. |
| 8 | **Pass** | `CONNECT github.com:22` returned **200** and `SSH-2.0-54ec6dd1` in **162 ms**. Actual `ssh -vv -T git@github.com` through the same listener completed key exchange, saw GitHub's host key, and ended with expected `Permission denied (publickey)`; no authentication required. |
| 9 | **Pass (measurement)** | For **500** sequential warm requests, Envoy's direct per-request ext_authz `latency_us` was **median 246 µs, p99 559 µs** (nearest rank). This measures time Envoy waits for the local gRPC authorization hook. Same-run total request latency was median **0.882 ms**, p99 **1.481 ms**. TLS/CONNECT establishment excluded. |
| 10 | **Reported** | Drain deadlines can terminate existing CONNECT streams; CONNECT matching needs its own catch-all; local TCP delayed ACK/Nagle behavior can dominate latency; no observed CONNECT/TLS handshake deadlock. Details below. |

## Configuration and process boundaries

[spike.py](spike.py) generates the [bootstrap](evidence/bootstrap.yaml), [CDS](evidence/cds.yaml), and [LDS](evidence/lds.yaml) snapshots. One Envoy process owns all per-vessel Unix sockets. A byte-copy TCP-to-Unix bridge lets stock git and Codex reach those sockets; it performs no HTTP parsing or credential operations. Hook services, bridge, generator, and upstream are disposable scaffolding.

The bootstrap enables `envoy.bootstrap.internal_listener`; `dynamic_resources.lds_config` and `cds_config` each use `path_config_source`. Complete `DiscoveryResponse` YAML files are atomically renamed over their watched paths. The generator retains removed vessels' clusters to permit active listeners to drain; cluster reclamation is not designed here.

Each outer Unix listener has exact CONNECT authority routes:

```yaml
match:
  connect_matcher: {}
  headers:
  - name: ':authority'
    string_match: {exact: 'forge.test:443'}
route:
  cluster: tls_a
  timeout: 0s
  upgrade_configs:
  - upgrade_type: CONNECT
    connect_config: {}
```

`git.test:443` also goes to that vessel's internal listener. Exact passthrough destinations are `echo.websocket.org:443`, `chatgpt.com:443`, and `github.com:22`, routed directly to external TCP endpoints. A terminal `connect_matcher: {}` direct response returns **403** for every other CONNECT authority. This also rejects unlisted ports. Plain non-CONNECT requests have a terminal 403 route.

The per-vessel internal listener terminates TLS using a pre-issued local CA-signed certificate (SANs `forge.test`, `git.test`), then runs ext_authz, ext_proc, and router. Its trusted route context is:

```yaml
typed_per_filter_config:
  envoy.filters.http.ext_authz:
    '@type': type.googleapis.com/envoy.extensions.filters.http.ext_authz.v3.ExtAuthzPerRoute
    check_settings:
      context_extensions: {vessel: a}
```

The gRPC auth service matches that vessel's bearer or Basic placeholder and returns an authorization header replacement. Invalid placeholders receive 403; missing credentials receive 401 with a Basic challenge so stock git can invoke its helper. Auth uses `failure_mode_allow: false`, a 2 s RPC timeout, explicit error status Forbidden, and `emit_filter_state_stats: true`. There is no request-body buffering option or retry policy. Both test placeholder and replacement are fake strings.

ext_proc starts with body mode `NONE`. Its header response switches to `BUFFERED` only for `/graphql` paths; git bodies are never requested by the body hook. The scaffolding rejects a body containing `mutation` and admits the sample query. This is deliberately a lexical test hook, **not** a GraphQL authorization implementation or parser validation.

The local upstream is HTTPS on loopback, backed by `git http-backend` for git URLs. It spools upload data to a temporary file before running git; that is an upstream process behavior, outside the measured Envoy RSS. Whole-host/process-tree memory is not claimed. The test upstream TLS connection uses an unverified test-only upstream context; client-to-Envoy verification uses the local test CA. Certificate generation and trust are explicit scaffolding, not a proposed production trust policy.

## Commands and evidence

Run from this directory in an otherwise clean vessel. The setup script uses `/workspace/flotilla` and `/tmp/envoy-spike`; it creates throwaway keys, repositories, and files. Do not rerun initialization over existing experiment repositories.

```bash
bash setup.sh
/tmp/envoy-spike/envoy --version
/tmp/envoy-spike/venv/bin/python spike.py generate --vessels a
/tmp/envoy-spike/envoy --mode validate -c /tmp/envoy-spike/bootstrap.yaml
# In separate terminals:
/tmp/envoy-spike/venv/bin/python spike.py services
/tmp/envoy-spike/envoy -c /tmp/envoy-spike/bootstrap.yaml --concurrency 2 --drain-time-s 15
# With both processes running:
/tmp/envoy-spike/venv/bin/python checks.py lifecycle
/tmp/envoy-spike/venv/bin/python checks.py benchmark
/tmp/envoy-spike/venv/bin/python checks.py basics
bash git-check.sh small
# Prepare the random blob and initial large push:
bash git-check.sh large
# For the measured repeat, initialize a fresh bare upstream, then:
/tmp/envoy-spike/venv/bin/python memory.py
/tmp/envoy-spike/venv/bin/python checks.py latency
/tmp/envoy-spike/venv/bin/python passthrough.py
ssh -vv -T -o 'ProxyCommand=python3 /workspace/flotilla/docs/research/3593-envoy-credential-proxy/connect.py' \
  -o StrictHostKeyChecking=no -o UserKnownHostsFile=/tmp/envoy-spike/known_hosts \
  -o IdentityAgent=none -o IdentitiesOnly=yes -o BatchMode=yes git@github.com
```

The measured repeat used a fresh empty bare upstream, pushing the already committed blob with `memory.py`; its subprocess runs `git-check.sh push`. RSS is sampled with psutil for the Envoy PID only. Peak is a sampled lower bound, not a guarantee about shorter spikes. Timings include local machine scheduling and scaffolding costs, with no concurrency/load claim.

The Codex probe was run in a recorded cleat session with `HTTPS_PROXY`, `HTTP_PROXY`, `ALL_PROXY`, `WSS_PROXY`, `WS_PROXY` set to `http://127.0.0.1:18080` and proxy bypass lists empty. Lowercase HTTP proxy equivalents were set too. Command:

```bash
RUST_LOG=codex_api::endpoint::responses_websocket=debug \
  codex --no-daemon -c 'features.hooks=false' -c 'notify=[]' exec \
  --ephemeral --skip-git-repo-check --json -C /tmp/envoy-spike \
  'Do not use tools or read files. Reply with exactly ENVOY_PASSTHROUGH_OK.'
```

Model subscription credentials stayed inside the encrypted raw tunnel; no real forge credential was sent through Envoy. The test neither copied nor logged model credentials.

[Evidence directory](evidence/) contains the config snapshots, listener timings, git transcripts, upstream credential excerpts, RSS samples and summary, WebSocket/SSH output, Codex result and targeted WebSocket log, and refusal checks. In particular:

```text
# hooks-excerpts.jsonl
{"upstream": "/repo.git/git-receive-pack", "authorization": "Bearer real-test-token-a", "bytes": 524448742, ...}
# refusals.jsonl
{"upstream_path": "/cross-vessel-proof", "count": 0}
{"upstream_path": "/graphql?case=mutation", "count": 0}
{"upstream_path": "/fail-closed-proof", "count": 0}
# codex-ws.stderr
... successfully connected to websocket: wss://chatgpt.com/backend-api/codex/responses
```

For direct latency, the internal HCM logs:

```yaml
log_format:
  json_format:
    path: '%REQ(:PATH)%'
    auth_us: '%FILTER_STATE(envoy.filters.http.ext_authz:FIELD:latency_us)%'
    code: '%RESPONSE_CODE%'
```

Filter `/latency` rows in `auth-timing.jsonl`, sort their 500 integer `auth_us` values, and select median / element 495 (one based) for p99. Twenty `/latency-warmup` requests precede the samples. The separate enabled/disabled/enabled comparison used 400 samples per phase after 20 warmups: enabled median/p99 **0.896/1.864 ms**, disabled **0.530/0.997 ms**, enabled again **0.962/1.662 ms**. Differences between these percentiles are not presented as a per-request overhead distribution; direct filter timing supplies that measurement.

To reproduce fail-closed, call the direct local upstream's `/scaffold/stop-auth` endpoint to stop only the gRPC auth server, then send an authorized proxy request to a unique path. The final run returned 403, recorded no upstream request for that path, and verified the direct upstream still returned 200. Restart `spike.py services` to restore auth.

## Surprises and limits

* An initial **3 s** drain run cut off B after 4,587,520 of 13,107,200 bytes, around 3.504 s after stream start; A completed. With 15 s, both completed. Removing a listener is not a promise of indefinitely preserving its CONNECT streams. This agrees with the current [draining documentation](https://www.envoyproxy.io/docs/envoy/latest/intro/arch_overview/operations/draining). The 15 s value was selected to exceed these roughly 10 s streams.
* A `prefix: /` terminal route does not catch CONNECT targets: our initial config returned **404**, not the required 403. An explicit CONNECT catch-all fixed it; the final unknown-host check returns 403.
* The [upgrade docs](https://www.envoyproxy.io/docs/envoy/latest/intro/arch_overview/http/upgrades) describe synthesizing CONNECT 200 after initial upstream data. In this tested configuration, a direct raw probe received **200 in 0.392 ms before sending ClientHello**. The inner TLS listener then handshook normally. We did not observe the potential handshake deadlock suggested by a literal reading of that description; we do not generalize this to every transport/configuration.
* Initial local request totals were about **43 ms**, dominated by separate HTTP header/body writes and TCP delayed ACK/Nagle behavior in the Python TLS stand-in. Setting TCP_NODELAY on that upstream socket reduced totals below 1 ms. Final timings use that setting. No Envoy patch was involved.
* Exact authority routing is intentional; this experiment does not validate arbitrary DNS forwarding, DNS changes, all spellings/case variants of a forge authority, certificate renewal, unbounded listener churn, or a full production GraphQL policy. No production process layout, credential minting, host isolation, or macOS result is inferred.

Primary-source expectations and version caveats are in [documentation.md](documentation.md). The exact versioned [ext_authz logging documentation](https://www.envoyproxy.io/docs/envoy/v1.40.0/configuration/http/http_filters/ext_authz_filter#logging) defines the per-request timer used above.
