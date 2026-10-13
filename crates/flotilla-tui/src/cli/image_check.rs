//! `flotilla image check`: run the check, then (unless `--no-wait`) poll its
//! ImageBuild until it settles, and report identity, builder, digest and a
//! per-layer probe table.
use std::{future::Future, time::Duration};

use comfy_table::{presets::UTF8_FULL_CONDENSED, Cell, Table};
use flotilla_daemon_api::daemon::DaemonHandle;
use flotilla_protocol::{
    image_check::{ImageBuildReport, ImageBuildReportPhase, ImageCheckAgentSource, ImageCheckReport, ImageProbeReport},
    output::OutputFormat,
    Command, CommandAction, CommandValue, DaemonEvent,
};

/// How often a waiting check re-reads its build.
pub const IMAGE_CHECK_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// A finished check: the report, and whether waiting gave up first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageCheckOutcome {
    pub report: ImageCheckReport,
    pub timed_out: bool,
}

impl ImageCheckOutcome {
    /// Refused, failed, or timed out: the CLI exits non-zero.
    pub fn failed(&self) -> bool {
        self.timed_out || self.report.failed()
    }
}

/// Execute `command` (an `ImageCheck`), wait up to `wait` for its build to
/// settle, and print the final report.
pub async fn run_image_check(
    daemon: &dyn DaemonHandle,
    command: Command,
    wait: Option<Duration>,
    format: OutputFormat,
) -> Result<ImageCheckOutcome, String> {
    let report = execute_image_check(daemon, command).await?;
    let outcome = match wait {
        Some(timeout) => {
            let progress = matches!(format, OutputFormat::Human);
            wait_for_build(
                report,
                timeout,
                IMAGE_CHECK_POLL_INTERVAL,
                |name| async move { query_image_build(daemon, name).await },
                |build| {
                    if progress {
                        eprintln!("image build {}: {} (attempt {})", build.name, build.phase.as_str(), build.attempt);
                    }
                },
            )
            .await?
        }
        None => ImageCheckOutcome { report, timed_out: false },
    };
    match format {
        OutputFormat::Human => print!("{}", format_image_check_human(&outcome)),
        OutputFormat::Json => println!("{}", flotilla_protocol::output::json_pretty(&outcome.report)),
    }
    if outcome.timed_out {
        eprintln!("timed out waiting for the image build to finish; it continues on its builder");
    }
    Ok(outcome)
}

async fn execute_image_check(daemon: &dyn DaemonHandle, command: Command) -> Result<ImageCheckReport, String> {
    let mut events = daemon.subscribe();
    let command_id = daemon.execute(command).await?;
    loop {
        match events.recv().await {
            Ok(DaemonEvent::CommandFinished { command_id: id, result, .. }) if id == command_id => {
                return match result {
                    CommandValue::ImageCheck(report) => Ok(*report),
                    CommandValue::Error { message } => Err(message),
                    CommandValue::Cancelled => Err("command cancelled".into()),
                    other => Err(format!("unexpected image check result: {other:?}")),
                };
            }
            Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return Err("daemon disconnected".into()),
        }
    }
}

async fn query_image_build(daemon: &dyn DaemonHandle, name: String) -> Result<ImageBuildReport, String> {
    let command = Command::builder().action(CommandAction::QueryImageBuild { name }).build();
    match daemon.execute_query(command, uuid::Uuid::new_v4()).await? {
        CommandValue::ImageBuild(report) => Ok(*report),
        CommandValue::Error { message } => Err(message),
        other => Err(format!("unexpected image build result: {other:?}")),
    }
}

/// Poll the report's build every `interval` until it settles or `timeout`
/// passes, reporting each phase change. A refused check (no build) and an
/// already-settled build return at once.
pub async fn wait_for_build<F, Fut>(
    mut report: ImageCheckReport,
    timeout: Duration,
    interval: Duration,
    mut poll: F,
    mut on_change: impl FnMut(&ImageBuildReport),
) -> Result<ImageCheckOutcome, String>
where
    F: FnMut(String) -> Fut,
    Fut: Future<Output = Result<ImageBuildReport, String>>,
{
    let deadline = tokio::time::Instant::now() + timeout;
    let Some(mut build) = report.build.clone() else { return Ok(ImageCheckOutcome { report, timed_out: false }) };
    let mut last = (build.phase, build.attempt);
    while !build.settled {
        if tokio::time::Instant::now() + interval > deadline {
            report.build = Some(build);
            return Ok(ImageCheckOutcome { report, timed_out: true });
        }
        tokio::time::sleep(interval).await;
        build = poll(build.name.clone()).await?;
        if (build.phase, build.attempt) != last {
            last = (build.phase, build.attempt);
            on_change(&build);
        }
    }
    report.build = Some(build);
    Ok(ImageCheckOutcome { report, timed_out: false })
}

fn probe_label(probe: &ImageProbeReport) -> String {
    match probe {
        ImageProbeReport::Pending => "pending".into(),
        ImageProbeReport::Passed => "passed".into(),
        ImageProbeReport::Failed { reason } => format!("failed: {reason}"),
        ImageProbeReport::NotRun => "not run".into(),
    }
}

/// The human report: the ref and project layer read, refusals or the build
/// with its per-layer probe table, and the failure with its build log.
pub fn format_image_check_human(outcome: &ImageCheckOutcome) -> String {
    let report = &outcome.report;
    let mut out = format!("image check: project {} at {}", report.project, report.git_ref);
    if let Some(revision) = &report.revision {
        out.push_str(&format!(" ({})", revision.get(..12).unwrap_or(revision)));
    }
    out.push('\n');
    out.push_str(&format!("project layer: {}\n", report.project_layer.as_deref().unwrap_or("none at this ref")));
    let agents = if report.agents.is_empty() { "none".to_string() } else { report.agents.join(", ") };
    let agents_from = match report.agents_from {
        ImageCheckAgentSource::Requested => "--agent",
        ImageCheckAgentSource::DefaultWorkflow => "default workflow",
    };
    out.push_str(&format!("agents: {agents} ({agents_from})\n"));
    if !report.refusals.is_empty() {
        out.push_str("refused (no build created):\n");
        for refusal in &report.refusals {
            if refusal.layers.is_empty() {
                out.push_str(&format!("  - {}\n", refusal.message));
            } else {
                out.push_str(&format!("  - [{}] {}\n", refusal.layers.join(", "), refusal.message));
            }
        }
        return out;
    }
    for shadow in &report.shadowed {
        out.push_str(&format!("shadowed: {} ({} hides {} {})\n", shadow.name, shadow.source, shadow.shadowed, shadow.versions.join(", ")));
    }
    let Some(build) = &report.build else { return out };
    out.push_str(&format!("identity: {}\n", build.identity));
    out.push_str(&format!(
        "build: {} ({}) on {}, {}{}\n",
        build.name,
        build.architecture,
        build.builder_host,
        build.delivery,
        if report.reused { "; reused existing build" } else { "" }
    ));
    let pending = if build.settled { "" } else { ", not finished" };
    out.push_str(&format!("phase: {} (attempt {}){pending}\n", build.phase.as_str(), build.attempt));
    out.push_str(&format!("digest: {}\n", build.digest.as_deref().unwrap_or("-")));
    let mut table = Table::new();
    table.load_preset(UTF8_FULL_CONDENSED);
    table.set_header(vec!["Layer", "Source", "Version", "Probe"]);
    for layer in &build.layers {
        table.add_row(vec![
            Cell::new(&layer.layer),
            Cell::new(&layer.source),
            Cell::new(&layer.version),
            Cell::new(probe_label(&layer.probe)),
        ]);
    }
    out.push_str(&format!("{table}\n"));
    if let Some(failure) = &build.failure {
        out.push_str(&format!("failure ({}): {}\n", failure.class, failure.reason));
        if build.phase == ImageBuildReportPhase::Failed && failure.class == "recipe" {
            out.push_str(&format!(
                "hint: recipe failures are not retried for this identity; changing a layer changes the identity \
                 (or delete ImageBuild {} to rebuild it)\n",
                build.name
            ));
        }
    }
    if let Some(log_ref) = &build.log_ref {
        out.push_str(&format!("build log: {log_ref}\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{Arc, Mutex},
        time::Duration,
    };

    use flotilla_protocol::image_check::{
        ImageBuildFailureReport, ImageBuildReport, ImageBuildReportPhase, ImageCheckAgentSource, ImageCheckRefusal, ImageCheckReport,
        ImageLayerReport, ImageProbeReport,
    };

    use super::{format_image_check_human, wait_for_build, ImageCheckOutcome};

    fn layer(name: &str, source: &str, probe: ImageProbeReport) -> ImageLayerReport {
        ImageLayerReport { layer: name.into(), source: source.into(), version: "1.0".into(), tier: "shared".into(), probe }
    }

    fn build(phase: ImageBuildReportPhase, settled: bool, attempt: u32) -> ImageBuildReport {
        ImageBuildReport {
            name: "image-0123abcd-amd64".into(),
            identity: "sha256:0123abcd".into(),
            architecture: "amd64".into(),
            builder_host: "feta".into(),
            delivery: "registry registry.test/flotilla/crew-images".into(),
            phase,
            settled,
            attempt,
            digest: None,
            layers: vec![layer("base", "stdlib", ImageProbeReport::Pending), layer("app-native", "project", ImageProbeReport::Pending)],
            failure: None,
            log_ref: None,
        }
    }

    fn report(build: Option<ImageBuildReport>) -> ImageCheckReport {
        ImageCheckReport {
            project: "app".into(),
            git_ref: "feature/native".into(),
            revision: Some("0123456789abcdef0123".into()),
            project_layer: Some("app-native@1.1".into()),
            agents: vec!["codex".into()],
            agents_from: ImageCheckAgentSource::DefaultWorkflow,
            refusals: Vec::new(),
            shadowed: Vec::new(),
            build,
            reused: true,
        }
    }

    fn failed_build() -> ImageBuildReport {
        let mut failed = build(ImageBuildReportPhase::Failed, true, 1);
        failed.layers = vec![
            layer("base", "stdlib", ImageProbeReport::Passed),
            layer("app-native", "project", ImageProbeReport::Failed { reason: "app: not found".into() }),
        ];
        failed.failure = Some(ImageBuildFailureReport {
            class: "recipe".into(),
            reason: "probe failed (owner: layer `app-native` 1.1 from project `app`)".into(),
        });
        failed.log_ref = Some("artifact-build-log".into());
        failed
    }

    // Human output names the build, its builder and digest, tabulates each
    // layer's source, version and probe, and for a recipe failure gives the
    // class, reason, build log and the identity hint.
    #[test]
    fn human_report_tabulates_layers_and_explains_a_recipe_failure() {
        let outcome = ImageCheckOutcome { report: report(Some(failed_build())), timed_out: false };
        assert!(outcome.failed());
        let text = format_image_check_human(&outcome);
        insta::assert_snapshot!(text, @r"
        image check: project app at feature/native (0123456789ab)
        project layer: app-native@1.1
        agents: codex (default workflow)
        identity: sha256:0123abcd
        build: image-0123abcd-amd64 (amd64) on feta, registry registry.test/flotilla/crew-images; reused existing build
        phase: failed (attempt 1)
        digest: -
        ┌────────────┬─────────┬─────────┬────────────────────────┐
        │ Layer      ┆ Source  ┆ Version ┆ Probe                  │
        ╞════════════╪═════════╪═════════╪════════════════════════╡
        │ base       ┆ stdlib  ┆ 1.0     ┆ passed                 │
        │ app-native ┆ project ┆ 1.0     ┆ failed: app: not found │
        └────────────┴─────────┴─────────┴────────────────────────┘
        failure (recipe): probe failed (owner: layer `app-native` 1.1 from project `app`)
        hint: recipe failures are not retried for this identity; changing a layer changes the identity (or delete ImageBuild image-0123abcd-amd64 to rebuild it)
        build log: artifact-build-log
        ");
    }

    // A refused check lists each refusal with the layers it names, and no build.
    #[test]
    fn human_report_lists_refusals() {
        let mut refused = report(None);
        refused.refusals = vec![
            ImageCheckRefusal { layers: Vec::new(), message: "unresolvable image need `toolchain:nope`".into() },
            ImageCheckRefusal {
                layers: vec!["zig-a".into(), "zig-b".into()],
                message: "duplicate providers for `toolchain:zig`: zig-a, zig-b".into(),
            },
        ];
        let outcome = ImageCheckOutcome { report: refused, timed_out: false };
        assert!(outcome.failed());
        let text = format_image_check_human(&outcome);
        assert!(text.ends_with(
            "refused (no build created):\n  - unresolvable image need `toolchain:nope`\n  - [zig-a, zig-b] duplicate providers for `toolchain:zig`: zig-a, zig-b\n"
        ), "{text}");
        assert!(!text.contains("identity:"));
    }

    // JSON output is the report itself: probe outcomes are tagged, absent
    // values are omitted, and it decodes back unchanged.
    #[test]
    fn json_report_round_trips() {
        let report = report(Some(failed_build()));
        let json = serde_json::to_value(&report).expect("encode");
        assert_eq!(json["build"]["layers"][1]["probe"], serde_json::json!({ "outcome": "failed", "reason": "app: not found" }));
        assert_eq!(json["build"]["phase"], "failed");
        assert_eq!(json["agents_from"], "default_workflow");
        assert!(json.get("refusals").is_none() && json["build"].get("digest").is_none());
        assert_eq!(serde_json::from_value::<ImageCheckReport>(json).expect("decode"), report);
    }

    // Waiting polls the build until it settles, reporting each phase change;
    // an infrastructure failure with retries left is not settled.
    #[tokio::test(start_paused = true)]
    async fn waiting_polls_until_the_build_settles() {
        let polls = Arc::new(Mutex::new(vec![
            build(ImageBuildReportPhase::Building, false, 1),
            build(ImageBuildReportPhase::Failed, false, 1),
            build(ImageBuildReportPhase::Queued, false, 1),
            build(ImageBuildReportPhase::Building, false, 2),
            failed_build(),
        ]));
        let changes = Arc::new(Mutex::new(Vec::new()));
        let outcome = wait_for_build(
            report(Some(build(ImageBuildReportPhase::Queued, false, 0))),
            Duration::from_secs(600),
            Duration::from_secs(2),
            |name| {
                let polls = Arc::clone(&polls);
                async move {
                    assert_eq!(name, "image-0123abcd-amd64");
                    Ok(polls.lock().expect("polls").remove(0))
                }
            },
            |build| changes.lock().expect("changes").push((build.phase, build.attempt)),
        )
        .await
        .expect("wait");
        assert!(!outcome.timed_out);
        assert_eq!(outcome.report.build, Some(failed_build()));
        assert!(polls.lock().expect("polls").is_empty());
        assert_eq!(
            *changes.lock().expect("changes"),
            [
                (ImageBuildReportPhase::Building, 1),
                (ImageBuildReportPhase::Failed, 1),
                (ImageBuildReportPhase::Queued, 1),
                (ImageBuildReportPhase::Building, 2),
                (ImageBuildReportPhase::Failed, 1),
            ]
        );
    }

    // A build that does not settle in time returns its last state, timed out.
    #[tokio::test(start_paused = true)]
    async fn waiting_gives_up_at_the_timeout() {
        let polls = Arc::new(Mutex::new(0));
        let outcome = wait_for_build(
            report(Some(build(ImageBuildReportPhase::Queued, false, 0))),
            Duration::from_secs(10),
            Duration::from_secs(2),
            |_| {
                let polls = Arc::clone(&polls);
                async move {
                    *polls.lock().expect("polls") += 1;
                    Ok(build(ImageBuildReportPhase::Building, false, 1))
                }
            },
            |_| {},
        )
        .await
        .expect("wait");
        assert!(outcome.timed_out && outcome.failed());
        assert_eq!(outcome.report.build.expect("build").phase, ImageBuildReportPhase::Building);
        assert_eq!(*polls.lock().expect("polls"), 5);
        // A refused check has nothing to wait for.
        let refused = wait_for_build(report(None), Duration::ZERO, Duration::from_secs(2), |_| async { Err("unused".to_string()) }, |_| {})
            .await
            .expect("refused");
        assert!(!refused.timed_out);
    }
}
