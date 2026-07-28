//! The persistent terminal daemon, as the GUI sees it.
//!
//! The daemon itself — PTY ownership, replay rings, fan-out, the wire protocol,
//! the native SSH engine — moved wholesale into `tty7-core` when the headless
//! `tty7-server` needed to run exactly the same code on a machine with no
//! display. Nothing about it was GUI-shaped to begin with (it has never
//! referenced gpui, `terminal`, or `ui`), so the move was a relocation, not a
//! rewrite.
//!
//! This re-export keeps the GUI's call sites reading `crate::daemon::protocol`,
//! `crate::daemon::spawn::ensure_running`, and so on, exactly as before. The
//! client-side terminal that talks the protocol is still
//! `terminal::remote::RemoteTerminal`.

pub use tty7_core::daemon::*;
