//! Source control: the panel, its file rows, the commit box, and the graph.
//!
//! Every file here hangs `impl Tty7App` blocks, the same shape `sftp.rs` and
//! `file_tree.rs` use. The directory only keeps the surface from piling into
//! `right_panel.rs`.

// What is left unused is what the graph and the commit detail view will call:
// `relative_time` has no row to date yet, and `status_rank` is the file tree's
// to use. Both allows come off with the step that wires them up.
pub(crate) mod actions;
pub(crate) mod panel;
#[allow(dead_code)]
pub(crate) mod path;
#[allow(dead_code)]
pub(crate) mod state;
#[allow(dead_code)]
pub(crate) mod status;

pub(crate) use actions::ScmIntent;
pub(crate) use state::{GraphState, ScmPanelState};
