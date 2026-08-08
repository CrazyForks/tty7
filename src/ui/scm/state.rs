//! Everything the source control panel remembers between frames.
//!
//! All of it is app-level, hanging off `Tty7App.scm` rather than off a tab.
//! The panel has exactly one instance per window — the same model
//! `RightPanelState.diff_cwd` already uses. `Tab.diff_overlay` and `Tab.code`
//! are per-tab because they are full-screen overlays that belong to a tab; a
//! side panel does not.
//!
//! The one thing that must survive everything is the commit draft, so it is
//! keyed by repository rather than by tab or pane: a working tree has one
//! pending message, no matter how many panes are looking at it.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use gpui::Entity;
use gpui_component::input::InputState;
use tty7_core::core::git::status::HeadState;

use crate::ui::host_ops::HostId;

/// Which working tree a piece of state belongs to.
///
/// The host is part of the key because the same path can exist on this machine
/// and on three different remotes at once, and they are unrelated repositories.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub(crate) struct RepoKey {
    pub(crate) host: HostId,
    pub(crate) root: PathBuf,
}

/// The four sections of the file list, in the order they are rendered.
///
/// `Merge` only appears while a merge is unresolved, which is why the panel
/// asks for it by variant rather than always drawing a header.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum ScmGroup {
    Merge,
    Staged,
    Changes,
    Untracked,
}

impl ScmGroup {
    pub(crate) const ORDER: [ScmGroup; 4] = [
        ScmGroup::Merge,
        ScmGroup::Staged,
        ScmGroup::Changes,
        ScmGroup::Untracked,
    ];
}

#[derive(Default)]
pub(crate) struct ScmPanelState {
    /// Which repository the panel is showing. Follows the active pane unless
    /// `repo_override` says otherwise.
    pub(crate) repo: Option<RepoKey>,
    /// Set when the user picks a repository from the multi-repo dropdown, and
    /// cleared whenever the active tab changes — an explicit choice should
    /// outlive a pane switch inside one tab, not a jump to somewhere else.
    pub(crate) repo_override: Option<RepoKey>,
    /// Unsent commit messages, one per working tree.
    pub(crate) drafts: HashMap<RepoKey, String>,
    /// The commit box. `None` until the panel has been rendered once: an
    /// `InputState` needs a real window to be created in, and this struct is
    /// built by `Default` alongside the rest of `Tty7App`.
    pub(crate) commit_input: Option<Entity<InputState>>,
    /// Which repository's draft the box is currently holding. A change here
    /// is what moves one draft out and the next one in.
    pub(crate) commit_repo: Option<RepoKey>,
    /// A commit that has been dispatched: the repository, what HEAD was
    /// before it, and the message it carried. Held until HEAD moves, so a
    /// commit a hook rejects leaves the message in the box.
    pub(crate) committing: Option<(RepoKey, HeadState, String)>,
    /// Whether the next commit rewrites HEAD. Armed from the commit dropdown
    /// rather than a checkbox row — 260px does not have a row to spare.
    pub(crate) amend: bool,
    /// Groups the user folded shut, and the ones whose fold state they have
    /// set at all. Both are needed: a group nobody has touched follows the
    /// default for its size (a thousand untracked files start folded), and
    /// opening one by hand has to outlast the next file landing in it.
    pub(crate) collapsed: HashSet<ScmGroup>,
    pub(crate) toggled: HashSet<ScmGroup>,
    /// Working directory → the repository root containing it. Cached because
    /// the root is what every write and every cache lookup is keyed by, and
    /// only a `git status` can say what it is.
    pub(crate) roots: HashMap<(HostId, PathBuf), PathBuf>,
    /// When the panel last asked for a status that it did not get back.
    pub(crate) probe_attempt: HashMap<(HostId, PathBuf), std::time::Instant>,
    /// The status the last frame drew, as (cache key, `Arc` identity). The
    /// watcher compares against it so a global write that changed nothing does
    /// not ask for another frame.
    pub(crate) seen: Option<((HostId, PathBuf), usize)>,
    pub(crate) watch: Option<gpui::Subscription>,
    /// The cheap per-tab git status the panel last reacted to. A change in it
    /// means a command touched the repository and the expensive status is due
    /// another look.
    pub(crate) last_tab_status: Option<crate::terminal::git_status::GitStatus>,
    pub(crate) graph: GraphState,
    /// When set, the panel body is replaced by a single commit's detail view
    /// instead of the working tree.
    pub(crate) detail: Option<CommitDetailView>,
    pub(crate) scroll: gpui::ScrollHandle,
}

impl ScmPanelState {
    /// The repository the panel should act on: an explicit pick wins over
    /// whatever the active pane happens to be sitting in.
    pub(crate) fn active_repo(&self) -> Option<&RepoKey> {
        self.repo_override.as_ref().or(self.repo.as_ref())
    }

    pub(crate) fn draft(&self, repo: &RepoKey) -> &str {
        self.drafts.get(repo).map(String::as_str).unwrap_or("")
    }

    /// Whether a group renders folded. `count` decides it only for a group the
    /// user has never touched.
    pub(crate) fn group_collapsed(&self, group: ScmGroup, count: usize) -> bool {
        if self.toggled.contains(&group) {
            self.collapsed.contains(&group)
        } else {
            crate::ui::scm::panel::starts_collapsed(group, count)
        }
    }

    pub(crate) fn set_group_collapsed(&mut self, group: ScmGroup, collapsed: bool) {
        self.toggled.insert(group);
        if collapsed {
            self.collapsed.insert(group);
        } else {
            self.collapsed.remove(&group);
        }
    }
}

#[derive(Default)]
pub(crate) struct GraphState {
    /// Mirrors `Config::scm_graph_expanded`, which starts `false`: the history
    /// section unfurling on first open would make the panel look like a mess
    /// nobody asked for.
    pub(crate) expanded: bool,
    /// How many commits have been asked for so far. Paging grows this and
    /// re-runs the query rather than using `--skip`, which is O(skip) to walk
    /// and shifts under you when a ref moves between pages.
    pub(crate) requested: usize,
    pub(crate) loading: bool,
    /// Filter box. Like `commit_input`, created on first render.
    pub(crate) search: Option<Entity<InputState>>,
    /// `refs/heads/...` the graph is restricted to; empty means all refs.
    pub(crate) branch_filter: Option<String>,
    /// The selected row, by full sha.
    pub(crate) selected: Option<String>,
    pub(crate) scroll: gpui::ScrollHandle,
    /// Height of the history section, and whether its divider is being
    /// dragged. Shaped like `right_panel_width` / `right_panel_dragging` so
    /// the same drag code works on the other axis.
    pub(crate) height: std::rc::Rc<std::cell::Cell<f32>>,
    pub(crate) dragging: std::rc::Rc<std::cell::Cell<bool>>,
}

/// The panel's second-level view: one commit's metadata and the files it
/// touched. A file-level diff is not shown here — that opens the full-screen
/// overlay, because 260px cannot render a diff and pretending otherwise would
/// mean inventing a third kind of container.
pub(crate) struct CommitDetailView {
    pub(crate) repo: RepoKey,
    pub(crate) oid: String,
    pub(crate) loading: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(root: &str) -> RepoKey {
        RepoKey {
            host: HostId::LOCAL,
            root: PathBuf::from(root),
        }
    }

    #[test]
    fn an_explicit_repo_pick_outranks_the_active_pane() {
        let mut state = ScmPanelState::default();
        assert!(state.active_repo().is_none());
        state.repo = Some(key("/a"));
        assert_eq!(state.active_repo(), Some(&key("/a")));
        state.repo_override = Some(key("/b"));
        assert_eq!(state.active_repo(), Some(&key("/b")));
        state.repo_override = None;
        assert_eq!(state.active_repo(), Some(&key("/a")));
    }

    #[test]
    fn drafts_are_keyed_by_repository_not_by_path_alone() {
        let mut state = ScmPanelState::default();
        state.drafts.insert(key("/a"), "wip".into());
        assert_eq!(state.draft(&key("/a")), "wip");
        assert_eq!(state.draft(&key("/b")), "");
    }
}
