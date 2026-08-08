//! What the working tree looks like right now — the model behind the source
//! control panel, the file tree's decorations, and every button that is only
//! enabled for some file states.
//!
//! One `git status --porcelain=v2 --branch -z` answers all of it. That format
//! is the only one that carries the staged and unstaged halves *separately*
//! (the `XY` pair), a rename's old path, unmerged stages, submodule sub-state,
//! and the branch header — getting the same picture out of `git diff` takes
//! four commands and a consistency window between them.
//!
//! The types live here rather than next to the parser because `ops` builds
//! commands out of them (`HeadState` decides how to unstage) and the GUI reads
//! them; the parser is just one producer.

use std::collections::HashMap;
use std::path::PathBuf;

/// A path relative to the repository root, always `/`-separated.
///
/// `-z` hands out raw bytes, and on Linux a path need not be UTF-8. When it is
/// not, `text` is the lossy rendering and `lossy` is set: such an entry can be
/// *shown* but never *acted on*, because `Host::git` takes `&[&str]` and the
/// control protocol carries args as `String` — the original bytes cannot reach
/// the far side. Callers must treat `pathspec() == None` as "read only".
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct RepoPath {
    pub text: String,
    pub lossy: bool,
}

impl RepoPath {
    pub fn from_bytes(bytes: &[u8]) -> RepoPath {
        match std::str::from_utf8(bytes) {
            Ok(text) => RepoPath {
                text: text.to_string(),
                lossy: false,
            },
            Err(_) => RepoPath {
                text: String::from_utf8_lossy(bytes).into_owned(),
                lossy: true,
            },
        }
    }

    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// The pathspec to hand git, or `None` if this path cannot be represented.
    ///
    /// `:(literal)` is not decoration: without it git globs the pathspec, so a
    /// file actually named `a[b].txt` or `foo*` would not match itself. Callers
    /// must additionally put a `--` ahead of the list so a file named `HEAD` or
    /// `-f` is not read as a rev or an option.
    pub fn pathspec(&self) -> Option<String> {
        (!self.lossy).then(|| format!(":(literal){}", self.text))
    }

    pub fn file_name(&self) -> &str {
        match self.text.rsplit_once('/') {
            Some((_, name)) => name,
            None => &self.text,
        }
    }

    pub fn parent(&self) -> &str {
        match self.text.rsplit_once('/') {
            Some((dir, _)) => dir,
            None => "",
        }
    }
}

/// One half of porcelain v2's `XY` pair.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ChangeCode {
    None,
    Modified,
    TypeChanged,
    Added,
    Deleted,
    Renamed,
    Copied,
    Unmerged,
}

impl ChangeCode {
    pub fn from_byte(b: u8) -> Option<ChangeCode> {
        Some(match b {
            b'.' | b' ' => ChangeCode::None,
            b'M' => ChangeCode::Modified,
            b'T' => ChangeCode::TypeChanged,
            b'A' => ChangeCode::Added,
            b'D' => ChangeCode::Deleted,
            b'R' => ChangeCode::Renamed,
            b'C' => ChangeCode::Copied,
            b'U' => ChangeCode::Unmerged,
            _ => return None,
        })
    }

    /// The single character the UI shows in its 14px status column.
    pub fn letter(self) -> char {
        match self {
            ChangeCode::None => ' ',
            ChangeCode::Modified => 'M',
            ChangeCode::TypeChanged => 'T',
            ChangeCode::Added => 'A',
            ChangeCode::Deleted => 'D',
            ChangeCode::Renamed => 'R',
            ChangeCode::Copied => 'C',
            ChangeCode::Unmerged => 'U',
        }
    }

    pub fn is_change(self) -> bool {
        self != ChangeCode::None
    }
}

/// The seven `XY` pairs porcelain v2 reports as unmerged (`u`) records.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ConflictKind {
    BothDeleted,
    AddedByUs,
    DeletedByThem,
    AddedByThem,
    DeletedByUs,
    BothAdded,
    BothModified,
}

impl ConflictKind {
    pub fn from_xy(x: u8, y: u8) -> Option<ConflictKind> {
        Some(match (x, y) {
            (b'D', b'D') => ConflictKind::BothDeleted,
            (b'A', b'U') => ConflictKind::AddedByUs,
            (b'U', b'D') => ConflictKind::DeletedByThem,
            (b'U', b'A') => ConflictKind::AddedByThem,
            (b'D', b'U') => ConflictKind::DeletedByUs,
            (b'A', b'A') => ConflictKind::BothAdded,
            (b'U', b'U') => ConflictKind::BothModified,
            _ => return None,
        })
    }

    /// Whether our side still has a file — decides if "open changes" can show
    /// an ours/theirs diff or only one stage.
    pub fn ours_exists(self) -> bool {
        !matches!(self, ConflictKind::BothDeleted | ConflictKind::DeletedByUs)
    }

    pub fn theirs_exists(self) -> bool {
        !matches!(
            self,
            ConflictKind::BothDeleted | ConflictKind::DeletedByThem
        )
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct SubmoduleState {
    pub commit_changed: bool,
    pub modified_content: bool,
    pub has_untracked: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EntryKind {
    Tracked,
    Unmerged,
    Untracked,
    Ignored,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct StatusEntry {
    pub path: RepoPath,
    /// Only set on rename/copy records, and it is the *old* path.
    pub orig_path: Option<RepoPath>,
    /// `X` — HEAD against the index, i.e. what is staged.
    pub index: ChangeCode,
    /// `Y` — the index against the working tree, i.e. what is not staged.
    pub worktree: ChangeCode,
    pub kind: EntryKind,
    /// `None` when the entry is not a submodule.
    pub submodule: Option<SubmoduleState>,
    /// Similarity score from `R<score>` / `C<score>`, 0..=100.
    pub rename_score: Option<u8>,
    /// Always `Some` when `kind == Unmerged`.
    pub conflict: Option<ConflictKind>,
}

impl StatusEntry {
    pub fn is_staged(&self) -> bool {
        self.index.is_change() && self.conflict.is_none()
    }

    /// A file can be both staged and unstaged at once (`XY == "MM"`) and then
    /// appears in both groups — which is exactly what VS Code shows.
    pub fn is_unstaged(&self) -> bool {
        self.worktree.is_change() && self.conflict.is_none()
    }

    pub fn is_untracked(&self) -> bool {
        matches!(self.kind, EntryKind::Untracked)
    }

    pub fn is_conflicted(&self) -> bool {
        self.conflict.is_some()
    }

    /// How this entry should be decorated wherever a single status is shown
    /// (file tree, commit detail): the worse of its two halves.
    pub fn deco(&self) -> DecoStatus {
        if self.is_conflicted() {
            return DecoStatus::Conflict;
        }
        if self.is_untracked() {
            return DecoStatus::Untracked;
        }
        let worse = if code_rank(self.worktree) >= code_rank(self.index) {
            self.worktree
        } else {
            self.index
        };
        match worse {
            ChangeCode::Deleted => DecoStatus::Deleted,
            ChangeCode::Added => DecoStatus::Added,
            ChangeCode::Renamed | ChangeCode::Copied => DecoStatus::Renamed,
            ChangeCode::Unmerged => DecoStatus::Conflict,
            _ => DecoStatus::Modified,
        }
    }
}

fn code_rank(code: ChangeCode) -> u8 {
    match code {
        ChangeCode::None => 0,
        ChangeCode::Modified | ChangeCode::TypeChanged => 1,
        ChangeCode::Copied => 2,
        ChangeCode::Renamed => 3,
        ChangeCode::Added => 4,
        ChangeCode::Deleted => 5,
        ChangeCode::Unmerged => 6,
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum HeadState {
    /// `# branch.oid (initial)` — the repository has no commits yet.
    Unborn {
        branch: String,
    },
    Detached {
        oid: String,
    },
    Branch {
        name: String,
        oid: String,
    },
}

impl HeadState {
    /// What the chrome shows: a branch name, or a short sha when detached.
    pub fn label(&self) -> String {
        match self {
            HeadState::Unborn { branch } => branch.clone(),
            HeadState::Detached { oid } => oid.chars().take(7).collect(),
            HeadState::Branch { name, .. } => name.clone(),
        }
    }

    /// `false` before the first commit, which is the one case where
    /// `git reset HEAD -- <path>` fails outright.
    pub fn has_commits(&self) -> bool {
        !matches!(self, HeadState::Unborn { .. })
    }
}

/// A sequencer operation left half-finished in the repository.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RepoOperation {
    Merge,
    Rebase,
    RebaseInteractive,
    CherryPick,
    Revert,
    Bisect,
    Am,
}

/// Entries past this are dropped; `total_entries` still reports the real count
/// so the panel can say so instead of quietly showing a short list.
pub const MAX_STATUS_ENTRIES: usize = 10_000;

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct WorkingTreeStatus {
    /// This worktree's toplevel.
    pub root: PathBuf,
    /// The shared repository home — differs from `root` inside a linked
    /// worktree. Same rule `core::git::probe` already uses to group tabs.
    pub home: PathBuf,
    pub head: HeadState,
    pub upstream: Option<String>,
    pub ahead_behind: Option<(u32, u32)>,
    pub entries: Vec<StatusEntry>,
    pub total_entries: usize,
    pub truncated: bool,
    pub stash_count: u32,
    pub operation: Option<RepoOperation>,
    /// `.git/MERGE_MSG` or `SQUASH_MSG`, to pre-fill the commit box mid-merge.
    pub prefilled_message: Option<String>,
}

impl WorkingTreeStatus {
    pub fn staged(&self) -> impl Iterator<Item = &StatusEntry> {
        self.entries.iter().filter(|e| e.is_staged())
    }

    pub fn unstaged(&self) -> impl Iterator<Item = &StatusEntry> {
        self.entries
            .iter()
            .filter(|e| e.is_unstaged() && !e.is_untracked())
    }

    pub fn untracked(&self) -> impl Iterator<Item = &StatusEntry> {
        self.entries.iter().filter(|e| e.is_untracked())
    }

    pub fn conflicts(&self) -> impl Iterator<Item = &StatusEntry> {
        self.entries.iter().filter(|e| e.is_conflicted())
    }

    pub fn is_clean(&self) -> bool {
        self.entries.is_empty()
    }
}

/// How a path is decorated wherever one status has to stand for a file.
///
/// `Ord` is display precedence, lowest to highest: a directory takes the max of
/// everything beneath it, so one conflict anywhere colours the whole path up to
/// the root.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum DecoStatus {
    Ignored,
    Untracked,
    Added,
    Modified,
    Renamed,
    Deleted,
    Conflict,
}

/// Beyond this many entries the per-file map is dropped and only directories
/// stay decorated — a `node_modules` that slipped past `.gitignore` should slow
/// nothing down.
pub const MAX_DECORATED_FILES: usize = 5_000;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct DirRollup {
    pub changed: bool,
    pub conflict: bool,
}

impl DirRollup {
    pub fn merge(&mut self, status: DecoStatus) {
        match status {
            DecoStatus::Ignored => {}
            DecoStatus::Conflict => {
                self.conflict = true;
                self.changed = true;
            }
            _ => self.changed = true,
        }
    }
}

/// Repo-root-relative lookup for the file tree, built once per status refresh.
///
/// Cost is O(changed paths × depth) — independent of how big the tree is —
/// and both lookups are a single hash probe, so a row can ask during render.
#[derive(Clone, Debug, Default)]
pub struct StatusIndex {
    pub root: PathBuf,
    files: HashMap<String, DecoStatus>,
    dirs: HashMap<String, DirRollup>,
    /// Set when `files` was dropped for exceeding [`MAX_DECORATED_FILES`].
    pub files_dropped: bool,
}

impl StatusIndex {
    pub fn file(&self, repo_rel: &str) -> Option<DecoStatus> {
        self.files.get(repo_rel).copied()
    }

    pub fn dir(&self, repo_rel: &str) -> Option<DirRollup> {
        self.dirs.get(repo_rel).copied()
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty() && self.dirs.is_empty()
    }

    /// Insert one path and roll it up through every ancestor. Exposed so the
    /// builder and its tests share exactly one definition of the walk.
    pub fn insert(&mut self, repo_rel: &str, status: DecoStatus) {
        self.files
            .entry(repo_rel.to_string())
            .and_modify(|slot| *slot = (*slot).max(status))
            .or_insert(status);
        let mut cut = repo_rel;
        while let Some((parent, _)) = cut.rsplit_once('/') {
            self.dirs
                .entry(parent.to_string())
                .or_default()
                .merge(status);
            cut = parent;
        }
    }

    pub fn drop_files(&mut self) {
        self.files.clear();
        self.files_dropped = true;
    }
}
