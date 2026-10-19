use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

pub struct TestGitRepo {
    path: PathBuf,
}

impl TestGitRepo {
    pub fn init(path: impl AsRef<Path>) -> Self {
        let path = path.as_ref().to_path_buf();
        fs::create_dir_all(&path).expect("create repo dir");
        let path_str = path.to_string_lossy().to_string();

        run_git(&["init", "--initial-branch=main", &path_str]);
        run_git(&["-C", &path_str, "config", "user.name", "Flotilla Tests"]);
        run_git(&["-C", &path_str, "config", "user.email", "flotilla@example.com"]);

        Self { path }
    }

    pub fn with_initial_commit(self) -> Self {
        let readme = self.path.join("README.md");
        fs::write(&readme, "hello\n").expect("write readme");
        let path_str = self.path.to_string_lossy().to_string();
        run_git(&["-C", &path_str, "add", "README.md"]);
        run_git(&["-C", &path_str, "commit", "-m", "init"]);
        self
    }

    pub fn with_origin(self, url: &str) -> Self {
        let path_str = self.path.to_string_lossy().to_string();
        run_git(&["-C", &path_str, "remote", "add", "origin", url]);
        self
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

fn run_git(args: &[&str]) {
    let status = Command::new("git").args(args).status().expect("git command should run");
    assert!(status.success(), "git {args:?} failed");
}
