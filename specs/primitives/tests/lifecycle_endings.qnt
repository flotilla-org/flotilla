module lifecycle_endings {
  import lifecycle(IDS = Set()) as L from "../machines/lifecycle"

  // A completed teardown and an observed disappearance retain distinct terminal evidence.
  run releaseRetainsItsEndingTest = assert(
    L::step(L::Realised("backing"), L::Release({ teardownComplete: true })) == L::Moved(L::Gone(L::Released)))

  // Both disappearance causes survive the transition, including an arbitrary failure reason.
  run vanishRetainsItsCauseTest = assert(Set(L::Lost, L::Failed("provider stopped"), L::Failed("")).forall(c =>
    L::step(L::Realised("backing"), L::Vanish({ absent: true, cause: c })) == L::Moved(L::Gone(L::Vanished({ cause: c })))))

  // Failure before readiness retains its last attempt's cause and cannot be mistaken for a release.
  run provisioningFailureRetainsItsCauseTest = assert(Set("provider refused", "").forall(c => {
    val failure = L::AttemptFailed({ retryable: false, cause: c })
    L::step(L::Requested([failure]), L::GiveUp({ cause: c })) == L::Moved(L::GaveUp(c))
  }))

  // Neither terminal ending can be revived; missing absence evidence cannot end a live backing.
  run terminalEndingsRefuseTest = assert(Set(L::Released, L::Vanished({ cause: L::Lost }), L::Vanished({ cause: L::Failed("failure") })).forall(e =>
    L::terminalRefuses(L::Gone(e))))
  run missingAbsenceRefusesTest = assert(Set(L::Lost, L::Failed("failure")).forall(c =>
    L::step(L::Realised("backing"), L::Vanish({ absent: false, cause: c })) == L::Refused(L::MissingEvidence)))
}
