use std::{path::Path, process::Command, sync::Arc};

use crate::{
    model::SystemClock,
    serve::{build_forge, start},
    Config, ForgeSettings,
};

const LAB_TOKEN: &str = "lab-test-token";

fn fixture(name: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
}

fn git(cwd: &Path, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .args(["-c", &format!("http.sslCAInfo={}", fixture("ca.pem").display()), "-c", "credential.helper="])
        .args(args)
        .current_dir(cwd)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .expect("git")
}

// Behaviour: the fake serves Git's fetch transport over HTTPS for a forge's
// repositories, as Forgejo serves a private repository: Git without a
// sign-in is challenged and fails, Git signed in as the forge's login with
// its token lists and clones, and a push is refused.
#[tokio::test]
async fn fetching_needs_the_forge_sign_in_and_pushing_is_refused() {
    let temp = tempfile::tempdir().expect("tempdir");
    let source = temp.path().join("source");
    std::fs::create_dir(&source).expect("source");
    for args in
        [&["init", "-q", "-b", "main"][..], &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "--allow-empty", "-m", "seed"]]
    {
        assert!(git(&source, args).status.success());
    }
    let root = temp.path().join("git");
    std::fs::create_dir_all(root.join("lab/acme")).expect("forge root");
    let bare = root.join("lab/acme/widgets.git");
    assert!(git(temp.path(), &["clone", "-q", "--bare", source.to_str().expect("path"), bare.to_str().expect("path")]).status.success());
    let config = Config {
        public_url: "https://localhost:3443".into(),
        listen: "127.0.0.1:0".parse().expect("addr"),
        admin_listen: "127.0.0.1:0".parse().expect("addr"),
        tls_cert: fixture("server.pem"),
        tls_key: fixture("server-key.pem"),
        admin_token: "admin-test-token".into(),
        auto_merge_after: None,
        seed: None,
        git_root: Some(root),
        forges: [("lab".to_string(), ForgeSettings { token: LAB_TOKEN.into(), login: "lab-bot".into() })].into(),
    };
    let forge = build_forge(&config, Arc::new(SystemClock)).expect("forge");
    let running = start(&config, forge).await.expect("start");
    let url = format!("https://localhost:{}/lab/acme/widgets.git", running.api_addr().port());
    let token_file = temp.path().join("token");
    std::fs::write(&token_file, LAB_TOKEN).expect("token file");
    let helper =
        |login: &str| format!("credential.helper=!f() {{ echo username={login}; echo password=$(cat '{}'); }}; f", token_file.display());

    let anonymous = tokio::task::spawn_blocking({
        let (cwd, url) = (temp.path().to_path_buf(), url.clone());
        move || git(&cwd, &["ls-remote", &url])
    })
    .await
    .expect("anonymous ls-remote");
    assert!(!anonymous.status.success());
    assert!(String::from_utf8_lossy(&anonymous.stderr).contains("could not read Username"), "{anonymous:?}");

    let stranger = tokio::task::spawn_blocking({
        let (cwd, url, helper) = (temp.path().to_path_buf(), url.clone(), helper("someone-else"));
        move || git(&cwd, &["-c", &helper, "ls-remote", &url])
    })
    .await
    .expect("stranger ls-remote");
    assert!(!stranger.status.success(), "{stranger:?}");

    let listed = tokio::task::spawn_blocking({
        let (cwd, url, helper) = (temp.path().to_path_buf(), url.clone(), helper("lab-bot"));
        move || git(&cwd, &["-c", &helper, "ls-remote", &url])
    })
    .await
    .expect("signed-in ls-remote");
    assert!(listed.status.success(), "{listed:?}");
    assert!(String::from_utf8_lossy(&listed.stdout).contains("refs/heads/main"), "{listed:?}");

    let clone = temp.path().join("clone");
    let cloned = tokio::task::spawn_blocking({
        let (cwd, url, helper, clone) = (temp.path().to_path_buf(), url.clone(), helper("lab-bot"), clone.clone());
        move || git(&cwd, &["-c", &helper, "clone", "-q", &url, clone.to_str().expect("path")])
    })
    .await
    .expect("signed-in clone");
    assert!(cloned.status.success(), "{cloned:?}");

    let pushed = tokio::task::spawn_blocking({
        let helper = helper("lab-bot");
        move || git(&clone, &["-c", &helper, "push", "-q", "origin", "HEAD:refs/heads/pushed"])
    })
    .await
    .expect("push");
    assert!(!pushed.status.success(), "{pushed:?}");
    running.shutdown().await;
}
