# Codex screens for #3460

Derived from the captured `codex-2927/scrolled-back.txt` screen. No live
capture of the incidents exists in this repository.

- `stray-cell-size-reply.txt`: the 2927 screen at the bottom of the transcript
  (scrollback hints removed). The composer row holds `6;24;9t`. That text is
  the reply to a cell-size query (CSI 16t, answered `CSI 6;<height>;<width> t`)
  with its `ESC [` prefix consumed, as seen on convoy
  `merged-evidence-never-stale-3396` on 2026-10-17.
- `scrolled-back-composer-hidden.txt`: the 2927 screen cut above the composer
  row. The transcript shows "↓ Back to bottom" and no live composer is
  readable, as on convoy `inbox-resident-state-3466` on 2026-10-18.
