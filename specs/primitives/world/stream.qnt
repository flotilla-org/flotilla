// Each stream belongs to one home. Apply a prefix in order; never leap over a row.
module stream {
  type Row = { at: int, kind: str, session: int }
  type Stream = { rows: List[Row], applied: int, linked: bool }
  pure val initial: Stream = { rows: [], applied: 0, linked: true }
  pure def publish(s: Stream, row: Row): Stream = { ...s, rows: s.rows.append(row) }
  pure def apply(s: Stream): Stream =
    if (s.linked and s.applied < s.rows.length()) { ...s, applied: s.applied + 1 } else s
  pure def link(s: Stream, up: bool): Stream = { ...s, linked: up }
  pure def seen(s: Stream, kind: str, session: int): bool =
    s.rows.indices().exists(i => i < s.applied and s.rows[i].kind == kind and s.rows[i].session == session)
  pure def watermark(s: Stream): int = s.rows.indices().fold(0, (mark, i) =>
    if (i < s.applied and s.rows[i].kind == "keepalive" and s.rows[i].at > mark) s.rows[i].at else mark)
}
