//! The source control panel's data pipeline: one cache of what each repository
//! looks like, one way to change it, and one way to say "that is now stale".
//!
//! Deliberately separate from [`super::git_status`]. That cache answers a
//! cheap question — branch name and a `+N −M` for a tab badge — on every cwd
//! change and every command boundary, for every pane. This one answers the
//! expensive question (`status --porcelain=v2 -uall`, seconds on a large
//! repository) and only while something is actually looking. Folding the two
//! would put the expensive probe on the cheap trigger.
//!
//! Invalidation is by epoch rather than by key. Working out which cache
//! entries a `git add` touched is a losing game; bumping a counter for the
//! repository and letting readers notice they are behind is not.

// This module is the contract the panel, the file tree and the `.git` watcher
// are built against, and it landed before any of them. Without the allow every
// item here reports unused and the real dead code elsewhere gets lost in the
// noise. Take it off once the panel calls `scm_refresh` — by then anything
// still unused genuinely is.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui::{Context, Window};

use crate::core::git::ops::{GitOp, GitOpError, GitOpErrorKind, GitOpOutcome, run_op};
use crate::core::git::status::{StatusIndex, WorkingTreeStatus, probe_status};
use crate::ui::app::Tty7App;
use crate::ui::host_ops::{ByHost, HostId, HostOps, InFlight, SharedHost};

/// How many network operations one host may have in flight.
///
/// The far side serves every request from one worker pool, and keepalive's
/// `Ping` queues behind the rest of it. Enough concurrent pushes and the ping
/// misses its own deadline for long enough that the link is declared dead —
/// so the client, not the server, keeps the number small.
pub const MAX_CONCURRENT_NETWORK_OPS: usize = 2;

#[derive(Default)]
pub struct ScmData {
    /// repo root → the last status we read.
    status: ByHost<PathBuf, Arc<WorkingTreeStatus>>,
    /// repo root → the decoration index derived from that status.
    index: ByHost<PathBuf, Arc<StatusIndex>>,
    /// repo root → a counter bumped by anything that could have changed it.
    epoch: ByHost<PathBuf, u64>,
    /// repo root → the epoch the cached status was read at.
    read_at: ByHost<PathBuf, u64>,
    probes: InFlight<(HostId, PathBuf)>,
    network: ByHost<PathBuf, usize>,
}

impl gpui::Global for ScmData {}

impl ScmData {
    pub fn status_for(&self, host: HostId, root: &Path) -> Option<Arc<WorkingTreeStatus>> {
        self.status.get(host, root).cloned()
    }

    pub fn index_for(&self, host: HostId, root: &Path) -> Option<Arc<StatusIndex>> {
        self.index.get(host, root).cloned()
    }

    pub fn epoch(&self, host: HostId, root: &Path) -> u64 {
        self.epoch.get(host, root).copied().unwrap_or(0)
    }

    /// Whether what we hold was read before the last thing that changed it.
    /// A repository we have never probed counts as stale.
    pub fn is_stale(&self, host: HostId, root: &Path) -> bool {
        match self.read_at.get(host, root) {
            Some(read) => *read < self.epoch(host, root),
            None => true,
        }
    }

    /// Mark a repository changed. Every write, every `.git` watcher event and
    /// every command boundary lands here; readers reprobe on their next look.
    pub fn bump(&mut self, host: HostId, root: &Path) {
        let next = self.epoch(host, root) + 1;
        self.epoch.insert(host, root.to_path_buf(), next);
        self.probes.invalidate(&(host, root.to_path_buf()));
    }

    /// Drop everything for a host that went away, so a reconnect does not show
    /// the state the machine was in when it dropped off.
    pub fn clear_host(&mut self, host: HostId) {
        self.status.clear_host(host);
        self.index.clear_host(host);
        self.epoch.clear_host(host);
        self.read_at.clear_host(host);
        self.network.clear_host(host);
    }

    fn network_slots(&self, host: HostId, root: &Path) -> usize {
        self.network.get(host, root).copied().unwrap_or(0)
    }
}

/// The status the panel draws from, or `None` until the first probe lands.
///
/// A free function rather than a method because it is read during `render`,
/// where all anyone holds is `&App`. `try_global` because a view test need
/// never have installed one.
pub(crate) fn status_of(
    cx: &gpui::App,
    host: HostId,
    root: &Path,
) -> Option<Arc<WorkingTreeStatus>> {
    cx.try_global::<ScmData>()?.status_for(host, root)
}

/// The per-path decoration index the file tree looks up during `render`.
pub(crate) fn index_of(cx: &gpui::App, host: HostId, root: &Path) -> Option<Arc<StatusIndex>> {
    cx.try_global::<ScmData>()?.index_for(host, root)
}

impl Tty7App {
    /// Read a repository's status, unless a read is already running or what we
    /// hold is current. Safe to call from `render`.
    pub(crate) fn scm_refresh(&mut self, host: SharedHost, root: PathBuf, cx: &mut Context<Self>) {
        let id = host.id();
        let key = (id, root.clone());
        let data = cx.default_global::<ScmData>();
        if !data.is_stale(id, &root) || !data.probes.begin(key.clone()) {
            return;
        }
        let at = data.epoch(id, &root);

        let probe_root = root.clone();
        HostOps::run_detached(
            host.clone(),
            cx,
            move |h| {
                let status = probe_status(h, &probe_root)?;
                let index = StatusIndex::build(&status);
                Some((Arc::new(status), Arc::new(index)))
            },
            move |cx, result| {
                let data = cx.default_global::<ScmData>();
                // The return says whether the epoch moved while this was in
                // flight. Nothing to do with it: `read_at` records the epoch
                // the read *started* at, so a bump has already left this
                // result behind and the next look reprobes on its own.
                data.probes.finish(&key);
                if let Some((status, index)) = result {
                    data.status.insert(id, root.clone(), status);
                    data.index.insert(id, root.clone(), index);
                    data.read_at.insert(id, root.clone(), at);
                }
            },
        );
    }

    /// Change the repository, then let everyone notice.
    ///
    /// Confirmation of a destructive operation is the caller's job, not this
    /// one's — see [`GitOp::destructive`]. `run_git_op` has to stay callable
    /// from a flow that already asked, and from a test.
    pub(crate) fn run_git_op(
        &mut self,
        host: SharedHost,
        root: PathBuf,
        op: GitOp,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let Some(status) = status_of(cx, host.id(), &root) else {
            return;
        };
        let head = status.head.clone();
        let id = host.id();
        let network = op.is_network();

        if network {
            let data = cx.default_global::<ScmData>();
            if data.network_slots(id, &root) >= MAX_CONCURRENT_NETWORK_OPS {
                return;
            }
            let next = data.network_slots(id, &root) + 1;
            data.network.insert(id, root.clone(), next);
        }

        let op_root = root.clone();
        HostOps::run_in(
            host.clone(),
            window,
            cx,
            move |h| run_op(h, &op_root, &op, &head),
            move |app, result, window, cx| {
                if network {
                    let data = cx.default_global::<ScmData>();
                    let left = data.network_slots(id, &root).saturating_sub(1);
                    data.network.insert(id, root.clone(), left);
                }
                cx.default_global::<ScmData>().bump(id, &root);
                app.on_git_op_done(host, root, result, window, cx);
            },
        );
    }

    fn on_git_op_done(
        &mut self,
        host: SharedHost,
        root: PathBuf,
        result: Result<GitOpOutcome, GitOpError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(_) => {
                self.scm_refresh(host, root, cx);
                cx.notify();
            }
            Err(err) => {
                self.report_git_op_error(&err, window, cx);
                self.scm_refresh(host, root, cx);
            }
        }
    }

    /// Say what went wrong, and — when the answer is a credential a window
    /// cannot supply — offer the one thing tty7 has that a GUI does not: a
    /// real terminal to run it in.
    fn report_git_op_error(
        &mut self,
        err: &GitOpError,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use crate::ui::i18n::{L10nKey, t_fmt};
        let text = t_fmt(
            L10nKey::HostOpsError,
            &[("context", err.op), ("error", &err.message)],
        );
        gpui_component::WindowExt::push_notification(window, text, cx);
        if err.kind == GitOpErrorKind::AuthRequired {
            log::info!(
                "git {} needs a credential; re-run in a pane: {}",
                err.op,
                shell_quote(&err.rerun_argv),
            );
        }
    }
}

/// Render an argv as a line a shell will read back identically.
///
/// Single quotes with the `'\''` escape: the only characters that survive
/// unquoted are the ones that cannot mean anything else.
pub(crate) fn shell_quote(argv: &[String]) -> String {
    argv.iter()
        .map(|arg| {
            let safe = !arg.is_empty()
                && arg
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_./=:@,+".contains(&b));
            if safe {
                arg.clone()
            } else {
                format!("'{}'", arg.replace('\'', r"'\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> PathBuf {
        PathBuf::from("/repo")
    }

    #[test]
    fn a_repository_nobody_has_read_counts_as_stale() {
        let data = ScmData::default();
        assert!(data.is_stale(HostId::LOCAL, &root()));
        assert_eq!(data.epoch(HostId::LOCAL, &root()), 0);
    }

    #[test]
    fn a_bump_makes_a_fresh_read_stale_again() {
        let mut data = ScmData::default();
        data.read_at.insert(HostId::LOCAL, root(), 0);
        assert!(!data.is_stale(HostId::LOCAL, &root()));

        data.bump(HostId::LOCAL, &root());
        assert!(
            data.is_stale(HostId::LOCAL, &root()),
            "a write has to send the next look back to git"
        );
    }

    #[test]
    fn epochs_do_not_leak_between_hosts() {
        let mut data = ScmData::default();
        let other = HostId::from_connection_key("somewhere-else");
        data.bump(HostId::LOCAL, &root());
        assert_eq!(data.epoch(HostId::LOCAL, &root()), 1);
        assert_eq!(
            data.epoch(other, &root()),
            0,
            "the same path on two machines is two repositories"
        );
    }

    #[test]
    fn clearing_a_host_forgets_what_it_looked_like() {
        let mut data = ScmData::default();
        data.bump(HostId::LOCAL, &root());
        data.read_at.insert(HostId::LOCAL, root(), 1);
        data.clear_host(HostId::LOCAL);
        assert!(
            data.is_stale(HostId::LOCAL, &root()),
            "a reconnect must not show the state from before the drop"
        );
    }

    #[test]
    fn shell_quote_leaves_a_plain_argv_alone() {
        let argv = ["git", "push", "origin", "main"].map(String::from);
        assert_eq!(shell_quote(&argv), "git push origin main");
    }

    #[test]
    fn shell_quote_survives_a_round_trip_through_a_shell() {
        let argv = [
            "git".to_string(),
            "commit".to_string(),
            "-m".to_string(),
            "it's a \"quoted\" $message; rm -rf /".to_string(),
        ];
        assert_eq!(
            shell_quote(&argv),
            r#"git commit -m 'it'\''s a "quoted" $message; rm -rf /'"#
        );
    }

    #[test]
    fn shell_quote_does_not_leave_an_empty_argument_bare() {
        assert_eq!(shell_quote(&["git".into(), String::new()]), "git ''");
    }
}
