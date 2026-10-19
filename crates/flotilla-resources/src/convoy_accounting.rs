//! Shared live-crew accounting for convoy consumers.
/// Settled roles release share before landing; unpublished crew state counts one.
pub fn active_crew_count(status: Option<&crate::ConvoyStatus>) -> usize {
    if status.is_some_and(|status| status.phase.is_terminal()) {
        return 0;
    }
    let Some(status) = status.filter(|status| status.crew_work.values().any(|crew| !crew.is_empty())) else {
        return 1;
    };
    status
        .crew_work
        .values()
        .flat_map(|crew| crew.values())
        .filter(|state| {
            matches!(
                state.phase,
                crate::CrewWorkPhase::Pending
                    | crate::CrewWorkPhase::Working
                    | crate::CrewWorkPhase::Interrupted
                    | crate::CrewWorkPhase::Stalled
            )
        })
        .count()
}

#[cfg(test)]
mod tests {
    use super::active_crew_count;
    use crate::ConvoyPhase;
    use std::collections::BTreeMap;
    // #2783 admission inputs count unsettled roles, not completed obligations.
    // Exhaustive finite enum mapping: every phase, unknown publication, terminal
    // convoy, and mixed roles. No process boundary or random interleaving exists.
    #[test]
    fn settled_roles_release_share_before_the_convoy_lands() {
        use crate::{ConvoyStatus, CrewWorkPhase, CrewWorkState};
        assert_eq!(active_crew_count(None), 1);
        assert_eq!(active_crew_count(Some(&ConvoyStatus::default())), 1);
        for (phase, unsettled) in [
            (CrewWorkPhase::Pending, true),
            (CrewWorkPhase::Working, true),
            (CrewWorkPhase::Interrupted, true),
            (CrewWorkPhase::Stalled, true),
            (CrewWorkPhase::Done, false),
            (CrewWorkPhase::HandedBack, false),
            (CrewWorkPhase::Failed, false),
        ] {
            let mut status = ConvoyStatus::default();
            status.crew_work.insert(
                "work".into(),
                BTreeMap::from([
                    ("coder".into(), CrewWorkState::builder().phase(phase).build()),
                    ("reviewer".into(), CrewWorkState::builder().phase(CrewWorkPhase::Working).build()),
                ]),
            );
            assert_eq!(active_crew_count(Some(&status)), 1 + usize::from(unsettled));
            status.crew_work.get_mut("work").expect("work").remove("reviewer");
            assert_eq!(active_crew_count(Some(&status)), usize::from(unsettled));
            status.phase = ConvoyPhase::Landed;
            assert_eq!(active_crew_count(Some(&status)), 0);
        }
    }
}
