//! Changing the repository.
//!
//! Every argv is built by [`GitOp::commands`], a pure function — that is where
//! the pathspec rules are enforced and where the tests point. `run_op` only
//! executes what it is handed and turns a failure into something the UI can
//! act on.
//!
//! Confirmation of destructive operations belongs to the UI, not here:
//! `run_op` has to stay callable from a flow that already confirmed once, and
//! from a test. What this module offers instead is [`GitOp::destructive`], the
//! policy datum the UI gates on.

use std::path::PathBuf;

/// One argv can only carry so many paths before it hits `E2BIG` (~256 KiB on
/// macOS), so a big stage is split into several calls.
pub const MAX_PATHSPECS_PER_CALL: usize = 200;

/// Long enough for a push over a slow link. Only applied to network operations
/// — the local path has no deadline at all.
pub const GIT_NETWORK_DEADLINE: std::time::Duration = std::time::Duration::from_secs(600);

/// What the user stands to lose. Purely advisory data for the UI's gate.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Destructive {
    LosesWorktreeEdits,
    LosesUntrackedFiles,
    LosesCommits,
    RewritesHistory,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ResetMode {
    Soft,
    Mixed,
    Hard,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PullMode {
    FfOnly,
    Rebase,
    Merge,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum GitOp {
    Stage {
        paths: Vec<super::status::RepoPath>,
    },
    StageAll,
    Unstage {
        paths: Vec<super::status::RepoPath>,
    },
    UnstageAll,
    /// `git checkout --` on tracked files.
    DiscardWorktree {
        paths: Vec<super::status::RepoPath>,
    },
    /// `git clean` on untracked ones.
    DiscardUntracked {
        paths: Vec<super::status::RepoPath>,
        directories: bool,
    },
    Commit {
        message: String,
        amend: bool,
        signoff: bool,
        no_verify: bool,
        /// Stage every tracked change first (`-a`), for "Commit All".
        all: bool,
    },
    CheckoutBranch {
        name: String,
    },
    CheckoutDetached {
        rev: String,
    },
    CreateBranch {
        name: String,
        start: Option<String>,
        checkout: bool,
    },
    DeleteBranch {
        name: String,
        force: bool,
    },
    CherryPick {
        rev: String,
        mainline: bool,
        no_commit: bool,
    },
    Revert {
        rev: String,
        mainline: bool,
    },
    Reset {
        rev: String,
        mode: ResetMode,
    },
    Stash {
        message: Option<String>,
        include_untracked: bool,
    },
    Fetch {
        remote: Option<String>,
        prune: bool,
    },
    Pull {
        mode: PullMode,
    },
    Push {
        remote: String,
        branch: String,
        /// First push of a new branch: `-u`.
        set_upstream: bool,
        force_with_lease: bool,
    },
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct GitOpOutcome {
    pub op: &'static str,
    pub stdout: String,
    /// Non-empty on success too — remote hints and hook output land here.
    pub stderr: String,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GitOpErrorKind {
    NotARepo,
    /// The path is not UTF-8 and so cannot be sent as a pathspec.
    UnrepresentablePath,
    InvalidArgument,
    DirtyWorktree,
    Conflict,
    NothingToCommit,
    HookRejected,
    LockHeld,
    AuthRequired,
    NetworkUnreachable,
    NonFastForward,
    Timeout,
    Spawn,
    Other,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct GitOpError {
    pub op: &'static str,
    pub kind: GitOpErrorKind,
    /// One line for the notification.
    pub message: String,
    /// Full stderr, for the details disclosure.
    pub detail: String,
    /// The argv that failed. Authentication has no answer inside a GUI, so the
    /// notification offers to re-run this in a pane — tty7 is a terminal, and a
    /// real tty can take a password, a hardware key, or a host-key prompt.
    pub rerun_argv: Vec<String>,
    pub cwd: PathBuf,
}

impl GitOp {
    /// The stable short name used in errors and telemetry.
    pub fn label(&self) -> &'static str {
        match self {
            GitOp::Stage { .. } | GitOp::StageAll => "stage",
            GitOp::Unstage { .. } | GitOp::UnstageAll => "unstage",
            GitOp::DiscardWorktree { .. } | GitOp::DiscardUntracked { .. } => "discard",
            GitOp::Commit { .. } => "commit",
            GitOp::CheckoutBranch { .. } | GitOp::CheckoutDetached { .. } => "checkout",
            GitOp::CreateBranch { .. } => "branch",
            GitOp::DeleteBranch { .. } => "branch-delete",
            GitOp::CherryPick { .. } => "cherry-pick",
            GitOp::Revert { .. } => "revert",
            GitOp::Reset { .. } => "reset",
            GitOp::Stash { .. } => "stash",
            GitOp::Fetch { .. } => "fetch",
            GitOp::Pull { .. } => "pull",
            GitOp::Push { .. } => "push",
        }
    }

    pub fn destructive(&self) -> Option<Destructive> {
        Some(match self {
            GitOp::DiscardWorktree { .. } => Destructive::LosesWorktreeEdits,
            GitOp::DiscardUntracked { .. } => Destructive::LosesUntrackedFiles,
            GitOp::DeleteBranch { .. } => Destructive::LosesCommits,
            GitOp::Reset {
                mode: ResetMode::Hard,
                ..
            } => Destructive::LosesWorktreeEdits,
            GitOp::Commit { amend: true, .. } => Destructive::RewritesHistory,
            GitOp::Push {
                force_with_lease: true,
                ..
            } => Destructive::RewritesHistory,
            _ => return None,
        })
    }

    /// Whether this reaches the network, and so needs the long deadline.
    pub fn is_network(&self) -> bool {
        matches!(
            self,
            GitOp::Fetch { .. } | GitOp::Pull { .. } | GitOp::Push { .. }
        )
    }

    /// Every path this operation names, for validation and for deciding
    /// which caches to invalidate.
    pub fn paths(&self) -> &[super::status::RepoPath] {
        match self {
            GitOp::Stage { paths }
            | GitOp::Unstage { paths }
            | GitOp::DiscardWorktree { paths }
            | GitOp::DiscardUntracked { paths, .. } => paths,
            _ => &[],
        }
    }
}
