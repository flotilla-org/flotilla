These command outputs were recorded on 2026-10-05 from the actual pinned cleat
CLI, revision `00c072b207dc943f6c93fe3b6b09abaa257695a6`, using private temporary
runtime roots. No host or crew daemon was drained.

For success/unchanged/warning, an HTTP-over-Unix-socket stand-in for the old
generation enforced the pinned daemon contract: `GET /` returns build status,
`GET /sessions` returns the session list, and `POST /drain` returns draining
status (success) or 404 (warning). The success/warning old build reported the
previous revision `fd66a7121149e85eb8fbc57a99a388b0c91dae6c` and protocol 11;
unchanged reported the installed build. Success and warning launched a real
successor daemon from the installed CLI and captured its actual build metadata.
For nonzero, no daemon was listening in the private root.

The recorded invocation was:
`cleat --runtime-root <private-root> --server default server drain --json`.
The files retain stdout, stderr, and successful/unsuccessful exit classification
(the injected CommandRunner's output contract). Report JSON is captured exactly
as emitted, including warnings and all embedded build fields.
