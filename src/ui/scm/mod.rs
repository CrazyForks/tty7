//! Source control: the panel, its file rows, the commit box, and the graph.
//!
//! Every file here hangs `impl Tty7App` blocks, the same shape `sftp.rs` and
//! `file_tree.rs` use. The directory only keeps the surface from piling into
//! `right_panel.rs`.

// The scaffolding lands one step ahead of the code that consumes it: the panel
// still renders the old flat list, so the status helpers, the path helpers and
// most of `ScmPanelState` have no caller yet. Each `allow` comes off as its
// module gets wired up rather than being left as a blanket at the top.
pub(crate) mod actions;
#[allow(dead_code)]
pub(crate) mod panel;
#[allow(dead_code)]
pub(crate) mod path;
#[allow(dead_code)]
pub(crate) mod state;
#[allow(dead_code)]
pub(crate) mod status;

pub(crate) use actions::ScmIntent;
pub(crate) use state::{GraphState, ScmPanelState};
