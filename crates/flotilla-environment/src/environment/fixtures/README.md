Recorded Docker 27 inspect outputs and systemd-oomd journal from feta, supplied verbatim by the operator on 2026-10-05.

Source: https://github.com/flotilla-org/flotilla/issues/2653#issuecomment-5992767235

Outputs were trimmed by the operator to Id, Name, State and HostConfig. Do not hand-edit these recordings.

`image-layers/` is a copy of the [ADR 0062](../../../../../docs/adr/0062-crew-images-are-ordered-layer-lists-built-once-per-identity.md) stdlib layer directories authored for
the stdlib PR (flotilla-org/flotilla#3149, stack PR 3), used as real inputs for
the Dockerfile generator tests. Replace the copy when the stdlib changes.
