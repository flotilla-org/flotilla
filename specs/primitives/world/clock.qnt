module clock {
  pure def tick(now: int, bound: int): int = if (now < bound) now + 1 else now
}
