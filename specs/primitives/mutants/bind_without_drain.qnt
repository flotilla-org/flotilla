module bind_without_drain {
  import adapter as Good from "../world/adapter"
  pure def turn(pending: bool, bound: bool, live: bool, alive: bool, alreadyTurned: bool): bool = Good::turn(pending, bound, live, alive, alreadyTurned)
  pure def bind(unbound: bool, live: bool, receiptsDrained: bool): bool = unbound and live
}
