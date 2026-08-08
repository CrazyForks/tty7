//! History: the commits themselves, the refs pointing at them, and the lane
//! layout the graph is drawn from.
//!
//! The lane assignment lives here, not in the renderer, for two reasons. It is
//! a pure function over `(sha, parents)` and therefore the part of the graph
//! most worth testing exhaustively; and gpui re-runs `render` on every notify,
//! so an O(commits × lanes) pass in a paint closure would be burned every
//! frame for a result that only changes when the history does.

use smallvec::SmallVec;

/// Full hex object id. Kept as `String` rather than `[u8; 20]` because sha256
/// repositories exist and the extra allocation is noise next to the subject.
pub type Oid = String;

/// Lane index as assigned by the layout pass — the *true* column, before the
/// renderer folds anything past its width cap into an overflow column.
pub type Lane = u16;

/// Which palette entry a lane draws with. Equal to the lane it was created for
/// and never reassigned, which is what keeps a branch one colour for its whole
/// life: a branch holds the same lane from its tip until it is merged, because
/// the first parent inherits the lane in place and never migrates.
pub type ColorIdx = u16;

pub const GRAPH_PAGE: usize = 200;
pub const MAX_GRAPH_COMMITS: usize = 5_000;
pub const MAX_LANES: Lane = 32;
pub const MAX_REFS: usize = 2_000;
pub const MAX_SUBJECT_BYTES: usize = 512;
pub const MAX_BODY_BYTES: usize = 8 * 1024;
pub const MAX_LOG_BYTES: usize = 16 * 1024 * 1024;

/// Record separator for `log --pretty`. Deliberately not NUL: `git log -z`
/// already uses NUL between records, so a NUL field separator could only be
/// told apart by counting fields — and one NUL inside a commit message (git
/// objects allow it) would desynchronise the whole stream. RS and US cannot
/// occur in a sha, a refname, an ISO date or an address.
pub const REC_SEP: u8 = 0x1e;
pub const FIELD_SEP: u8 = 0x1f;

/// A timestamp plus the author's own UTC offset, so times can be shown in the
/// zone they were written in. Parsed from `%aI` / `%cI`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct OffsetTs {
    pub unix: i64,
    pub offset_minutes: i32,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Signature {
    pub name: String,
    pub email: String,
    pub at: OffsetTs,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum RefKind {
    /// Sorts last so the highest-priority chip wins a `max()`.
    Other,
    RemoteBranch,
    Tag,
    LocalBranch,
    Head,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct RefDeco {
    pub kind: RefKind,
    /// `refs/heads/feature/x`
    pub full: String,
    /// `feature/x`
    pub short: String,
    /// Carried the `HEAD -> ` prefix in `%D`.
    pub is_head: bool,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Commit {
    pub oid: Oid,
    pub parents: SmallVec<[Oid; 2]>,
    pub author: Signature,
    pub committer: Signature,
    pub summary: String,
    pub body: String,
    pub refs: Vec<RefDeco>,
}

impl Commit {
    pub fn short(&self) -> &str {
        let n = self.oid.len().min(7);
        &self.oid[..n]
    }

    pub fn is_merge(&self) -> bool {
        self.parents.len() > 1
    }
}

/// One line inside a single row's band: from the row's top edge to its bottom.
///
/// Row-local on purpose. A model that described whole polylines across rows
/// could not emit a line until its far end arrived, so a long-lived branch
/// would stay invisible until the page holding its parent loaded — the bug
/// Zed's own graph has. Here a row is final the moment it is produced, which
/// is also what makes paging free of visual reflow.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Edge {
    /// Straight through the band without touching this row's node.
    Pass { lane: Lane, color: ColorIdx },
    /// Comes down from `from` on the top edge and ends at this row's node.
    In { from: Lane, color: ColorIdx },
    /// Leaves this row's node for `to` on the bottom edge.
    Out { to: Lane, color: ColorIdx },
}

impl Edge {
    pub fn color(self) -> ColorIdx {
        match self {
            Edge::Pass { color, .. } | Edge::In { color, .. } | Edge::Out { color, .. } => color,
        }
    }

    /// Paint order: pass-through lines first, so the node's own line lands on
    /// top of anything crossing behind it.
    pub fn paint_rank(&self) -> u8 {
        match self {
            Edge::Pass { .. } => 0,
            Edge::In { .. } => 1,
            Edge::Out { .. } => 2,
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct GraphRow {
    pub node: Lane,
    pub color: ColorIdx,
    /// 0 = root commit, 1 = ordinary, >1 = merge (>2 = octopus).
    pub parents: u8,
    pub edges: SmallVec<[Edge; 4]>,
}

/// Which refs the log is walked from.
#[derive(Clone, PartialEq, Eq, Hash, Debug, Default)]
pub enum GraphScope {
    Head,
    /// HEAD plus its upstream — the default, matching VS Code.
    #[default]
    HeadAndUpstream,
    All,
    Refs(Vec<String>),
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CommitPage {
    pub commits: Vec<Commit>,
    /// Same length as `commits`.
    pub rows: Vec<GraphRow>,
    pub max_lanes: Lane,
    pub scope: GraphScope,
    pub requested: usize,
    /// git returned fewer than asked for, so this is the end of history.
    pub complete: bool,
    pub truncated_lanes: bool,
    /// Lanes still open past the last row — drawn as fading stubs so a page
    /// boundary does not read as a row of root commits.
    pub open_lanes: Vec<Lane>,
}

// `LaneAlloc` — the append-only lane assigner these rows come out of — is
// defined below by the graph layout pass. It is append-only by design: a later
// page extends the graph without reflowing what is already on screen.
