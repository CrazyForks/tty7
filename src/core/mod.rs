//! Domain core: the configuration model, session persistence, the action
//! vocabulary shared by the shell and the terminal view, and the streaming OSC
//! tokenizer shared by the daemon- and client-side output scanners.
//!
//! These modules are framework-light and depend on neither `ui` nor `terminal`,
//! so the dependency arrow always points *inward* to here.
//!
//! Most of it now lives one crate down, in `tty7-core`, so the headless
//! `tty7-server` can share it — the modules re-exported below are that crate's,
//! reachable under their original `crate::core::…` paths. What stays declared
//! here is either gpui-shaped outright (`actions`, `update`) or the gpui half
//! of a type whose data moved down (`config`, `session`, `window_state`).

// A glob, so every module `tty7-core` grows is reachable here for free. The
// four `pub mod`s below deliberately shadow their glob-imported namesakes: each
// is a thin layer that re-exports the core module's contents itself — gpui for
// `config` / `session` / `window_state`, the OS keychain for `keychain`.
pub use tty7_core::core::*;

pub mod actions;
pub mod agent_prompt;
pub mod config;
pub mod keychain;
pub mod session;
pub mod ssh_config;
pub mod update;
pub mod window_state;
