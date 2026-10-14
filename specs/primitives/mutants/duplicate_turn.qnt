module duplicate_turn {
  import adapter as Good from "../world/adapter"
  pure def bind(unbound: bool, live: bool, receiptsDrained: bool): bool = Good::bind(unbound, live, receiptsDrained)
  pure def turn(pending: bool, bound: bool, live: bool, alive: bool, alreadyTurned: bool): bool =
    pending and bound and live and alive
}
