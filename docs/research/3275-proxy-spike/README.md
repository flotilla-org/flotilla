# Spike: transparent mode and per-vessel listeners over hudsucker (issue 3275)

Throwaway research code, not part of the workspace. To run it, clone hudsucker at commit 7b7baf3 into `../hudsucker` next to this directory, then run `cargo run`.

What it shows (on macOS):
- **Front-end:** about 50 lines in front of unmodified hudsucker 0.25. Each vessel gets its own listener, a Unix socket or a TCP port. The front-end reads the TLS ClientHello for the SNI name, then opens a loopback connection to hudsucker and records that connection's local address against the vessel. It sends a synthetic CONNECT, replays the bytes it already read, and splices the streams.
- **Policy:** an `HttpHandler` looks up the vessel from `ctx.client_addr` and swaps the placeholder bearer token for that vessel's real one. A placeholder from the wrong vessel, or an unknown one, gets 403. A GraphQL mutation gets 403 while a query passes; only `/graphql` bodies are buffered, capped at 1 MB.
- **Streaming:** a chunked POST arrives upstream unbuffered (two chunks 600 ms apart arrive 601 ms apart).
- **HTTP/2:** HTTP/1 and h2 clients both work.
- **Linux:** a `SO_ORIGINAL_DST` path is behind `cfg(target_os = "linux")` and has not been compiled or run.

Findings and the comparison with alternatives are on issue 3275.
