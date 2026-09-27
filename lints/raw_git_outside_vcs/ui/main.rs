use std::process::Command;
use std::process::Command as Launcher;

struct Runner;
impl Runner {
    fn run(&self, _: &str, _: &[&str]) {}
    fn run_output(&self, _: &str, _: &[&str]) {}
    fn run_with_input(&self, _: &str, _: &[&str], _: &str) {}
}

macro_rules! run {
    ($runner:expr, $command:expr, $args:expr) => {
        $runner.run($command, $args)
    };
}

macro_rules! run_output {
    ($runner:expr, $command:expr, $args:expr) => {
        $runner.run_output($command, $args)
    };
}

fn main() {
    let runner = Runner;
    runner.run("git", &[]);
    runner.run_output("git", &[]);
    runner.run_with_input("git", &[], "");
    run!(runner, "git", &[]);
    run_output!(runner, "git", &[]);
    let _ = Command::new("git");
    let _ = std::process::Command::new(/* still Git */ "git");
    let _ = Launcher::new("git");
    runner.run("gh", &[]);
    let _ = Command::new("echo");
}
