use std::collections::VecDeque;
use std::path::PathBuf;

pub enum Command {
    SwitchWorktree(PathBuf),
    SelectWorkspace(String),
    CreateWorktree { branch: String, create_branch: bool, issue_ids: Vec<(String, String)> },
    FetchDeleteInfo(usize),
    ConfirmDelete,
    OpenPr(String),
    OpenIssueBrowser(String),
    LinkIssuesToPr { pr_id: String, issue_ids: Vec<String> },
    ArchiveSession(String),
    GenerateBranchName(Vec<String>),
    /// Teleport into a web session (creates worktree + workspace as needed)
    TeleportSession { session_id: String, branch: Option<String>, checkout_key: Option<PathBuf> },
    AddRepo(PathBuf),
}

#[derive(Default)]
pub struct CommandQueue {
    queue: VecDeque<Command>,
}

impl CommandQueue {
    pub fn push(&mut self, cmd: Command) {
        self.queue.push_back(cmd);
    }

    pub fn take_next(&mut self) -> Option<Command> {
        self.queue.pop_front()
    }
}
