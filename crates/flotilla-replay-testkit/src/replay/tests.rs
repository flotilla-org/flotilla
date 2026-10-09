macro_rules! run {
    ($runner:expr, $cmd:expr, $args:expr, $cwd:expr) => {
        $runner.run($cmd, $args, $cwd, &flotilla_core::providers::command_channel_label($cmd, $args)).await
    };
    ($runner:expr, $cmd:expr, $args:expr, $cwd:expr, $labeler:expr) => {
        $runner.run($cmd, $args, $cwd, &flotilla_core::providers::command_channel_label_with::<true, _>($cmd, $args, &$labeler)).await
    };
}
use super::*;

#[test]
fn masks_substitute_and_reverse() {
    let mut masks = Masks::new();
    masks.add("/Users/bob/dev/repo", "{repo}");
    masks.add("/Users/bob", "{home}");

    assert_eq!(masks.mask("/Users/bob/dev/repo/src"), "{repo}/src");
    assert_eq!(masks.unmask("{repo}/src"), "/Users/bob/dev/repo/src");
    // Ordering matters: longer match first
    assert_eq!(masks.mask("/Users/bob/.config"), "{home}/.config");
}

#[test]
fn yaml_round_trip() {
    let log = InteractionLog {
        interactions: vec![
            Interaction::Command {
                label: None,
                cmd: "git".into(),
                args: vec!["status".into()],
                cwd: "{repo}".into(),
                stdout: Some("clean\n".into()),
                stderr: None,
                exit_code: Some(0),
                error: None,
            },
            Interaction::GhApi {
                label: None,
                method: "GET".into(),
                endpoint: "/repos/owner/repo/pulls".into(),
                status: 200,
                body: "[]".into(),
                headers: HashMap::new(),
            },
        ],
    };

    let yaml = serde_yml::to_string(&log).unwrap();
    let parsed: InteractionLog = serde_yml::from_str(&yaml).unwrap();
    assert_eq!(parsed.interactions.len(), 2);
}

#[tokio::test]
async fn replay_runner_with_git_vcs() {
    let yaml = r#"
interactions:
  - channel: command
    cmd: git
    args: ["branch", "--list", "--format=%(refname:short)"]
    cwd: "{repo}"
    stdout: "main\nfeature/foo\n"
    exit_code: 0
"#;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.yaml");
    std::fs::write(&path, yaml).unwrap();

    let mut masks = Masks::new();
    masks.add("/test/repo", "{repo}");
    let session = Session::replaying(&path, masks);
    let runner = Arc::new(ReplayRunner::new(session.clone()));

    use flotilla_core::providers::vcs::{git::GitVcs, VcsInspection};
    use flotilla_paths::path_context::ExecutionEnvironmentPath;
    let git = GitVcs::new(runner);
    let repo = ExecutionEnvironmentPath::new("/test/repo");
    let branches = git.list_local_branches(&repo).await.unwrap();

    assert_eq!(branches.len(), 2);
    assert_eq!(branches[0].name, "main");
    assert!(branches[0].is_trunk);
    assert_eq!(branches[1].name, "feature/foo");
    assert!(!branches[1].is_trunk);
}

#[test]
fn replay_session_serves_in_order() {
    let log = InteractionLog {
        interactions: vec![Interaction::Command {
            label: None,
            cmd: "git".into(),
            args: vec!["status".into()],
            cwd: "{repo}".into(),
            stdout: Some("ok\n".into()),
            stderr: None,
            exit_code: Some(0),
            error: None,
        }],
    };

    let yaml = serde_yml::to_string(&log).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.yaml");
    std::fs::write(&path, &yaml).unwrap();

    let mut masks = Masks::new();
    masks.add("/real/repo", "{repo}");
    let session = Session::replaying(&path, masks);

    let interaction = session.next(&ChannelLabel::Command("git status".into()));
    match interaction {
        Interaction::Command { cmd, cwd, .. } => {
            assert_eq!(cmd, "git");
            assert_eq!(cwd, "/real/repo");
        }
        _ => panic!("expected command"),
    }
    session.assert_complete();
}

#[tokio::test]
async fn replay_gh_api_get() {
    let yaml = r#"
interactions:
  - channel: gh_api
    method: GET
    endpoint: "/repos/owner/repo/pulls?state=all&per_page=100"
    status: 200
    body: '[{"number": 42, "title": "Fix bug"}]'
"#;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.yaml");
    std::fs::write(&path, yaml).unwrap();

    let session = Session::replaying(&path, Masks::new());
    let api = ReplayGhApi::new(session.clone());

    let endpoint = "/repos/owner/repo/pulls?state=all&per_page=100";
    let label = ChannelLabel::GhApi(endpoint.to_string());
    let result = api.get(endpoint, Path::new("/repo"), &label).await;
    assert!(result.is_ok());
    assert!(result.unwrap().contains("Fix bug"));
    session.assert_complete();
}

#[tokio::test]
async fn replay_gh_api_get_non_2xx_returns_err() {
    let yaml = r#"
interactions:
  - channel: gh_api
    method: GET
    endpoint: "/repos/owner/repo/pulls"
    status: 404
    body: '{"message": "Not Found"}'
"#;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.yaml");
    std::fs::write(&path, yaml).unwrap();

    let session = Session::replaying(&path, Masks::new());
    let api = ReplayGhApi::new(session.clone());

    let endpoint = "/repos/owner/repo/pulls";
    let label = ChannelLabel::GhApi(endpoint.to_string());
    let result = api.get(endpoint, Path::new("/repo"), &label).await;
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("404"));
    session.assert_complete();
}

#[tokio::test]
async fn replay_gh_api_preserves_rate_limit_reset_time() {
    let yaml = r#"
interactions:
  - channel: gh_api
    method: GET
    endpoint: "/repos/owner/repo/issues/42"
    status: 403
    body: '{"message": "API rate limit exceeded"}'
    headers:
      X-RateLimit-Reset: "1784822400"
"#;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.yaml");
    std::fs::write(&path, yaml).unwrap();

    let session = Session::replaying(&path, Masks::new());
    let api = ReplayGhApi::new(session.clone());
    let endpoint = "/repos/owner/repo/issues/42";
    let label = ChannelLabel::GhApi(endpoint.to_string());

    let error = api.get(endpoint, Path::new("/repo"), &label).await.expect_err("403 must fail");

    assert_eq!(error, "github rate limited (budget=REST core, identity=host gh login, reset_at=2026-07-23T16:00:00+00:00)");
    session.assert_complete();
}

#[tokio::test]
async fn replay_gh_api_get_with_headers() {
    let yaml = r#"
interactions:
  - channel: gh_api
    method: GET
    endpoint: "/repos/owner/repo/issues?per_page=100"
    status: 200
    body: '[{"number": 1}]'
    headers:
      etag: 'W/"abc123"'
      has_next_page: "true"
      total_count: "42"
"#;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.yaml");
    std::fs::write(&path, yaml).unwrap();

    let session = Session::replaying(&path, Masks::new());
    let api = ReplayGhApi::new(session.clone());

    let endpoint = "/repos/owner/repo/issues?per_page=100";
    let label = ChannelLabel::GhApi(endpoint.to_string());
    let result = api.get_with_headers(endpoint, Path::new("/repo"), &label).await;
    assert!(result.is_ok());
    let resp = result.unwrap();
    assert_eq!(resp.status, 200);
    assert_eq!(resp.etag, Some("W/\"abc123\"".to_string()));
    assert!(resp.body.contains("number"));
    assert!(resp.has_next_page);
    assert_eq!(resp.total_count, Some(42));
    session.assert_complete();
}

#[tokio::test]
async fn replay_gh_api_get_with_headers_no_pagination() {
    let yaml = r#"
interactions:
  - channel: gh_api
    method: GET
    endpoint: "/repos/owner/repo/issues"
    status: 200
    body: '[]'
"#;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.yaml");
    std::fs::write(&path, yaml).unwrap();

    let session = Session::replaying(&path, Masks::new());
    let api = ReplayGhApi::new(session.clone());

    let endpoint = "/repos/owner/repo/issues";
    let label = ChannelLabel::GhApi(endpoint.to_string());
    let result = api.get_with_headers(endpoint, Path::new("/repo"), &label).await;
    assert!(result.is_ok());
    let resp = result.unwrap();
    assert_eq!(resp.status, 200);
    assert_eq!(resp.etag, None);
    assert!(!resp.has_next_page);
    assert_eq!(resp.total_count, None);
    session.assert_complete();
}

#[tokio::test]
async fn record_then_replay() {
    use super::testing::MockRunner;

    let dir = tempfile::tempdir().unwrap();
    let fixture_path = dir.path().join("recorded.yaml");

    // Record phase: use MockRunner as the "real" backend
    {
        let mock = Arc::new(MockRunner::new(vec![Ok("hello\n".into()), Err("not found".into())]));
        let session = Session::recording(&fixture_path, Masks::new());
        let recorder = RecordingRunner::new(session.clone(), mock);

        let r1 = run!(recorder, "echo", &["hello"], Path::new("/tmp"));
        assert!(r1.is_ok());

        let r2 = run!(recorder, "missing", &[], Path::new("/tmp"));
        assert!(r2.is_err());

        session.finish();
    }

    // Replay phase: verify the recorded fixture works
    {
        let session = Session::replaying(&fixture_path, Masks::new());
        let runner = ReplayRunner::new(session.clone());

        let r1 = run!(runner, "echo", &["hello"], Path::new("/tmp"));
        assert_eq!(r1.unwrap(), "hello\n");

        let r2 = run!(runner, "missing", &[], Path::new("/tmp"));
        assert!(r2.is_err());

        session.assert_complete();
    }
}

#[tokio::test]
async fn replay_http_client_round_trip() {
    use flotilla_core::providers::HttpClient;

    let yaml = r#"
interactions:
  - channel: http
    method: GET
    url: "https://example.test/v1/sessions"
    request_headers:
      authorization: "Bearer token-1"
      anthropic-version: "2023-06-01"
    status: 200
    response_body: '{"data":[]}'
"#;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("http.yaml");
    std::fs::write(&path, yaml).unwrap();

    let session = Session::replaying(&path, Masks::new());
    let client = ReplayHttpClient::new(session.clone());

    let request = flotilla_core::tls::client()
        .get("https://example.test/v1/sessions")
        .header("authorization", "Bearer token-1")
        .header("anthropic-version", "2023-06-01")
        .build()
        .unwrap();

    let label = ChannelLabel::http_from_url("https://example.test/v1/sessions");
    let response = client.execute(request, &label).await.expect("replay should work");
    assert_eq!(response.status().as_u16(), 200);
    assert_eq!(response.body().as_ref(), br#"{"data":[]}"#);
    session.assert_complete();
}

#[test]
fn multi_channel_round_allows_any_consumption_order() {
    // Fixture has command and http in same round
    let yaml = r#"
interactions:
  - channel: command
    cmd: git
    args: ["status"]
    cwd: "/repo"
    stdout: "ok\n"
    exit_code: 0
  - channel: http
    method: GET
    url: "https://api.test/v1/sessions"
    status: 200
    response_body: '{"data":[]}'
  - channel: command
    cmd: git
    args: ["log"]
    cwd: "/repo"
    stdout: "abc\n"
    exit_code: 0
"#;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.yaml");
    std::fs::write(&path, yaml).unwrap();

    let session = Session::replaying(&path, Masks::new());

    // Consume http FIRST (before any commands) — this is the key test.
    // With the old linear cursor this would have panicked.
    let http = session.next(&ChannelLabel::Http("api.test".into()));
    assert!(matches!(http, Interaction::Http { .. }));

    // Now consume commands in order
    let cmd1 = session.next(&ChannelLabel::Command("git status".into()));
    match cmd1 {
        Interaction::Command { args, .. } => assert_eq!(args[0], "status"),
        _ => panic!("expected command"),
    }

    let cmd2 = session.next(&ChannelLabel::Command("git log".into()));
    match cmd2 {
        Interaction::Command { args, .. } => assert_eq!(args[0], "log"),
        _ => panic!("expected command"),
    }

    session.finish();
}

#[test]
fn channel_label_from_interaction() {
    let cmd = Interaction::Command {
        label: None,
        cmd: "git".into(),
        args: vec!["status".into()],
        cwd: "/repo".into(),
        stdout: Some("ok\n".into()),
        stderr: None,
        exit_code: Some(0),
        error: None,
    };
    // DefaultLabeler uses subcommand: "git status"
    assert_eq!(cmd.channel_label(), ChannelLabel::Command("git status".into()));

    let cmd_no_args = Interaction::Command {
        label: None,
        cmd: "git".into(),
        args: vec![],
        cwd: "/repo".into(),
        stdout: Some("ok\n".into()),
        stderr: None,
        exit_code: Some(0),
        error: None,
    };
    assert_eq!(cmd_no_args.channel_label(), ChannelLabel::Command("git".into()));

    let api = Interaction::GhApi {
        label: None,
        method: "GET".into(),
        endpoint: "repos/owner/repo/pulls".into(),
        status: 200,
        body: "[]".into(),
        headers: HashMap::new(),
    };
    assert_eq!(api.channel_label(), ChannelLabel::GhApi("repos/owner/repo/pulls".into()));

    let http = Interaction::Http {
        label: None,
        method: "GET".into(),
        url: "https://api.claude.ai/v1/sessions".into(),
        request_headers: HashMap::new(),
        request_body: None,
        status: 200,
        response_body: "{}".into(),
        response_headers: HashMap::new(),
    };
    assert_eq!(http.channel_label(), ChannelLabel::Http("api.claude.ai".into()));
}

#[test]
fn default_labeler_uses_subcommand() {
    let request = ChannelRequest::Command { cmd: "git", args: &["branch", "--list"] };
    assert_eq!(DefaultLabeler.label_for(&request), ChannelLabel::Command("git branch".into()));
}

#[test]
fn default_labeler_no_args_uses_cmd() {
    let request = ChannelRequest::Command { cmd: "git", args: &[] };
    assert_eq!(DefaultLabeler.label_for(&request), ChannelLabel::Command("git".into()));
}

#[test]
fn task_id_overrides_label() {
    use flotilla_core::providers::TaskId;
    let request = ChannelRequest::Command { cmd: "git", args: &["rev-list", "--left-right", "--count", "HEAD...main"] };
    assert_eq!(TaskId("trunk-ab").label_for(&request), ChannelLabel::Command("trunk-ab".into()));
}

#[test]
fn load_rounds_flat_format() {
    let yaml = r#"
interactions:
  - channel: command
    cmd: git
    args: ["status"]
    cwd: "/repo"
    stdout: "ok\n"
    exit_code: 0
  - channel: gh_api
    method: GET
    endpoint: "/repos/owner/repo/pulls"
    status: 200
    body: "[]"
"#;
    let rounds = load_rounds_from_str(yaml);
    assert_eq!(rounds.len(), 1);
    assert_eq!(rounds[0].queues.len(), 2);
    assert!(rounds[0].queues.contains_key(&ChannelLabel::Command("git status".into())));
    assert!(rounds[0].queues.contains_key(&ChannelLabel::GhApi("/repos/owner/repo/pulls".into())));
}

#[test]
fn load_rounds_multi_round_format() {
    let yaml = r#"
rounds:
  - interactions:
      - channel: command
        cmd: git
        args: ["status"]
        cwd: "/repo"
        stdout: "ok\n"
        exit_code: 0
  - interactions:
      - channel: gh_api
        method: GET
        endpoint: "/repos/owner/repo/pulls"
        status: 200
        body: "[]"
"#;
    let rounds = load_rounds_from_str(yaml);
    assert_eq!(rounds.len(), 2);
    assert_eq!(rounds[0].queues.len(), 1);
    assert!(rounds[0].queues.contains_key(&ChannelLabel::Command("git status".into())));
    assert_eq!(rounds[1].queues.len(), 1);
    assert!(rounds[1].queues.contains_key(&ChannelLabel::GhApi("/repos/owner/repo/pulls".into())));
}

#[test]
fn round_is_empty() {
    let round = Round::from_interactions(vec![]);
    assert!(round.is_empty());

    let round = Round::from_interactions(vec![Interaction::Command {
        label: None,
        cmd: "git".into(),
        args: vec![],
        cwd: "/repo".into(),
        stdout: None,
        stderr: None,
        exit_code: Some(0),
        error: None,
    }]);
    assert!(!round.is_empty());
}

#[test]
fn replayer_serves_by_channel_label() {
    let yaml = r#"
interactions:
  - channel: command
    cmd: git
    args: ["status"]
    cwd: "/repo"
    stdout: "ok\n"
    exit_code: 0
  - channel: gh_api
    method: GET
    endpoint: "/repos/owner/repo/pulls"
    status: 200
    body: "[]"
  - channel: command
    cmd: git
    args: ["log"]
    cwd: "/repo"
    stdout: "commits\n"
    exit_code: 0
"#;
    let dir = tempfile::tempdir().expect("create temp dir");
    let path = dir.path().join("test.yaml");
    std::fs::write(&path, yaml).expect("write fixture");

    let replayer = Replayer::from_file(&path, Masks::new());

    // Can consume in any channel order within the same round
    let api = replayer.next(&ChannelLabel::GhApi("/repos/owner/repo/pulls".into()));
    match api {
        Interaction::GhApi { endpoint, .. } => {
            assert_eq!(endpoint, "/repos/owner/repo/pulls");
        }
        _ => panic!("expected gh_api"),
    }

    // First git command
    let cmd1 = replayer.next(&ChannelLabel::Command("git status".into()));
    match cmd1 {
        Interaction::Command { stdout, .. } => {
            assert_eq!(stdout, Some("ok\n".into()));
        }
        _ => panic!("expected command"),
    }

    // Second git command (different subcommand channel)
    let cmd2 = replayer.next(&ChannelLabel::Command("git log".into()));
    match cmd2 {
        Interaction::Command { stdout, .. } => {
            assert_eq!(stdout, Some("commits\n".into()));
        }
        _ => panic!("expected command"),
    }

    replayer.assert_complete();
}

#[test]
#[should_panic(expected = "no queue for channel")]
fn replayer_enforces_round_boundaries() {
    let yaml = r#"
rounds:
  - interactions:
      - channel: command
        cmd: git
        args: ["status"]
        cwd: "/repo"
        stdout: "ok\n"
        exit_code: 0
  - interactions:
      - channel: gh_api
        method: GET
        endpoint: "/repos/owner/repo/pulls"
        status: 200
        body: "[]"
"#;
    let dir = tempfile::tempdir().expect("create temp dir");
    let path = dir.path().join("test.yaml");
    std::fs::write(&path, yaml).expect("write fixture");

    let replayer = Replayer::from_file(&path, Masks::new());

    // Round 1 only has a command — requesting gh_api should panic
    replayer.next(&ChannelLabel::GhApi("/repos/owner/repo/pulls".into()));
}

#[test]
fn replayer_auto_advances_rounds() {
    let yaml = r#"
rounds:
  - interactions:
      - channel: command
        cmd: git
        args: ["status"]
        cwd: "/repo"
        stdout: "ok\n"
        exit_code: 0
  - interactions:
      - channel: gh_api
        method: GET
        endpoint: "/repos/owner/repo/pulls"
        status: 200
        body: "[]"
"#;
    let dir = tempfile::tempdir().expect("create temp dir");
    let path = dir.path().join("test.yaml");
    std::fs::write(&path, yaml).expect("write fixture");

    let replayer = Replayer::from_file(&path, Masks::new());

    // Consume round 1
    let cmd = replayer.next(&ChannelLabel::Command("git status".into()));
    match cmd {
        Interaction::Command { cmd, .. } => assert_eq!(cmd, "git"),
        _ => panic!("expected command"),
    }

    // Round auto-advances — now we can get gh_api from round 2
    let api = replayer.next(&ChannelLabel::GhApi("/repos/owner/repo/pulls".into()));
    match api {
        Interaction::GhApi { endpoint, .. } => {
            assert_eq!(endpoint, "/repos/owner/repo/pulls");
        }
        _ => panic!("expected gh_api"),
    }

    replayer.assert_complete();
}

#[test]
fn recorder_saves_single_round() {
    let dir = tempfile::tempdir().expect("create temp dir");
    let path = dir.path().join("recorded.yaml");

    let recorder = Recorder::new(&path, Masks::new());
    recorder.record(Interaction::Command {
        label: None,
        cmd: "git".into(),
        args: vec!["status".into()],
        cwd: "/repo".into(),
        stdout: Some("ok\n".into()),
        stderr: None,
        exit_code: Some(0),
        error: None,
    });
    recorder.save();

    let content = std::fs::read_to_string(&path).expect("read fixture");
    let round_log: RoundLog = serde_yml::from_str(&content).expect("parse fixture");
    assert_eq!(round_log.rounds.len(), 1);
    assert_eq!(round_log.rounds[0].interactions.len(), 1);
}

#[test]
fn recorder_saves_multi_round_with_barriers() {
    let dir = tempfile::tempdir().expect("create temp dir");
    let path = dir.path().join("recorded.yaml");

    let recorder = Recorder::new(&path, Masks::new());
    recorder.record(Interaction::Command {
        label: None,
        cmd: "git".into(),
        args: vec!["status".into()],
        cwd: "/repo".into(),
        stdout: Some("ok\n".into()),
        stderr: None,
        exit_code: Some(0),
        error: None,
    });
    recorder.barrier();
    recorder.record(Interaction::GhApi {
        label: None,
        method: "GET".into(),
        endpoint: "/repos/owner/repo/pulls".into(),
        status: 200,
        body: "[]".into(),
        headers: HashMap::new(),
    });
    recorder.save();

    let content = std::fs::read_to_string(&path).expect("read fixture");
    let round_log: RoundLog = serde_yml::from_str(&content).expect("parse fixture");
    assert_eq!(round_log.rounds.len(), 2);
    assert_eq!(round_log.rounds[0].interactions.len(), 1);
    assert_eq!(round_log.rounds[1].interactions.len(), 1);
}

#[test]
fn recorder_applies_masks() {
    let dir = tempfile::tempdir().expect("create temp dir");
    let path = dir.path().join("recorded.yaml");

    let mut masks = Masks::new();
    masks.add("/Users/bob/dev/repo", "{repo}");

    let recorder = Recorder::new(&path, masks);
    recorder.record(Interaction::Command {
        label: None,
        cmd: "git".into(),
        args: vec!["status".into()],
        cwd: "/Users/bob/dev/repo".into(),
        stdout: Some("ok\n".into()),
        stderr: None,
        exit_code: Some(0),
        error: None,
    });
    recorder.save();

    let content = std::fs::read_to_string(&path).expect("read fixture");
    assert!(content.contains("{repo}"), "expected masked value in output");
    assert!(!content.contains("/Users/bob/dev/repo"), "expected concrete value to be masked");
}

#[tokio::test]
async fn session_record_then_replay_round_trip() {
    use super::testing::MockRunner;

    let dir = tempfile::tempdir().expect("create temp dir");
    let fixture_path = dir.path().join("round_trip.yaml");

    // Record phase with barriers
    {
        let mock = Arc::new(MockRunner::new(vec![Ok("branch-list\n".into()), Ok("status-ok\n".into())]));
        let session = Session::recording(&fixture_path, Masks::new());
        let runner = RecordingRunner::new(session.clone(), mock);

        let r1 = run!(runner, "git", &["branch"], Path::new("/repo"));
        assert!(r1.is_ok());

        session.barrier();

        let r2 = run!(runner, "git", &["status"], Path::new("/repo"));
        assert!(r2.is_ok());

        session.finish();
    }

    // Replay phase — verify round structure is preserved
    {
        let session = Session::replaying(&fixture_path, Masks::new());
        let runner = ReplayRunner::new(session.clone());

        let r1 = run!(runner, "git", &["branch"], Path::new("/repo"));
        assert_eq!(r1.expect("round 1 replay"), "branch-list\n");

        let r2 = run!(runner, "git", &["status"], Path::new("/repo"));
        assert_eq!(r2.expect("round 2 replay"), "status-ok\n");

        session.assert_complete();
    }
}

#[tokio::test]
async fn concurrent_same_subcommand_with_task_id() {
    use flotilla_core::providers::TaskId;

    let yaml = r#"
rounds:
- interactions:
  - channel: command
    label: trunk-ab
    cmd: git
    args: ["rev-list", "--left-right", "--count", "HEAD...main"]
    cwd: /test
    stdout: "2\t3"
    exit_code: 0
  - channel: command
    label: remote-ab
    cmd: git
    args: ["rev-list", "--left-right", "--count", "HEAD...origin/feature"]
    cwd: /test
    stdout: "0\t5"
    exit_code: 0
"#;
    let session = Session::replaying_from_str(yaml, Masks::new());
    let runner = Arc::new(ReplayRunner::new(session.clone()));
    let cwd = Path::new("/test");

    // Consume in REVERSE order — works because TaskId puts them on different channels
    let (remote, trunk) = tokio::join!(
        async { run!(runner, "git", &["rev-list", "--left-right", "--count", "HEAD...origin/feature"], cwd, TaskId("remote-ab")) },
        async { run!(runner, "git", &["rev-list", "--left-right", "--count", "HEAD...main"], cwd, TaskId("trunk-ab")) },
    );

    assert_eq!(remote.unwrap().trim(), "0\t5");
    assert_eq!(trunk.unwrap().trim(), "2\t3");
    session.finish();
}

#[test]
fn replay_mode_defaults_to_replay() {
    // With no env var set (or non-matching value), mode is Replay
    assert_eq!(ReplayMode::parse(""), ReplayMode::Replay);
    assert_eq!(ReplayMode::parse("nonsense"), ReplayMode::Replay);
}

#[test]
fn replay_mode_record() {
    assert_eq!(ReplayMode::parse("record"), ReplayMode::Record);
}

#[test]
fn replay_mode_passthrough() {
    assert_eq!(ReplayMode::parse("passthrough"), ReplayMode::Passthrough);
}

#[test]
fn is_live_true_for_record_and_passthrough() {
    assert!(!ReplayMode::Replay.is_live());
    assert!(ReplayMode::Record.is_live());
    assert!(ReplayMode::Passthrough.is_live());
}

#[test]
fn session_passthrough_finish_is_noop() {
    let session = Session::Passthrough;
    // Should not panic
    session.finish();
}

#[test]
fn session_passthrough_barrier_is_noop() {
    let session = Session::Passthrough;
    session.barrier();
}

#[test]
fn session_passthrough_is_not_recording() {
    let session = Session::Passthrough;
    assert!(!session.is_recording());
}

#[test]
fn session_passthrough_is_live() {
    assert!(!Session::replaying_from_str("interactions: []", Masks::new()).is_live());
    assert!(Session::Passthrough.is_live());
}

#[tokio::test]
async fn test_runner_passthrough_returns_process_runner() {
    // Runs a real subprocess unconditionally — `echo` is universally available
    // and instant, so this is acceptable outside passthrough mode.  The test
    // validates that Session::Passthrough produces a functional runner.
    let session = Session::Passthrough;
    let runner = test_runner(&session);
    let result = runner.run("echo", &["hello"], Path::new("/tmp"), &ChannelLabel::Default).await;
    assert!(result.is_ok());
    assert!(result.unwrap().contains("hello"));
}

#[tokio::test]
async fn concurrent_different_subcommands_default_labeler() {
    let yaml = r#"
rounds:
- interactions:
  - channel: command
    cmd: git
    args: ["status", "--porcelain"]
    cwd: /test
    stdout: "M file.txt"
    exit_code: 0
  - channel: command
    cmd: git
    args: ["log", "-1", "--format=%h\t%s"]
    cwd: /test
    stdout: "abc1234\tcommit msg"
    exit_code: 0
"#;
    let session = Session::replaying_from_str(yaml, Masks::new());
    let runner = Arc::new(ReplayRunner::new(session.clone()));
    let cwd = Path::new("/test");

    // Consume in reverse order — works because "git status" and "git log" are different channels
    let (log, status) = tokio::join!(async { run!(runner, "git", &["log", "-1", "--format=%h\t%s"], cwd) }, async {
        run!(runner, "git", &["status", "--porcelain"], cwd)
    },);

    assert_eq!(log.unwrap().trim(), "abc1234\tcommit msg");
    assert_eq!(status.unwrap().trim(), "M file.txt");
    session.finish();
}

// Network boundary for record/replay: return a classified REST outcome directly.
struct ClassifiedTestApi(Result<GhApiResponse, ObservationError>);

#[async_trait]
impl GhApi for ClassifiedTestApi {
    async fn get(&self, _endpoint: &str, _root: &Path, _label: &ChannelLabel) -> Result<String, String> {
        panic!("classified reads only")
    }
    async fn get_with_headers(&self, _endpoint: &str, _root: &Path, _label: &ChannelLabel) -> Result<GhApiResponse, String> {
        panic!("classified reads only")
    }
    async fn get_classified_with_headers(
        &self,
        _endpoint: &str,
        _root: &Path,
        _label: &ChannelLabel,
    ) -> Result<GhApiResponse, ObservationError> {
        match &self.0 {
            Ok(response) => Ok(GhApiResponse {
                status: response.status,
                etag: response.etag.clone(),
                body: response.body.clone(),
                has_next_page: response.has_next_page,
                total_count: response.total_count,
            }),
            Err(error) => Err(error.clone()),
        }
    }
}

// #2541: recordings round-trip classification and diagnostics without parsing Display.
#[hegel::test]
fn classified_rest_recording_round_trips(tc: hegel::TestCase) {
    use hegel::generators as gs;

    use flotilla_core::providers::github_api::GithubRetrySource;
    // Exhaust error variants, both kinds, every retry source and absent/present
    // deadlines in each case; generate timestamps across zero and modern dates,
    // pagination boundaries and counts across the empty/100-item page boundary.
    let timestamp = tc.draw(gs::integers::<i64>().min_value(0).max_value(1893456000));
    let has_next_page = tc.draw(gs::booleans());
    let total_count = tc.draw(gs::integers::<u32>().min_value(0).max_value(101));
    let mut outcomes = vec![
        Err(ObservationError::Forge("rate limited diagnostics unavailable".into())),
        Ok(GhApiResponse { status: 200, etag: Some("etag".into()), body: "[]".into(), has_next_page, total_count: Some(total_count) }),
    ];
    for kind in [GithubRateLimitKind::Primary, GithubRateLimitKind::Secondary] {
        for retry_source in [
            GithubRetrySource::Unavailable,
            GithubRetrySource::RetryAfter,
            GithubRetrySource::RateLimitReset,
            GithubRetrySource::SecondaryFallback,
        ] {
            for retry_at in [None, Some(chrono::DateTime::from_timestamp(timestamp, 0).expect("deadline"))] {
                outcomes.push(Err(ObservationError::RateLimited {
                    budget: "REST core".into(),
                    limit: GithubRateLimit { kind, retry_at, retry_source },
                }));
            }
        }
    }
    tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime").block_on(async {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("recording.yaml");
        for expected in outcomes {
            let recording = Session::recording(&path, Masks::new());
            let api = RecordingGhApi::new(recording.clone(), Arc::new(ClassifiedTestApi(expected)));
            let endpoint = "repos/team/one/pulls";
            let label = flotilla_core::providers::gh_api_channel_label("GET", endpoint);
            let recorded = api.get_classified_with_headers(endpoint, Path::new("/"), &label).await;
            recording.finish();
            let replay = Session::replaying(&path, Masks::new());
            let replayed = ReplayGhApi::new(replay.clone()).get_classified_with_headers(endpoint, Path::new("/"), &label).await;
            match (recorded, replayed) {
                (Err(expected), Err(actual)) => assert_eq!(actual, expected),
                (Ok(expected), Ok(actual)) => {
                    assert_eq!(actual.status, expected.status);
                    assert_eq!(actual.body, expected.body);
                    assert_eq!(actual.etag, expected.etag);
                    assert_eq!(actual.has_next_page, expected.has_next_page);
                    assert_eq!(actual.total_count, expected.total_count);
                }
                outcomes => panic!("record/replay outcomes differ: {outcomes:?}"),
            }
            replay.finish();
        }
    });
}

// Substitute only the gh subprocess boundary; run the real REST client and recorder.
struct RestFailureRunner {
    stdout: String,
    stderr: String,
    transport_failure: bool,
}

#[async_trait]
impl CommandRunner for RestFailureRunner {
    async fn run(&self, _cmd: &str, _args: &[&str], _cwd: &Path, _label: &ChannelLabel) -> Result<String, String> {
        panic!("REST uses full process output")
    }
    async fn run_output(&self, _cmd: &str, _args: &[&str], _cwd: &Path, _label: &ChannelLabel) -> Result<CommandOutput, String> {
        if self.transport_failure {
            return Err(self.stderr.clone());
        }
        Ok(CommandOutput { stdout: self.stdout.clone(), stderr: self.stderr.clone(), exit_code: Some(1) })
    }
    async fn exists(&self, _cmd: &str, _args: &[&str]) -> bool {
        true
    }
}

// #2557: actual HTTP status, headers and body survive classification/recording;
// both replay interfaces retain the corresponding live diagnostics/classification.
#[hegel::test]
fn classified_rest_failures_preserve_response_metadata(tc: hegel::TestCase) {
    use hegel::generators as gs;

    use flotilla_core::providers::github_api::{GhApiClient, GithubRetrySource};
    // Exhaust ordinary 403/404, primary and secondary 403/429, missing reset,
    // and transport failure each run. Generate reset boundaries and retry delay.
    let reset = tc.draw(gs::integers::<i64>().min_value(0).max_value(1893456000));
    let delay = tc.draw(gs::integers::<u32>().min_value(0).max_value(120));
    tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime").block_on(async {
        let temp = tempfile::tempdir().expect("tempdir");
        for (status, remaining, message, deadline, retry_after) in [
            (403, "4999", "Resource not accessible by integration", true, false),
            (404, "4999", "Not Found", false, false),
            (403, "0", "API rate limit exceeded", true, false),
            (429, "0", "API rate limit exceeded", true, false),
            (403, "0", "API rate limit exceeded", false, false),
            (403, "4999", "secondary rate limit exceeded", true, true),
            (429, "4999", "secondary rate limit exceeded", true, true),
            (0, "", "transport failure", false, false),
        ] {
            let body = format!("{{\"message\":\"{message}\"}}");
            let mut raw = format!("HTTP/2 {status}\r\nX-RateLimit-Resource: core\r\nX-RateLimit-Remaining: {remaining}\r\n");
            if deadline {
                raw.push_str(&format!("X-RateLimit-Reset: {reset}\r\n"));
            }
            if retry_after {
                raw.push_str(&format!(
                    "Retry-After: {}\r\n",
                    chrono::DateTime::from_timestamp(reset + i64::from(delay), 0).expect("retry time").to_rfc2822()
                ));
            }
            // Reserved recording keys cannot be injected by a REST response:
            // an untimed primary limit must not inherit this fabricated reset.
            raw.push_str("Observation-Retry-At: 2040-01-01T00:00:00+00:00\r\n");
            raw.push_str(&format!("\r\n{body}"));
            let runner = Arc::new(RestFailureRunner {
                stdout: raw,
                stderr: format!("gh: {message} (HTTP {status})"),
                transport_failure: status == 0,
            });
            let endpoint = "repos/team/one/pulls";
            let label = flotilla_core::providers::gh_api_channel_label("GET", endpoint);
            let path = temp.path().join("recording.yaml");
            let live = GhApiClient::new(runner.clone());
            let expected_legacy = live.get_with_headers(endpoint, Path::new("/"), &label).await.expect_err("live failure");
            let recording = Session::recording(&path, Masks::new());
            let api = RecordingGhApi::new(recording.clone(), Arc::new(live));
            let classified = api.get_classified_with_headers(endpoint, Path::new("/"), &label).await.expect_err("classified failure");
            recording.finish();
            let detailed = Session::replaying(&path, Masks::new());
            let failure = ReplayGhApi::new(detailed.clone())
                .get_classified_response(endpoint, Path::new("/"), &label)
                .await
                .expect_err("detailed replay");
            assert_eq!(failure.error, classified);
            assert_eq!(failure.response.is_some(), status != 0);
            if let Some(response) = failure.response {
                assert_eq!(response.status, status);
                assert_eq!(response.body, body);
                assert_eq!(response.legacy_error, expected_legacy);
                assert!(!response.headers.keys().any(|key| key.starts_with("observation-")));
            }
            detailed.finish();
            let log: RoundLog = serde_yml::from_str(&std::fs::read_to_string(&path).expect("recording")).expect("log");
            let Interaction::GhApi { status: actual_status, body: actual_body, headers, .. } = &log.rounds[0].interactions[0] else {
                panic!("REST recording")
            };
            assert_eq!(*actual_status, status);
            if status != 0 {
                assert_eq!(*actual_body, body);
                assert_eq!(headers.get("x-ratelimit-remaining").expect("remaining"), remaining);
                assert_eq!(headers.get("x-ratelimit-resource").expect("resource"), "core");
                assert_eq!(headers.get("x-ratelimit-reset"), deadline.then(|| reset.to_string()).as_ref());
                assert_eq!(
                    headers.get("retry-after"),
                    retry_after
                        .then(|| chrono::DateTime::from_timestamp(reset + i64::from(delay), 0).expect("retry time").to_rfc2822())
                        .as_ref()
                );
            }
            if status == 403 && remaining == "0" && !deadline {
                let ObservationError::RateLimited { limit, .. } = &classified else { panic!("primary limit") };
                assert_eq!(limit.retry_at, None);
                assert_eq!(limit.retry_source, GithubRetrySource::Unavailable);
            }
            let replay = Session::replaying(&path, Masks::new());
            assert_eq!(
                ReplayGhApi::new(replay.clone())
                    .get_classified_with_headers(endpoint, Path::new("/"), &label)
                    .await
                    .expect_err("classified replay"),
                classified
            );
            replay.finish();
            for with_headers in [false, true] {
                let replay = Session::replaying(&path, Masks::new());
                let api = ReplayGhApi::new(replay.clone());
                let actual = if with_headers {
                    api.get_with_headers(endpoint, Path::new("/"), &label).await.expect_err("legacy headers")
                } else {
                    api.get(endpoint, Path::new("/"), &label).await.expect_err("legacy body")
                };
                assert_eq!(actual, expected_legacy);
                replay.finish();
            }
        }
    });
}

// #2557: preemptive issue-budget refusals have no HTTP failure response, and a
// cached refusal makes no further subprocess call; replay retains that distinction.
#[tokio::test]
async fn classified_issue_budget_recording_has_no_failure_response() {
    use super::testing::MockRunner;
    use flotilla_core::providers::github_api::GhApiClient;
    let reset = chrono::Utc::now().timestamp() + 3600;
    // Substitute the gh subprocess boundary with one successful low-budget response.
    let runner = Arc::new(MockRunner::new(vec![Ok(format!(
        "HTTP/2 200 OK\r\nX-RateLimit-Resource: core\r\nX-RateLimit-Remaining: 75\r\nX-RateLimit-Reset: {reset}\r\n\r\n[]"
    ))]));
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("recording.yaml");
    let recording = Session::recording(&path, Masks::new());
    let api = RecordingGhApi::new(recording.clone(), Arc::new(GhApiClient::new(runner.clone())));
    let endpoint = "repos/team/one/issues?state=all";
    let label = flotilla_core::providers::gh_api_channel_label("GET", endpoint);
    let first = api.get_classified_response(endpoint, Path::new("/"), &label).await.expect_err("budget refusal");
    let second = api.get_classified_response(endpoint, Path::new("/"), &label).await.expect_err("cached refusal");
    assert!(first.response.is_none());
    assert!(second.response.is_none());
    assert_eq!(first.error, second.error);
    assert_eq!(runner.calls().len(), 1);
    recording.finish();
    let replay = Session::replaying(&path, Masks::new());
    let api = ReplayGhApi::new(replay.clone());
    for _ in 0..2 {
        let failure = api.get_classified_response(endpoint, Path::new("/"), &label).await.expect_err("replayed refusal");
        assert!(failure.response.is_none());
        assert_eq!(failure.error, first.error);
    }
    replay.finish();
}

// Full-output recording/replay preserves every numeric code and termination
// without a code. Generate arbitrary codes, retaining 0/1/2/17 as fixed edges.
#[hegel::test]
fn command_exit_status_survives_record_replay(tc: hegel::TestCase) {
    use hegel::generators as gs;

    use super::testing::MockRunner;

    let label = flotilla_core::providers::command_channel_label("pgrep", &["-x", "flotillad"]);
    let generated = tc.draw(gs::integers::<i32>());
    tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime").block_on(async {
        let temp = tempfile::tempdir().expect("recording directory");
        for exit_code in [Some(0), Some(1), Some(2), Some(17), Some(generated), None] {
            let path = temp.path().join("command.yaml");
            let session = Session::recording(&path, Masks::new());
            let inner = Arc::new(MockRunner::with_outputs(vec![Ok(CommandOutput {
                stdout: "output\n".into(),
                stderr: "diagnostic\n".into(),
                exit_code,
            })]));
            let recording = RecordingRunner::new(session.clone(), inner);
            let live = recording.run_output("pgrep", &["-x", "flotillad"], Path::new("/"), &label).await.expect("live");
            session.finish();
            let session = Session::replaying(&path, Masks::new());
            let replay = ReplayRunner::new(session.clone());
            let output = replay.run_output("pgrep", &["-x", "flotillad"], Path::new("/"), &label).await.expect("replay");
            assert_eq!(output.exit_code, exit_code);
            assert_eq!(output.success(), exit_code == Some(0));
            assert_eq!((output.stdout, output.stderr), (live.stdout, live.stderr));
            session.finish();
            // The string convenience API selects stderr for every failure,
            // including explicit null/no-code termination, and stdout for zero.
            let session = Session::replaying(&path, Masks::new());
            let output = ReplayRunner::new(session.clone()).run("pgrep", &["-x", "flotillad"], Path::new("/"), &label).await;
            let expected = if exit_code == Some(0) { Ok("output\n".to_string()) } else { Err("diagnostic\n".to_string()) };
            assert_eq!(output, expected);
            session.finish();
        }
    });
}

// Existing integer and omitted status fields remain decodable; an explicit
// null newly represents termination without a numeric exit code.
#[test]
fn legacy_command_exit_status_is_readable() {
    for (field, expected) in [("", Some(0)), ("exit_code: 42\n", Some(42)), ("exit_code: null\n", None)] {
        let yaml = format!("channel: command\ncmd: pgrep\nargs: []\ncwd: /\n{field}");
        let interaction: Interaction = serde_yml::from_str(&yaml).expect("decode command");
        let Interaction::Command { exit_code, .. } = interaction else { panic!("command") };
        assert_eq!(exit_code, expected);
    }
}

// Raw-output execution failures survive recording as errors, rather than an
// exit code 1 that pgrep callers must interpret as a successful empty match.
#[tokio::test]
async fn command_execution_error_survives_record_replay() {
    use super::testing::MockRunner;
    let temp = tempfile::tempdir().expect("recording directory");
    let path = temp.path().join("failure.yaml");
    let label = flotilla_core::providers::command_channel_label("pgrep", &["-x", "flotillad"]);
    let mut masks = Masks::new();
    masks.add("/private/runtime", "{runtime}");
    let session = Session::recording(&path, masks.clone());
    let inner = Arc::new(MockRunner::with_outputs(vec![Err("cannot execute in /private/runtime".into())]));
    let expected =
        RecordingRunner::new(session.clone(), inner).run_output("pgrep", &["-x", "flotillad"], Path::new("/"), &label).await.err();
    session.finish();
    assert!(!std::fs::read_to_string(&path).expect("recording").contains("/private/runtime"));
    let session = Session::replaying(&path, masks);
    let actual = ReplayRunner::new(session.clone()).run_output("pgrep", &["-x", "flotillad"], Path::new("/"), &label).await.err();
    assert_eq!(actual, expected);
    assert_eq!(actual.as_deref(), Some("cannot execute in /private/runtime"));
    session.finish();
}
