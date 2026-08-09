//! Source control: the panel, its file rows, the commit box, and the graph.
//!
//! Every file here hangs `impl Tty7App` blocks, the same shape `sftp.rs` and
//! `file_tree.rs` use. The directory only keeps the surface from piling into
//! `right_panel.rs`.

// What is left unused is what the commit detail view will call; `status_rank`
// is the file tree's to use. Those allows come off with the step that wires
// them up — the graph's did, with this one.
pub(crate) mod actions;
#[allow(dead_code)]
pub(crate) mod detail;
pub(crate) mod graph;
pub(crate) mod panel;
pub(crate) mod path;
pub(crate) mod state;
#[allow(dead_code)]
pub(crate) mod status;

pub(crate) use actions::ScmIntent;
pub(crate) use state::{GraphState, ScmPanelState};
