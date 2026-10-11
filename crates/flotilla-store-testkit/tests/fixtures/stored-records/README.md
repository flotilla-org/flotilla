# Deployed stored-record corpus

`r355/` was captured on 2026-09-28 from the live local fleet daemon with
`scripts/capture-stored-corpus r355`. It contains the spec and status payloads
returned by `flotilla resource list <kind> --json` for every registered kind.
Five kinds had no records on that host at capture time: ChangeRequest, Issue,
DispatchObservation, Regard, and Usage. Their empty files make
the coverage gap explicit. The populated kinds include convoy workflow
snapshots, standing ensures, templates, placement snapshots, checkouts, and
terminal sessions.

The script removes metadata and provenance, replaces hostnames and paths,
redacts arbitrary user text and known credential material. Review
the generated files for sensitive content before committing them.

After each fleet roll, capture the new deployed generation into a new
directory and retire the prior one. Never regenerate a corpus to pass a
schema-changing PR; fix the decoder instead, per ADR 0047.
