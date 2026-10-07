# Outbound HTTP request contract audit

Audit for #1512 (2026-10-04).

I searched all Rust sources for `tls::client`, `client_builder`, `reqwest`, request execution, and `.send()`; construction-only factories and transport implementations are grouped with their consumers below.

| Boundary / call sites | Coverage or justification |
| --- | --- |
| Dispatch footprints (`issue_tracker/github.rs`) | Authenticated record/replay `github_footprints.yaml` covers paginated merged/open PR file requests, zero additional PR calls on an unchanged second refresh, branch-tip resolution and comparison after push, and 404 for an unpublished branch. Changed-file counts and PR-head consistency guard truncation/races; pure file tests cover renames, missing patches and malformed rows. |
| Dispatch native dependencies, closing PRs and board metadata (`issue_tracker/github.rs`) | Authenticated live record/replay fixtures `github_dispatch_facts.yaml`, `github_dispatch_board.yaml` and `github_mission_fields.yaml` cover paginated native blocked-by REST requests, issue type, closing PR merge state and gh board JSON fields, native parents/types and the issue-field-values request. Pure mission normalization tests cover documented numeric/select values and source precedence. Client tests prohibit direct gh access. |
| GitHub App installation discovery and token mint (`credential.rs`) | New loopback HTTP stand-in runs the real minter and HTTP executor, checks routing, required headers, RS256 signature with the existing test key, issuer and timing claims, repositories and permissions. Negative cases prove the stand-in refuses malformed requests. No live recording or real App material. |
| Forgejo issues and pull requests (`providers/issue_tracker/forgejo.rs`, `providers/change_request/forgejo.rs`) | Existing recorded HTTP fixtures: `forgejo_issues.yaml`, `forgejo_pulls.yaml`, `forgejo_ghostty_governor.yaml`, `forgejo_head_filter_capability.yaml`. |
| Claude cloud sessions (`providers/coding_agent/claude.rs`) | Exemption beside the Claude factory tests: unpublished `ccr-byoc` consumer-session contract and keychain OAuth material. Legacy `claude_*.yaml` files have no active test references and are not claimed as current coverage. This remains a coverage gap. |
| Codex tasks (`providers/coding_agent/codex.rs`) | Existing recorded HTTP fixtures, including `codex_tasks.yaml`, `codex_auth_retry.yaml`, `codex_label_fallback.yaml`. |
| Cursor agent listing (`providers/coding_agent/cursor.rs`) | New HTTP stand-in enforces GET route, Basic test-key authorization and limit query. |
| Anthropic Messages (`providers/ai_utility/claude_api.rs`) | New HTTP stand-in enforces POST route, API key/version/JSON headers, model/max_tokens/messages payload. |
| S3 blob store (`blob_store.rs`) | Existing recorded `src/fixtures/s3_blob_contract.yaml` exercises signed PUT/GET/HEAD/DELETE and the shared store behavior contract. |
| Relay WebSocket upgrade, HTTP long poll and ack (`event_relay.rs`) | Exemption documented beside tests: owned ConsumerFrame/StreamFrame protocol, with in-memory lifecycle scenarios; no external-service header contract or live fixture. These scenarios do not claim HTTP transport compatibility coverage. |
| Codex OAuth refresh (`codex_central.rs`) | Exemption documented beside tests: consumer OAuth endpoint has no published request contract for this adapter; replay would capture rotating live credentials. Fixed-response tests cover decoding and failure classification, not remote compatibility. |
| Resource backend CRUD/list/watch, namespace/CRD bootstrap and kubeconfig (`flotilla-resources/src/http/`) | Owned resource-store/Kubernetes protocol: existing `http_wire.rs` and `provisioning_http_wire.rs` test request encoding. `http_digest_drill_down_reads_only_the_requested_partition` exercises root/children/bucket reads against the production daemon handler. Exemption beside wire tests distinguishes this from external-service contract coverage and does not claim bootstrap/TLS identity coverage. |
| Resource validation (`src/resource_validate.rs`) | Exemption beside tests: owned daemon API over local Unix socket, exercised against actual server by existing integration tests. |
| Wheelhouse metadata POST (`flotilla-manifest/src/sink.rs`) | Exemption beside tests: owned local IPC protocol, existing socket HTTP stand-in covers POST routing and payloads. |
| Shared TLS clients / `ReqwestHttpClient::execute` / `execute_to_file`, replay transports and request factories | Service-neutral construction/execution, grouped with consumers. Exemption beside transport tests; shared TLS User-Agent has existing wire coverage. Discovery factories and examples construct the audited adapters; they introduce no distinct request shape. |

The former replication-plane image archive HTTP boundary was removed in the
#2729 follow-up. [#2850](https://github.com/flotilla-org/flotilla/issues/2850)
tracks registry-less image bytes through a separate Tender raw-stream exposure
between directly reachable peers, without relay routing. Its byte-stream and
Docker process contract coverage belongs with that implementation; there is no
remaining replication HTTP image caller or contract to claim here.

CLAUDE.md now requires enforcing stand-ins or recorded replay for outbound HTTP; arbitrary-request mocks alone are insufficient. The exemptions above identify compatibility gaps explicitly, rather than treating response-only mocks as service contracts.
