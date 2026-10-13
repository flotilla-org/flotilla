// A finite graph with two states; both simulation and TLC check the invariant.
module smoke {
  var bit: int
  action init = bit' = 0
  action step = bit' = 1 - bit
  val bounded = bit >= 0 and bit <= 1
}
