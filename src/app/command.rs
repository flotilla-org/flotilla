use std::collections::VecDeque;
use std::path::PathBuf;

pub enum Command {
    SwitchWorktree(usize),
    SelectWorkspace(String),
    CreateWorktree { branch: String, create_branch: bool },
    FetchDeleteInfo(usize),
    ConfirmDelete,
    OpenPr(String),
    OpenIssueBrowser(String),
    ArchiveSession(usize),
    GenerateBranchName(Vec<usize>),
    /// Teleport into a web session (creates worktree + workspace as needed)
    TeleportSession { session_id: String, branch: Option<String>, worktree_idx: Option<usize> },
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
