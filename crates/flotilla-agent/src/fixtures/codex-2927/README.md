# Captured Codex screens for #2927

Source: feta's contained-cleat recording for convoy
`convoy-0d576c49af2544c58b459f8785553e68`, work/coder:
`env-convoy-0d576c49af2544c58b459f8785553e68-work/default@1/sessions/terminal-convoy-0d576c49af2544c58b459f8785553e68-work-coder/session.cast`.
The operator supplied the asciicast v3 recording on 2026-10-08.
Its header timestamp is 1791455672; the cast clock runs about 44 seconds
behind daemon logs. The original VT engine was Ghostty.

These captures (with only right-hand row padding removed) were rendered through `cleat launch --size 184x37`
and `cleat capture` using Ghostty. Output events were replayed in order up to
the timestamps below, summing v3 event deltas. The replay process used raw
terminal input so VT query replies could not echo into the captured screen.
The recording's earlier 200x50 history was replayed into the final 184x37
screen; Codex's later full redraw supplies the captured layout.

- `scrolled-back.txt`: 2026-10-08 11:58:00 UTC. Idle composer below the
  scrolled transcript; the two New activity hints remain visible.
- `working-after-release.txt`: 2026-10-08 15:16:30 UTC. Working after the
  attached client leaves scrollback and the stale input is pasted.

The full 10 MB cast is intentionally not committed.
