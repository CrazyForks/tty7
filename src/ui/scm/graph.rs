//! The history section at the foot of the panel.
//!
//! Its job is shape, not text. 260px leaves room for roughly 26 characters
//! beside the lanes, and this repository's commit subjects run to a median of
//! 64 — so what a reader gets here is where the branches are, where they
//! merged, which refs sit where, and how recently anything moved. Reading a
//! message is the commit detail view's job, one click away.
//!
//! That is also VS Code's own reading of a sidebar graph, and it is why the
//! conventional-commit prefix is lifted out into a chip rather than left to
//! eat half the line.

use gpui::{AnyElement, Context};

use crate::ui::app::Tty7App;
use crate::ui::scm::state::RepoKey;

impl Tty7App {
    /// The history section, when it is expanded and has something to draw.
    ///
    /// Sits below the file list as its own scroll region rather than at the
    /// end of one: the graph pages, and sharing a scroller would mean scrolling
    /// back past hundreds of commits to reach the message box.
    pub(crate) fn render_graph_section(
        &mut self,
        _repo: &RepoKey,
        _window: &mut gpui::Window,
        _cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        None
    }
}
