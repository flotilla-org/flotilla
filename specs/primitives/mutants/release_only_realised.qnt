// One overridden definition: Requested cannot be cancelled.
module release_only_realised {
  import lifecycle as Good from "../machines/lifecycle"
  pure val initial = Good::initial
  pure val ends = Good::ends
  pure def transition(s: str, edge: str, gate: bool): str =
    if (s == "Requested" and edge == "release") s else Good::transition(s, edge, gate)
}
