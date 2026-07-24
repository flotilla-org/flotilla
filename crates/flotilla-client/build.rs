use std::process::Command;

fn git_output(args: &[&str]) -> Option<String> {
    Command::new("git")
        .args(args)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|output| output.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn main() {
    println!("cargo::rerun-if-env-changed=FLOTILLA_BUILD_ID");
    if let Some(head) = git_output(&["rev-parse", "--git-path", "HEAD"]) {
        println!("cargo::rerun-if-changed={head}");
    }
    if let Some(reference) = git_output(&["symbolic-ref", "-q", "HEAD"]) {
        if let Some(reference_path) = git_output(&["rev-parse", "--git-path", &reference]) {
            println!("cargo::rerun-if-changed={reference_path}");
        }
    }

    let build_id = std::env::var("FLOTILLA_BUILD_ID")
        .ok()
        .filter(|value| !value.is_empty())
        .or_else(|| git_output(&["rev-parse", "--short=12", "HEAD"]))
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo::rustc-env=FLOTILLA_BUILD_ID={build_id}");
}
