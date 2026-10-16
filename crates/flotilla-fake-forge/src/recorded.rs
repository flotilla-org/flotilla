//! Forge worlds that flotilla's recorded Forgejo fixtures show, so the same
//! provider tests run against a recording and against the fake (#3374).
//!
//! Each [`RecordedWorld`] seeds a fake to the state its recording observed on
//! the lab Forgejo: the same repository, item numbers, titles, bodies,
//! labels, states, heads and timestamps. A recording that only shows a
//! subset of a repository (an open-only listing, say) is seeded with exactly
//! what it shows.

use std::sync::{Arc, Mutex, PoisonError};

use chrono::{DateTime, Utc};

use crate::model::{Clock, FakeForge, ForgeSettings, ItemEdit, ItemState, NewIssue, NewPull, RepoRef};

/// The lab Forgejo origin the fixtures were recorded against.
pub const RECORDED_ORIGIN: &str = "https://forgejo.lab.flotilla.work";
/// The placeholder the fixtures' masks put in place of the recording token.
pub const RECORDED_TOKEN: &str = "<LAB_FORGEJO_TOKEN>";

/// A clock set by hand, for seeding recorded timestamps and for tests.
pub struct ManualClock(Mutex<DateTime<Utc>>);

impl ManualClock {
    pub fn new(now: DateTime<Utc>) -> Self {
        Self(Mutex::new(now))
    }

    pub fn set(&self, now: DateTime<Utc>) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = now;
    }
}

impl Clock for ManualClock {
    fn now(&self) -> DateTime<Utc> {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A repository state one recorded fixture shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordedWorld {
    /// `forgejo_pulls.yaml`: `lab/flotilla` with no open pull requests.
    LabFlotillaWithoutOpenPulls,
    /// `forgejo_ghostty_governor.yaml`: `robert/ghostty-ops` before any pull
    /// request was opened.
    GhosttyOpsBeforePulls,
    /// `forgejo_head_filter_capability.yaml`: `robert/ghostty-ops` with pull
    /// request 1 merged from `governor-opus-5-5`, whose branch was then
    /// deleted, and pull request 2 merged from `drop-retired-stance`.
    GhosttyOpsWithMergedPulls,
    /// `forgejo_issues.yaml`: `fork-issues/zellij` with issues 9 and 8 open
    /// and issue 7 closed. Earlier numbers are closed issues last updated in
    /// 2025, before every window the recording reads.
    ZellijIssues,
}

impl RecordedWorld {
    pub const ALL: [Self; 4] =
        [Self::LabFlotillaWithoutOpenPulls, Self::GhosttyOpsBeforePulls, Self::GhosttyOpsWithMergedPulls, Self::ZellijIssues];

    /// The `owner/name` the recording reads.
    pub fn repo_slug(self) -> &'static str {
        match self {
            Self::LabFlotillaWithoutOpenPulls => "lab/flotilla",
            Self::GhosttyOpsBeforePulls | Self::GhosttyOpsWithMergedPulls => "robert/ghostty-ops",
            Self::ZellijIssues => "fork-issues/zellij",
        }
    }

    pub fn repository(self, forge: &str) -> RepoRef {
        let (owner, name) = self.repo_slug().split_once('/').expect("slug has an owner");
        RepoRef::new(forge, owner, name)
    }

    /// A fake at `public_url` serving this world under `forge`, accepting
    /// only `token`.
    pub fn seed(self, public_url: &str, forge: &str, token: &str) -> Arc<FakeForge> {
        let clock = Arc::new(ManualClock::new(at("2025-01-01T00:00:00Z")));
        let fake = Arc::new(FakeForge::new(public_url, clock.clone()));
        fake.add_forge(forge, ForgeSettings { token: token.into(), login: "robert".into() }).expect("recorded forge settings are valid");
        let repo = self.repository(forge);
        fake.seed_repository(&repo, "main").expect("recorded repository name is valid");
        match self {
            Self::LabFlotillaWithoutOpenPulls | Self::GhosttyOpsBeforePulls => {}
            Self::GhosttyOpsWithMergedPulls => seed_merged_pulls(&fake, &clock, &repo),
            Self::ZellijIssues => seed_zellij_issues(&fake, &clock, &repo),
        }
        fake
    }
}

fn at(time: &str) -> DateTime<Utc> {
    time.parse().expect("recorded timestamps are RFC 3339")
}

const GOVERNOR_OPUS_BODY: &str = "Moves this governor to **Opus 5.5** by declaring the model in the ops ensure entry: `agents: [governor=claude-code:claude-opus-5-5]` (`[capability=]adapter[:model]`, flotilla #2059).\n\nUntil now the live ConvoyEnsure carried a Fable override set out-of-band with `resource apply`, which any `project refresh` would have reverted to the default model. Declaring it here makes the ops file the single source.\n\n**Apply order (operator):** after flotilla-org/flotilla#2433 (crew image with claude-code 2.1.287) is merged and applied, and an Opus 5.5 smoke crew is verified, merge this, run `flotilla project refresh <project>`, then cycle the governor once so it re-admits under the new ensure.";

const DROP_STANCE_BODY: &str = "Removes the retired `stance:` field from `ops/governor.ensure.md`. Stance was replaced by the fulfilment model in flotilla-org/flotilla#2105 (2026-09-27), and the ensure parser rejects unknown fields. Since then every `flotilla project refresh` for this project has been refused with `invalid ensure entry governor: unknown field stance`. That is why the merged `agents: [governor=claude-code:claude-opus-5-5]` change never reached the live ConvoyEnsure. Placement is still `docker-crew-image-*`, which is a contained fulfilment, so nothing changes behaviourally.";

fn seed_merged_pulls(fake: &FakeForge, clock: &ManualClock, repo: &RepoRef) {
    let pulls = [
        (
            "governor-opus-5-5",
            "ops: run the governor on claude-opus-5-5",
            GOVERNOR_OPUS_BODY,
            "2026-10-02T10:34:59Z",
            "2026-10-02T11:05:02Z",
            true,
        ),
        (
            "drop-retired-stance",
            "ops: drop the retired stance field from the governor ensure",
            DROP_STANCE_BODY,
            "2026-10-02T12:05:47Z",
            "2026-10-02T12:27:20Z",
            false,
        ),
    ];
    for (head, title, body, created, merged, delete_head) in pulls {
        clock.set(at(created));
        let pull = fake
            .create_pull(
                repo,
                NewPull {
                    head: head.into(),
                    base: "main".into(),
                    title: title.into(),
                    body: body.into(),
                    user: Some("robert".into()),
                    draft: false,
                },
            )
            .expect("recorded pull request opens");
        clock.set(at(merged));
        fake.merge(repo, pull.number, "merge", None).expect("recorded pull request merges");
        if delete_head {
            fake.delete_head(repo, pull.number).expect("recorded head branch is deleted");
        }
    }
}

const SYNTHETIC_BODY: &str = "Synthetic Forgejo fixture issue used by replay tests.";

fn seed_zellij_issues(fake: &FakeForge, clock: &ManualClock, repo: &RepoRef) {
    let issue = |title: &str, labels: &[&str]| NewIssue {
        title: title.into(),
        body: SYNTHETIC_BODY.into(),
        labels: labels.iter().map(|label| label.to_string()).collect(),
        ..NewIssue::default()
    };
    let close = |number: u64| {
        fake.edit(repo, number, ItemEdit { state: Some(ItemState::Closed), ..ItemEdit::default() }).expect("recorded issue closes");
    };
    clock.set(at("2025-12-01T00:00:00Z"));
    for earlier in 1..=6 {
        let created = fake.create_issue(repo, issue(&format!("Earlier synthetic issue {earlier}"), &[])).expect("earlier issue opens");
        close(created.number);
    }
    clock.set(at("2026-07-24T09:00:00Z"));
    let closed = fake.create_issue(repo, issue("Closed synthetic Forgejo issue", &[])).expect("issue 7 opens");
    clock.set(at("2026-07-24T10:00:00Z"));
    close(closed.number);
    clock.set(at("2026-07-24T11:00:00Z"));
    fake.create_issue(repo, issue("List Forgejo issues by label", &["provider"])).expect("issue 8 opens");
    clock.set(at("2026-07-24T12:00:00Z"));
    fake.create_issue(repo, issue("Wire Forgejo issue provider into convoy startup", &["ready"])).expect("issue 9 opens");
}
