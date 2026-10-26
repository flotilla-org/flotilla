# Outbound HTTP request contract audit

Audit for #1512 (2026-10-04).

I searched all Rust sources for `tls::client`, `client_builder`, `reqwest`, request execution, and `.send()`; construction-only factories and transport implementations are grouped with their consumers below.

| Boundary / call sites | Coverage or justification |
| --- | --- |
| Dispatch native dependencies, closing PRs and board metadata (`crates/flotilla-github-providers/src/issue_tracker.rs`) | Authenticated live record/replay fixtures `github_dispatch_facts.yaml`, `github_dispatch_board.yaml` and `github_mission_fields.yaml` cover paginated native blocked-by REST requests, issue type, closing PR merge state and gh board JSON fields, native parents/types and the issue-field-values request. Pure mission normalization tests cover documented numeric/select values and source precedence. Client tests prohibit direct gh access. |
| Cleat old-generation session count (`crates/flotilla-core/src/cleat_roll.rs`) | Local Unix-socket IPC exception beside `contained_old_sessions_remain_skewed_without_a_captured_roll_report`: curl generates the standard GET, and the process-seam test asserts its complete argv, socket path and `/healthz` endpoint with no authored headers/body. The pinned cleat host handover gate exercises the real daemon protocol. This exception does not claim remote HTTP compatibility coverage. |
| OCI Distribution metadata (`crates/flotilla-environment/src/environment/registry_client.rs`) | In-process OCI stand-in checks manifest HEAD/GET and Accept negotiation, digest verification, missing content, paginated tags, Basic token-service auth, service and repository pull scopes, Bearer retries, denial and cross-origin pagination refusal. Only synthetic identities/tokens; no credential files. Sources are Distribution Specification v1.1.1 and CNCF Distribution token authentication. |
| GitHub App installation discovery and token mint (`credential.rs`) | New loopback HTTP stand-in runs the real minter and HTTP executor, checks routing, required headers, RS256 signature with the existing test key, issuer and timing claims, repositories and permissions. Negative cases prove the stand-in refuses malformed requests. No live recording or real App material. |
| Forgejo issues and pull requests (`crates/flotilla-forgejo-providers/src/issue_tracker.rs`, `crates/flotilla-forgejo-providers/src/change_request.rs`) | Existing recorded HTTP fixtures: `forgejo_issues.yaml`, `forgejo_pulls.yaml`, `forgejo_ghostty_governor.yaml`, `forgejo_head_filter_capability.yaml`. Typed rate-limit errors are covered by a fault-injection wrapper over the strict fake forge, including review reads and retry metadata. |
| Claude cloud sessions (`crates/flotilla-cloud-agent/src/claude.rs`) | Exemption beside the Claude factory tests: unpublished `ccr-byoc` consumer-session contract and keychain OAuth material. Legacy `claude_*.yaml` files have no active test references and are not claimed as current coverage. This remains a coverage gap. |
| Codex tasks (`crates/flotilla-cloud-agent/src/codex.rs`) | Existing recorded HTTP fixtures, including `codex_tasks.yaml`, `codex_auth_retry.yaml`, `codex_label_fallback.yaml`. |
| Cursor agent listing (`crates/flotilla-cloud-agent/src/cursor.rs`) | New HTTP stand-in enforces GET route, Basic test-key authorization and limit query. |
| Anthropic Messages (`crates/flotilla-ai-utility/src/claude_api.rs`) | New HTTP stand-in enforces POST route, API key/version/JSON headers, model/max_tokens/messages payload. |
| S3 blob store (`blob_store.rs`) | Existing recorded `src/fixtures/s3_blob_contract.yaml` exercises signed PUT/GET/HEAD/DELETE and the shared store behavior contract. |
| Archive bundles on the S3 blob store (`blob_store.rs` named objects, `runtime/archive.rs`, #3939) | New in-process S3 stand-in in `runtime/archive/tests.rs` verifies each request's AWS Signature Version 4 against the test credential, region and service, the signed payload hash, the bucket, prefix and canonical key path, and the bundle media type on PUT; it refuses a request signed with another secret. Synthetic credentials only. |
| Relay WebSocket upgrade, HTTP long poll and ack (`event_relay.rs`) | Exemption documented beside tests: owned ConsumerFrame/StreamFrame protocol, with in-memory lifecycle scenarios; no external-service header contract or live fixture. These scenarios do not claim HTTP transport compatibility coverage. |
| Codex OAuth refresh (`codex_central.rs`) | Exemption documented beside tests: consumer OAuth endpoint has no published request contract for this adapter; replay would capture rotating live credentials. Fixed-response tests cover decoding and failure classification, not remote compatibility. |
| Resource backend CRUD/list/watch, namespace/CRD bootstrap and kubeconfig (`flotilla-resources/src/http/`) | Owned resource-store/Kubernetes protocol: existing `http_wire.rs` and `provisioning_http_wire.rs` test request encoding. `http_digest_drill_down_reads_only_the_requested_partition` exercises root/children/bucket reads against the production daemon handler. Exemption beside wire tests distinguishes this from external-service contract coverage and does not claim bootstrap/TLS identity coverage. |
| Resource validation (`src/resource_validate.rs`) | Exemption beside tests: owned daemon API over local Unix socket, exercised against actual server by existing integration tests. |
| Browser login code (`src/cli/web.rs`, `flotilla web login`) | Owned daemon route over the local Unix socket. An in-process stand-in serves only POST on `/v1/web/login-codes` and returns the daemon's success and refusal shapes; the route itself is covered by the router tests in `crates/flotilla-daemon/src/server/http/tests.rs`. |
| Claude Code HTTP hooks (`crates/flotilla-agent/src/hooks.rs` install shape, `crates/flotilla-daemon/src/server/http/hooks.rs` routes, #3247) | Claude Code is the HTTP client. A stand-in for its hook client in `crates/flotilla-daemon/src/server/http/hook_tests.rs` follows code.claude.com/docs/en/hooks: it refuses `http` handlers on SessionStart/Setup and unknown handler fields, interpolates header variables only from `allowedEnvVars`, sends POST with `Content-Type: application/json` and the hook input, and classifies 2xx-empty, 2xx-JSON and other replies as Claude Code does. It drives the installed settings through the real loopback listener; synthetic hook keys only. |
| Wheelhouse metadata POST (`flotilla-manifest/src/sink.rs`) | Exemption beside tests: owned local IPC protocol, existing socket HTTP stand-in covers POST routing and payloads. |
| Shared TLS clients / `ReqwestHttpClient::execute` / `execute_to_file`, replay transports and request factories | Service-neutral construction/execution, grouped with consumers. Exemption beside transport tests; shared TLS User-Agent has existing wire coverage. Discovery factories and examples construct the audited adapters; they introduce no distinct request shape. |

The former replication-plane image archive HTTP boundary was removed in the
#2729 follow-up. [#2850](https://github.com/flotilla-org/flotilla/issues/2850)
tracks registry-less image bytes through a separate Tender raw-stream exposure
between directly reachable peers, without relay routing. Its byte-stream and
Docker process contract coverage belongs with that implementation; there is no
remaining replication HTTP image caller or contract to claim here.

CLAUDE.md now requires enforcing stand-ins or recorded replay for outbound HTTP; arbitrary-request mocks alone are insufficient. The exemptions above identify compatibility gaps explicitly, rather than treating response-only mocks as service contracts.


### ADR 0064 change stream (#3463)

`server/change_stream.rs` uses the forwarded Unix resource HTTP API. Its strict in-process
stand-in enforces GET and the change-stream path and delivers fragmented CBOR frames through
the real reqwest client. The router's change-stream body is covered in-process, including a
stalled consumer that the producer abandons after its deadline.
The same protocol source/follower is exercised over in-memory delivery with two and three
registered stores. Point-read requests use the production `read_record` path, encode query parameters, and
cover present/absent remote results and a local hit after replication. A paused HTTP download
proves gates, another home's apply, and outgoing serving remain available before keepalive.
