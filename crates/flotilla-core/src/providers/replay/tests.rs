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
                exit_code: 0,
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

    use crate::providers::vcs::{git::GitVcs, Vcs};
    let git = GitVcs::new(runner);
    let branches = git.list_local_branches(Path::new("/test/repo")).await.unwrap();

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
            exit_code: 0,
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
    use crate::providers::testing::MockRunner;

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
    use crate::providers::HttpClient;

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

    let request = reqwest::Client::new()
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
        exit_code: 0,
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
        exit_code: 0,
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
    use super::super::TaskId;
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
        exit_code: 0,
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
        exit_code: 0,
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
        exit_code: 0,
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
        exit_code: 0,
    });
    recorder.save();

    let content = std::fs::read_to_string(&path).expect("read fixture");
    assert!(content.contains("{repo}"), "expected masked value in output");
    assert!(!content.contains("/Users/bob/dev/repo"), "expected concrete value to be masked");
}

#[tokio::test]
async fn session_record_then_replay_round_trip() {
    use crate::providers::testing::MockRunner;

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
    use super::super::TaskId;

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
    let result = runner.run("echo", &["hello"], Path::new("/tmp"), &ChannelLabel::Noop).await;
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
