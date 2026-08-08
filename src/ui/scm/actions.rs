//! Where the source control actions and palette commands land.
//!
//! Two of them are finished here because they are pure view state and have
//! nothing to wait for. The rest funnel through one `ScmIntent` match so the
//! wiring — action, key binding, palette entry, menu item — can be verified
//! now, and each arm gets its body filled in by the step that owns it.

use gpui::Context;

use crate::core::config::DiffViewMode;
use crate::ui::app::Tty7App;

/// One entry point for every source control verb.
///
/// A single enum rather than fourteen methods: the actions, the palette and
/// (later) the row buttons all want the same behaviour, and routing them
/// through one match is what keeps the three from drifting apart.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ScmIntent {
    Commit,
    CommitAmend,
    StageAll,
    UnstageAll,
    DiscardAll,
    Refresh,
    Sync,
    Push,
    Pull,
    Fetch,
    CheckoutBranch,
    CreateBranch,
}

impl Tty7App {
    /// Fold the history section open or shut and remember it.
    pub(crate) fn scm_toggle_graph(&mut self, cx: &mut Context<Self>) {
        let next = !self.scm.graph.expanded;
        self.scm.graph.expanded = next;
        self.update_config(cx, |cfg| cfg.scm_graph_expanded = next);
        cx.notify();
    }

    /// Flip the diff overlay between side-by-side and unified.
    ///
    /// Global rather than per-overlay, matching `diffEditor.renderSideBySide`:
    /// someone who prefers unified prefers it for every file.
    pub(crate) fn toggle_diff_view_mode(&mut self, cx: &mut Context<Self>) {
        let next = match cx.global::<crate::core::config::Config>().diff_view {
            DiffViewMode::Split => DiffViewMode::Unified,
            DiffViewMode::Unified => DiffViewMode::Split,
        };
        self.update_config(cx, |cfg| cfg.diff_view = next);
        cx.notify();
    }

    pub(crate) fn run_scm_action(
        &mut self,
        intent: ScmIntent,
        _window: &mut gpui::Window,
        cx: &mut Context<Self>,
    ) {
        match intent {
            // Refresh is the one verb the panel can already answer: the flat
            // diff probe behind the old Changes tab is exactly what it means.
            ScmIntent::Refresh => self.right_panel_refresh_changes(cx),
            // Staging, discarding and committing need `core::git::ops`, which
            // arrives with the row buttons and the commit box.
            ScmIntent::StageAll
            | ScmIntent::UnstageAll
            | ScmIntent::DiscardAll
            | ScmIntent::Commit
            | ScmIntent::CommitAmend => {}
            // The network verbs and the branch switcher come with the
            // repository header row.
            ScmIntent::Sync
            | ScmIntent::Push
            | ScmIntent::Pull
            | ScmIntent::Fetch
            | ScmIntent::CheckoutBranch
            | ScmIntent::CreateBranch => {}
        }
    }
}
