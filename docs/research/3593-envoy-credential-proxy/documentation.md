# Envoy credential-proxy experiment: official documentation notes

Checked 2026-10-20. These are configuration expectations, not experimental results. The `/latest/` documentation currently identifies itself as 1.41.0-dev; the experiment report must identify its actual binary version and distinguish version-specific behavior.

## File-based listener lifecycle

Filesystem subscriptions consume a `DiscoveryResponse` (YAML, JSON, binary protobuf, or proto text), not a bare Listener or bootstrap document. Default filesystem watching uses Linux inotify move events; write a complete temporary file and atomically rename it into place. Invalid updates preserve the last valid configuration; use logs and update-success/rejection counters because filesystem subscriptions provide no ACK/NACK channel. A management server is unnecessary. [xDS protocol: filesystem subscriptions](https://www.envoyproxy.io/docs/envoy/latest/api-docs/xds_protocol#filesystem-subscriptions)

Listener removal/modification through LDS invokes per-listener draining. HCM supports graceful draining; raw TCP proxy is absent from the documented list of filters supporting graceful draining. Current docs explicitly warn that CONNECT/WebSocket streams are reset during proactive draining, while ordinary active requests can complete. Therefore report separately whether the removed listener stops accepting, whether existing tunnels survive, and whether another listener's tunnels survive. Current draining behavior may differ from an older installed release. [Draining](https://www.envoyproxy.io/docs/envoy/latest/intro/arch_overview/operations/draining)

Unix listening addresses use `address.pipe.path`; they are distinct from Envoy's in-process internal-listener address type. [Address API](https://www.envoyproxy.io/docs/envoy/latest/api-v3/config/core/v3/address.proto.html)

## CONNECT, identity, and TLS

CONNECT is disabled by default and returns 403 unless enabled. Use HCM `upgrade_configs` with `upgrade_type: CONNECT`, a route `connect_matcher: {}`, and route upgrade `connect_config: {}` to terminate CONNECT and forward raw bytes. Without `connect_config`, Envoy forwards the CONNECT request upstream. Exact-domain CONNECT matching may require the port, such as `github.com:22`. Use an explicit allowlist and terminal 403 route.

The docs say the router synthesizes CONNECT 200 after initial upstream TCP data. Test TLS-inside-CONNECT carefully: the client can wait for 200 before sending ClientHello, while a TLS server waits for ClientHello before sending data. This is a hypothesis to verify, not a demonstrated failure. A passthrough CONNECT carries the entire TLS/WebSocket conversation as raw bytes; no inner WebSocket HCM configuration is required. [HTTP upgrades](https://www.envoyproxy.io/docs/envoy/latest/intro/arch_overview/http/upgrades)

Internal listeners require the bootstrap extension `envoy.bootstrap.internal_listener`, a listener with `internal_listener: {}`, and a cluster endpoint `envoy_internal_address.server_listener_name` naming it. They support ordinary network filter chains; official examples chain two HCMs to decapsulate CONNECT and parse the inner HTTP. Optional internal-upstream transport transfers metadata/filter state. Separate internal listeners per vessel avoid trusting an inbound client-supplied identity header. [Internal Listener](https://www.envoyproxy.io/docs/envoy/latest/configuration/other_features/internal_listener)

TLS termination belongs on the inner listener filter chain using a `DownstreamTlsContext` with the pre-issued test certificate/key; upstream HTTPS separately uses `UpstreamTlsContext`. Neither field constitutes on-demand certificate generation. [TLS transport API](https://www.envoyproxy.io/docs/envoy/latest/api-v3/extensions/transport_sockets/tls/v3/tls.proto.html)

## Authorisation and body processing

For HTTP ext_authz, configure `authorization_response.allowed_upstream_headers` to match `authorization`: returned matching headers replace existing request headers. Do not use `allowed_upstream_headers_to_append` for credentials. HTTP check requests include Authorization by default. Override any vessel-identifying hook header in the trusted listener's `authorization_request.headers_to_add` configuration. Set `failure_mode_allow: false`; default service-error status is 403. Avoid `with_request_body` and HTTP `retry_policy` on git uploads because both introduce buffering. HTTP/gRPC auth service timeout defaults to 200 ms. [ext_authz API](https://www.envoyproxy.io/docs/envoy/latest/api-v3/extensions/filters/http/ext_authz/v3/ext_authz.proto.html)

ext_proc body mode defaults to NONE. BUFFERED retains the whole message and errors above the buffer limit; STREAMED sends pieces. Restrict BUFFERED processing to the small GraphQL route and disable body processing on git routes, so mutation refusal can happen before the full GraphQL body reaches the upstream without requiring git uploads to buffer. STREAMED may remove Content-Length and cause HTTP/1 chunked transfer encoding, so the local git HTTP server must support chunked input if that mode is tested. [Processing-mode API](https://www.envoyproxy.io/docs/envoy/latest/api-v3/extensions/filters/http/ext_proc/v3/processing_mode.proto.html)

The ext_proc hook is a bidirectional gRPC service with request/response messages for headers and selected body/trailer stages. Report hook handling and stream closure independently from ext_authz failure closure. [External Processing](https://www.envoyproxy.io/docs/envoy/latest/configuration/http/http_filters/ext_proc_filter)
