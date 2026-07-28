//! A lightweight git snapshot for a pane's working directory — the current
//! branch and the working-tree diff size — rendered as the sidebar row's third
//! line (`⎇ feat/x  +6 −5`): each session fronted with its branch and change
//! count.
//!
//! Snapshots are shared through [`GitStatusCache`], a process-wide map keyed
//! by machine *and* work-tree root: every pane whose cwd resolves into the same
//! repo reads the *same* entry, so ten tabs in one repo show one truth,
//! refreshed by whichever pane probed last — not ten drifting copies refreshed
//! on ten different schedules. Probes stay per-trigger (a pane's cwd change,
//! command end, or agent-turn end — see [`crate::terminal::view`]) but are
//! deduped in-flight, so simultaneous triggers from panes in the same directory
//! cost one `git` invocation, not one per pane.
//!
//! **Machine is part of every key.** `/home/me/proj` is a real path on this
//! laptop and on the box it is SSH'd into, and they are different repositories
//! on different branches. A cache keyed by path alone would serve one's branch
//! line for the other, so every table here is a [`ByHost`] and every entry
//! point takes the [`HostId`] the cwd belongs to.
//!
//! The probe itself — [`probe`], [`branch_name`], and the [`git`] invocation
//! every git read in tty7 funnels through — lives in `tty7-core`, because the
//! remote server has to answer the same questions the same way. All three now
//! take the [`Host`](crate::ui::host_ops::Host) to ask, which is what lets a
//! pane on another machine report its own repository instead of reporting
//! nothing. What stays here is the cache, which is a gpui `Global`.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub use crate::core::git::{GitStatus, RepoSnapshot, branch_name, git, probe};
use crate::ui::host_ops::{ByHost, HostId, InFlight};

/// The process-wide snapshot store (a gpui [`Global`](gpui::Global)): pane
/// cwds grouped by work-tree root, one [`GitStatus`] per root. Views read
/// through [`status_for`](Self::status_for); the probe loop in
/// [`crate::terminal::view`] brackets each background probe with
/// [`begin_probe`](Self::begin_probe) / [`finish_probe`](Self::finish_probe).
///
/// In-flight dedup is keyed by cwd (the root isn't known until a first probe
/// answers), so two panes at the same directory share one probe; panes in
/// *different* subdirectories of one repo can still race a redundant probe —
/// rare, and both land the same answer.
#[derive(Default)]
pub struct GitStatusCache {
    /// cwd → its work-tree root; `None` = probed and found not to be a repo.
    roots: ByHost<PathBuf, Option<PathBuf>>,
    /// work-tree root → the repository home it belongs to (see
    /// [`RepoSnapshot::home`]). Identity for a plain checkout; the main
    /// root for a linked worktree, so the sidebar groups them together.
    homes: ByHost<PathBuf, PathBuf>,
    /// root → the snapshot every pane in that tree shares.
    status: ByHost<PathBuf, GitStatus>,
    /// Probes in flight, and which of those were re-triggered while flying —
    /// so concurrent triggers fold into one invocation and the newest
    /// trigger's state is still observed. Keyed by `(host, cwd)`: two machines
    /// at the same path are two independent probes.
    probes: InFlight<(HostId, PathBuf)>,
    /// When each cwd's last probe *landed*, for the throttle that opportunistic
    /// triggers go through ([`begin_probe_throttled`](Self::begin_probe_throttled)).
    last_probe: ByHost<PathBuf, Instant>,
}

impl gpui::Global for GitStatusCache {}

impl GitStatusCache {
    /// The snapshot for a pane at `cwd`: resolved through its work-tree root,
    /// so every pane in the same repo answers identically. `None` before the
    /// first probe lands or when `cwd` isn't in a repo.
    pub fn status_for(&self, host: HostId, cwd: &Path) -> Option<GitStatus> {
        let root = self.roots.get(host, cwd)?.as_ref()?;
        self.status.get(host, root).cloned()
    }

    /// What the cache *knows* about the repository `cwd` belongs to,
    /// three-valued for the sidebar's repo grouping: `None` = no probe has
    /// answered yet (the caller should keep whatever grouping it had, not
    /// reshuffle on a guess); `Some(None)` = probed and confirmed outside any
    /// work tree; `Some(Some(home))` = probed and inside the repo at `home`.
    /// `home` is the repository home, not the work-tree root — a linked
    /// worktree answers with the main checkout's root, so every worktree of
    /// one repo lands in one sidebar group.
    pub fn known_repo_for(&self, host: HostId, cwd: &Path) -> Option<Option<PathBuf>> {
        let root = self.roots.get(host, cwd)?;
        Some(root.as_ref().map(|root| {
            self.homes
                .get(host, root)
                .cloned()
                .unwrap_or_else(|| root.clone())
        }))
    }

    /// Claim a probe for `cwd`. `false` means one is already in flight — the
    /// caller must *not* spawn another; the landed flight will reprobe once
    /// (the cwd is marked dirty) so this trigger's state still gets observed.
    pub fn begin_probe(&mut self, host: HostId, cwd: &Path) -> bool {
        let key = (host, cwd.to_path_buf());
        if self.probes.begin(key.clone()) {
            true
        } else {
            // Already flying: mark it superseded so the landing asks for one
            // more run rather than dropping this trigger's state.
            self.probes.invalidate(&key);
            false
        }
    }

    /// Claim an *opportunistic* probe for `cwd`: one triggered by a cheap,
    /// frequent signal — the window regaining focus, an agent finishing a tool
    /// call — rather than by a rare edge like a command ending.
    ///
    /// Unlike [`begin_probe`](Self::begin_probe) this declines instead of
    /// queueing: a probe already in flight, or one against a repo probed less
    /// than `min_interval` ago, drops the trigger entirely (no dirty mark, no
    /// rerun). That's the whole point of the two entry points — the rare edges
    /// must never be missed, while these signals repeat on their own, so a
    /// count that's a second stale beats a `git` storm across every pane of a
    /// repo the moment the user alt-tabs back.
    ///
    /// The throttle counts per *repo*, not per cwd (see
    /// [`throttle_key`](Self::throttle_key)), and the claim stamps the clock
    /// rather than waiting for the landing: without that, a dozen panes
    /// scattered over one repo's subdirectories would all claim in the same
    /// instant — each of them passing a throttle no probe had answered yet —
    /// and produce a dozen identical full-repo diffs.
    pub fn begin_probe_throttled(
        &mut self,
        host: HostId,
        cwd: &Path,
        min_interval: Duration,
    ) -> bool {
        if self.probes.is_pending(&(host, cwd.to_path_buf())) {
            return false;
        }
        let key = self.throttle_key(host, cwd).to_path_buf();
        if self
            .last_probe
            .get(host, key.as_path())
            .is_some_and(|at| at.elapsed() < min_interval)
        {
            return false;
        }
        self.last_probe.insert(host, key, Instant::now());
        self.probes.begin((host, cwd.to_path_buf()));
        true
    }

    /// What the opportunistic throttle counts against: the work-tree root once
    /// some probe has answered for `cwd`, and `cwd` itself before that.
    ///
    /// The counts a probe produces are repo-wide — `git diff --numstat HEAD`
    /// ignores which subdirectory it ran in — so panes at `repo/`, `repo/src`
    /// and `repo/docs` are three ways of asking one question, and want one
    /// shared clock rather than one each. In-flight dedup stays keyed by cwd:
    /// it brackets a specific spawn, and [`finish_probe`](Self::finish_probe)
    /// has to be able to release exactly what was claimed.
    ///
    /// Before any probe has landed the root is simply unknown, so the first
    /// sweep over a repo still costs one probe per distinct cwd; every sweep
    /// after that collapses to one.
    fn throttle_key<'a>(&'a self, host: HostId, cwd: &'a Path) -> &'a Path {
        match self.roots.get(host, cwd) {
            Some(Some(root)) => root,
            _ => cwd,
        }
    }

    /// Fold a landed probe for `cwd` into the cache. A failed diff inside a
    /// live repo keeps the root's previous counts (a transient `git` error is
    /// not "the tree went clean"). Returns whether the cwd was re-triggered
    /// while this probe flew — the caller should start one more probe.
    pub fn finish_probe(
        &mut self,
        host: HostId,
        cwd: &Path,
        snapshot: Option<RepoSnapshot>,
    ) -> bool {
        // `finish` both retires the claim and reports whether it survived: it
        // answers "still current", so the rerun this function promises is its
        // negation.
        let rerun = !self.probes.finish(&(host, cwd.to_path_buf()));
        // Re-stamp on landing so the gap is measured from fresh counts, and
        // under the root this probe just resolved — which is how a cwd first
        // learns to share its repo's clock (at claim time it had none).
        let key = match &snapshot {
            Some(snap) => snap.root.clone(),
            None => self.throttle_key(host, cwd).to_path_buf(),
        };
        self.last_probe.insert(host, key, Instant::now());
        match snapshot {
            Some(snap) => {
                let (added, removed) = snap.counts.unwrap_or_else(|| {
                    self.status
                        .get(host, &snap.root)
                        .map(|g| (g.added, g.removed))
                        .unwrap_or((0, 0))
                });
                self.status.insert(
                    host,
                    snap.root.clone(),
                    GitStatus {
                        branch: snap.branch,
                        added,
                        removed,
                    },
                );
                self.homes.insert(host, snap.root.clone(), snap.home);
                self.roots.insert(host, cwd.to_path_buf(), Some(snap.root));
            }
            // Not a repo (or the dir vanished). The root's entry stays for
            // other cwds that still live in it.
            None => {
                self.roots.insert(host, cwd.to_path_buf(), None);
            }
        }
        rerun
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// This machine — the host every pre-existing case implicitly used, back
    /// when there was only one.
    const L: HostId = HostId::LOCAL;

    fn snap(root: &str, branch: &str, counts: Option<(u32, u32)>) -> RepoSnapshot {
        RepoSnapshot {
            root: PathBuf::from(root),
            home: PathBuf::from(root),
            branch: branch.into(),
            counts,
        }
    }

    /// A snapshot for a linked worktree: its own root, a shared repo home.
    fn wt_snap(root: &str, home: &str, branch: &str) -> RepoSnapshot {
        RepoSnapshot {
            root: PathBuf::from(root),
            home: PathBuf::from(home),
            branch: branch.into(),
            counts: Some((0, 0)),
        }
    }
    /// Two cwds landing in the same work tree share one entry: a probe from
    /// either updates what both read (the group-by-root contract).
    #[test]
    fn cwds_in_one_repo_share_a_snapshot() {
        let mut cache = GitStatusCache::default();
        let (a, b) = (Path::new("/repo/sub/a"), Path::new("/repo"));
        cache.finish_probe(L, a, Some(snap("/repo", "main", Some((5, 2)))));
        cache.finish_probe(L, b, Some(snap("/repo", "main", Some((5, 2)))));
        // A later probe from `a` refreshes the numbers `b` reads too.
        cache.finish_probe(L, a, Some(snap("/repo", "main", Some((200, 42)))));
        for cwd in [a, b] {
            let got = cache.status_for(L, cwd).unwrap();
            assert_eq!((got.added, got.removed), (200, 42), "cwd {cwd:?}");
        }
    }

    /// The same absolute path on two machines is two repositories. `/src/app`
    /// exists on this laptop and on the box it is SSH'd into, on different
    /// branches with different diffs — and before the tables were keyed by
    /// host, whichever probed last would have overwritten the other's branch
    /// line. Dedup and the throttle are per host too: a probe flying for one
    /// machine must not make the other's trigger silently vanish.
    #[test]
    fn one_path_on_two_machines_is_two_entries() {
        let mut cache = GitStatusCache::default();
        let remote = HostId::from_connection_key("ssh-direct:me@box:22");
        let cwd = Path::new("/src/app");

        cache.finish_probe(L, cwd, Some(snap("/src/app", "main", Some((1, 2)))));
        cache.finish_probe(
            remote,
            cwd,
            Some(snap("/src/app", "feat/x", Some((30, 40)))),
        );

        let local = cache.status_for(L, cwd).unwrap();
        let there = cache.status_for(remote, cwd).unwrap();
        assert_eq!((local.branch.as_str(), local.added), ("main", 1));
        assert_eq!((there.branch.as_str(), there.added), ("feat/x", 30));

        // The repo *home* — what the sidebar groups by — is resolved per host
        // too. Here the same path is a plain checkout on one machine and a
        // linked worktree of a different repository on the other; a shared
        // `homes` table would have handed one machine's answer to the other.
        cache.finish_probe(
            remote,
            cwd,
            Some(wt_snap("/src/app", "/src/main", "feat/x")),
        );
        assert_eq!(
            cache.known_repo_for(L, cwd),
            Some(Some(PathBuf::from("/src/app")))
        );
        assert_eq!(
            cache.known_repo_for(remote, cwd),
            Some(Some(PathBuf::from("/src/main")))
        );

        // A probe in flight for one host leaves the other free to claim.
        assert!(cache.begin_probe(L, cwd));
        assert!(cache.begin_probe(remote, cwd));
        assert!(!cache.begin_probe(L, cwd), "same host, already flying");
        assert!(cache.finish_probe(L, cwd, None), "…so it asks for a rerun");
        assert!(!cache.finish_probe(remote, cwd, None), "the other did not");

        // …and the throttle clock is the host's own: one machine's fresh probe
        // does not silence the other's.
        let gap = Duration::from_secs(60);
        assert!(!cache.begin_probe_throttled(L, cwd, gap), "just landed");
        assert!(
            !cache.begin_probe_throttled(remote, cwd, gap),
            "just landed"
        );
        let other = Path::new("/elsewhere");
        assert!(cache.begin_probe_throttled(L, other, gap));
        assert!(cache.begin_probe_throttled(remote, other, gap));
    }

    /// A failed `git diff` (counts `None`) keeps the previous numbers rather
    /// than rendering the tree as suddenly clean; the branch still updates.
    #[test]
    fn failed_diff_keeps_previous_counts() {
        let mut cache = GitStatusCache::default();
        let cwd = Path::new("/repo");
        cache.finish_probe(L, cwd, Some(snap("/repo", "main", Some((200, 42)))));
        cache.finish_probe(L, cwd, Some(snap("/repo", "feat/x", None)));
        let got = cache.status_for(L, cwd).unwrap();
        assert_eq!(got.branch, "feat/x");
        assert_eq!((got.added, got.removed), (200, 42));
    }

    /// In-flight dedup: a second trigger while a probe flies doesn't claim a
    /// new one, but marks the cwd dirty so the landing reports "go again".
    #[test]
    fn concurrent_triggers_fold_into_one_probe_then_rerun() {
        let mut cache = GitStatusCache::default();
        let cwd = Path::new("/repo");
        assert!(cache.begin_probe(L, cwd));
        assert!(!cache.begin_probe(L, cwd)); // deduped, marked dirty
        assert!(cache.finish_probe(L, cwd, Some(snap("/repo", "main", Some((1, 0))))));
        // The rerun claims cleanly and lands with nothing pending.
        assert!(cache.begin_probe(L, cwd));
        assert!(!cache.finish_probe(L, cwd, Some(snap("/repo", "main", Some((1, 0))))));
    }

    /// A cwd that leaves the repo (dir deleted / not a work tree) stops
    /// answering, without disturbing the root entry other cwds still use.
    #[test]
    fn non_repo_cwd_clears_only_itself() {
        let mut cache = GitStatusCache::default();
        let (a, b) = (Path::new("/repo/a"), Path::new("/repo/b"));
        cache.finish_probe(L, a, Some(snap("/repo", "main", Some((3, 1)))));
        cache.finish_probe(L, b, Some(snap("/repo", "main", Some((3, 1)))));
        cache.finish_probe(L, a, None);
        assert_eq!(cache.status_for(L, a), None);
        assert!(cache.status_for(L, b).is_some());
    }

    /// The three-valued `known_repo_for` the sidebar's repo grouping reads:
    /// unprobed → `None`, probed-and-in-a-repo → `Some(Some(home))`,
    /// probed-and-not-a-repo → `Some(None)`. The three cases are what let a
    /// sticky group key hold across an in-flight cd instead of flickering.
    #[test]
    fn known_repo_for_is_three_valued() {
        let mut cache = GitStatusCache::default();
        let (repo, plain, unseen) = (
            Path::new("/repo/a"),
            Path::new("/tmp/x"),
            Path::new("/never"),
        );
        cache.finish_probe(L, repo, Some(snap("/repo", "main", Some((1, 0)))));
        cache.finish_probe(L, plain, None);

        // Inside a work tree: the resolved repo home, wrapped twice.
        assert_eq!(
            cache.known_repo_for(L, repo),
            Some(Some(PathBuf::from("/repo")))
        );
        // Probed and confirmed outside any repo: a definite "not a repo".
        assert_eq!(cache.known_repo_for(L, plain), Some(None));
        // Never probed: no answer yet — the caller keeps its sticky key.
        assert_eq!(cache.known_repo_for(L, unseen), None);
    }

    /// Linked worktrees of one repository share a *group* (`known_repo_for`
    /// answers the main root for both) while their *status* stays per work
    /// tree — different branches never clobber each other.
    #[test]
    fn worktrees_share_a_repo_but_not_a_status() {
        let mut cache = GitStatusCache::default();
        let (main, wt) = (Path::new("/repo"), Path::new("/repo/.wt/feat"));
        cache.finish_probe(L, main, Some(wt_snap("/repo", "/repo", "main")));
        cache.finish_probe(L, wt, Some(wt_snap("/repo/.wt/feat", "/repo", "feat/x")));

        // One sidebar group…
        assert_eq!(
            cache.known_repo_for(L, main),
            Some(Some(PathBuf::from("/repo")))
        );
        assert_eq!(
            cache.known_repo_for(L, wt),
            Some(Some(PathBuf::from("/repo")))
        );
        // …two independent branch lines.
        assert_eq!(cache.status_for(L, main).unwrap().branch, "main");
        assert_eq!(cache.status_for(L, wt).unwrap().branch, "feat/x");
    }
    /// The opportunistic path declines where the edge path queues: an in-flight
    /// probe drops the trigger (and leaves nothing dirty, so no rerun), and a
    /// probe that just landed rate-limits the next one.
    #[test]
    fn throttled_probes_decline_instead_of_queueing() {
        let mut cache = GitStatusCache::default();
        let cwd = Path::new("/repo");
        let gap = Duration::from_secs(60);

        assert!(cache.begin_probe_throttled(L, cwd, gap));
        // In flight: declined, and unlike `begin_probe` it doesn't mark dirty —
        // the landing reports "nothing pending" rather than asking for a rerun.
        assert!(!cache.begin_probe_throttled(L, cwd, gap));
        assert!(!cache.finish_probe(L, cwd, Some(snap("/repo", "main", Some((1, 0))))));

        // Landed just now: still inside the gap, so the next trigger is dropped.
        assert!(!cache.begin_probe_throttled(L, cwd, gap));
        // …but a zero gap always lets one through, and edge triggers never
        // consult the throttle at all.
        assert!(cache.begin_probe_throttled(L, cwd, Duration::ZERO));
        assert!(!cache.finish_probe(L, cwd, Some(snap("/repo", "main", Some((1, 0))))));
        assert!(cache.begin_probe(L, cwd));
    }

    /// The throttle is per repo, not per cwd: panes sitting in different
    /// subdirectories ask one question (the counts are repo-wide), so once the
    /// cache knows where they live, a window activation costs one probe for
    /// the repo rather than one per pane.
    #[test]
    fn throttle_collapses_subdirectories_of_one_repo() {
        let mut cache = GitStatusCache::default();
        let (top, src, docs) = (
            Path::new("/repo"),
            Path::new("/repo/src"),
            Path::new("/repo/docs"),
        );
        let gap = Duration::from_secs(60);

        // Nothing known yet, so each cwd is its own key and each gets a probe.
        for cwd in [top, src, docs] {
            assert!(cache.begin_probe_throttled(L, cwd, gap));
            assert!(!cache.finish_probe(L, cwd, Some(snap("/repo", "main", Some((3, 1))))));
        }

        // Now all three resolve to `/repo`, so the next sweep collapses: the
        // first pane to ask spends the probe and the rest ride on it.
        assert!(!cache.begin_probe_throttled(L, top, gap));
        assert!(!cache.begin_probe_throttled(L, src, gap));

        // …and the claim itself is what stops the stampede — with the clock
        // wound back far enough to let one through, the *others* still decline
        // while it is in flight, even though nothing has landed yet.
        assert!(cache.begin_probe_throttled(L, docs, Duration::ZERO));
        assert!(!cache.begin_probe_throttled(L, top, gap));
        assert!(!cache.begin_probe_throttled(L, src, gap));

        // A pane elsewhere is untouched by any of it.
        let other = Path::new("/other");
        assert!(cache.begin_probe_throttled(L, other, gap));
    }
}
