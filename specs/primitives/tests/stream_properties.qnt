module stream_properties {
  import stream as S from "../world/stream"
  // Enumerate both mark orders, equality, zero, partial prefixes, and link state.
  // An applied watermark is the strongest observed mark, independent of replay/order.
  run watermarkNeverRegressesTest = assert(Set(0, 1, 2).forall(a => Set(0, 1, 2).forall(b =>
    Set(false, true).forall(linked => {
      val s = { rows: [{ at: a, kind: "keepalive", session: 0 }, { at: b, kind: "keepalive", session: 0 }], applied: 2, linked: linked }
      val mark = S::watermark(s)
      S::watermark({ ...s, applied: 0 }) == 0 and S::watermark({ ...s, applied: 1 }) == a and
        mark >= a and mark >= b and Set(a, b).contains(mark)
    }))))
}
