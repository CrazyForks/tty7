//! One commit, in the panel's own body.
//!
//! Replacing the body rather than opening a third kind of container: the
//! panel already knows how to draw a list of changed files, and the only
//! things a commit adds above it are its message and who wrote it. The file
//! rows are the same rows, minus the buttons — nothing here should be able to
//! drift from what the working tree shows.
//!
//! The patch itself still goes to the full-screen overlay. 260px is not a
//! place to read a diff.

use gpui::{AnyElement, Context};

use crate::ui::app::Tty7App;
use crate::ui::scm::state::CommitDetailView;

impl Tty7App {
    /// The commit detail body, shown in place of the file groups.
    pub(crate) fn render_commit_detail(
        &mut self,
        _detail: &CommitDetailView,
        _window: &mut gpui::Window,
        _cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        None
    }
}
