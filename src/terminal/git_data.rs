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

// The watcher and the subscription gate now use this module, but the panel and
// the file tree — the things that read the status and run the writes — are
// still landing alongside it, so `status_of`, `index_of`, `run_git_op`,
// `shell_quote` and the `FileTree`/`Editor` subscribers have no callers yet.
// Take the allow off with the last of them; anything still unused then is.
#![allow(dead_code)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use gpui::{Context, Window};

use crate::core::git::ops::{GitOp, GitOpError, GitOpErrorKind, GitOpOutcome, run_op};
use crate::core::git::status::{StatusIndex, WorkingTreeStatus, probe_status};
use crate::ui::app::Tty7App;
use crate::ui::host_ops::{ByHost, Host, HostId, HostOps, InFlight, SharedHost, WatchSub};

/// How many network operations one host may have in flight.
///
/// The far side serves every request from one worker pool, and keepalive's
/// `Ping` queues behind the rest of it. Enough concurrent pushes and the ping
/// misses its own deadline for long enough that the link is declared dead —
/// so the client, not the server, keeps the number small.
pub const MAX_CONCURRENT_NETWORK_OPS: usize = 2;

/// How quiet a burst of invalidations has to go before we believe it is over.
pub const GIT_WATCH_DEBOUNCE: Duration = Duration::from_millis(250);

/// …and how long we are willing to keep waiting for that quiet. A checkout
/// rewrites `.git` continuously for longer than the debounce window, and
/// showing nothing for the whole of it reads as a hang.
pub const GIT_WATCH_MAX_DELAY: Duration = Duration::from_millis(1000);

/// How many namespace directories under `refs/heads` we will watch.
///
/// `Host::watch` is non-recursive, so `refs/heads/feat/x` is only seen if
/// `refs/heads/feat` is listed by name. Past this many namespaces we list none
/// of them: `packed-refs` and `<git_dir>` itself catch nearly every branch
/// operation anyway, and the command boundary catches the rest.
pub const MAX_WATCHED_REF_DIRS: usize = 64;

/// The directories a repository's `.git` needs watched, in the order they are
/// passed to [`Host::watch`].
///
/// A pure function of the three answers `rev-parse` gives, so the layout rules
/// can be tested without a repository:
///
/// - `<git_dir>` carries `index`, `HEAD`, `ORIG_HEAD`, `MERGE_HEAD`,
///   `CHERRY_PICK_HEAD`. It is the one that matters most; nearly every verb
///   touches something in it.
/// - `<common_dir>` is where `packed-refs` lives, and in a linked worktree it
///   is *not* `<git_dir>` — that is the whole reason both are asked for.
/// - the three `refs/` directories, plus the namespaces below `refs/heads`,
///   because the watch does not recurse.
///
/// Deliberately absent: the working tree. Recursively watching a working tree
/// over SSH is a disaster, and edits there already arrive on the same
/// invalidation bus from the file tree, the editor and the command boundary.
pub fn git_watch_dirs(
    sep: char,
    git_dir: &Path,
    common_dir: &Path,
    head_namespaces: &[String],
) -> Vec<PathBuf> {
    use tty7_core::host::default_join;

    let mut dirs = vec![git_dir.to_path_buf()];
    if common_dir != git_dir {
        dirs.push(common_dir.to_path_buf());
    }
    let refs = default_join(common_dir, "refs", sep);
    let heads = default_join(&refs, "heads", sep);
    dirs.push(heads.clone());
    dirs.push(default_join(&refs, "remotes", sep));
    dirs.push(default_join(&refs, "tags", sep));
    if head_namespaces.len() <= MAX_WATCHED_REF_DIRS {
        dirs.extend(
            head_namespaces
                .iter()
                .map(|name| default_join(&heads, name, sep)),
        );
    }
    dirs
}

/// What a debounced repository wants next.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DebounceStep {
    /// No burst is open; the timer that asked should stop.
    Idle,
    /// Sleep this long and ask again.
    Wait(Duration),
    /// Probe now.
    Fire,
}

/// Collapses a burst of invalidations into one probe.
///
/// Two clocks, because either alone is wrong: the quiet period alone is
/// starved by an operation that writes `.git` continuously (a checkout, a
/// rebase), and the ceiling alone fires in the middle of a burst that was
/// about to end.
///
/// There is no self-triggering to defend against here, and the reason is
/// structural rather than lucky: every read goes through `core::git`'s
/// `git_output`, which sets `GIT_OPTIONAL_LOCKS=0`. That variable's only
/// effect is to stop `git status` refreshing and writing back `.git/index`, so
/// the probes this schedules provably cannot wake the watcher that schedules
/// them. Writes *do* wake it, and that is wanted — `run_git_op` announces
/// itself on the same bus, so the write's own notification and the watcher
/// event a few milliseconds behind it land in one window and cost one probe.
#[derive(Default)]
pub struct Debounce {
    /// When the open burst started, if one is open.
    opened: Option<Instant>,
    /// The newest event in the open burst.
    latest: Option<Instant>,
    /// Bumped when a burst opens, so the timer left over from an older burst
    /// wakes, finds it is not the one, and stops.
    seq: u64,
}

impl Debounce {
    /// Record an event. `Some(seq)` means this opened a burst and the caller
    /// owes it a timer; `None` means one is already running.
    pub fn note(&mut self, now: Instant) -> Option<u64> {
        self.latest = Some(now);
        if self.opened.is_some() {
            return None;
        }
        self.opened = Some(now);
        self.seq += 1;
        Some(self.seq)
    }

    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// What the timer should do now. `Fire` closes the burst, so it is
    /// returned exactly once however many events went into it.
    pub fn poll(&mut self, now: Instant) -> DebounceStep {
        let (Some(opened), Some(latest)) = (self.opened, self.latest) else {
            return DebounceStep::Idle;
        };
        let deadline = (latest + GIT_WATCH_DEBOUNCE).min(opened + GIT_WATCH_MAX_DELAY);
        match deadline.checked_duration_since(now) {
            Some(left) if !left.is_zero() => DebounceStep::Wait(left),
            _ => {
                self.opened = None;
                self.latest = None;
                DebounceStep::Fire
            }
        }
    }
}

/// A claim on one of a host's network slots, released by dropping it.
///
/// The count has to come back even when nothing lands: `HostOps::run_in` skips
/// its landing closure if the view died first, and a slot released only there
/// leaks for the lifetime of the process. So the guard rides in the *work*
/// closure instead, which the blocking pool runs either way, and the counter
/// is an atomic rather than a field of the global so that dropping it needs no
/// `App` at all.
pub struct NetworkSlot(Arc<AtomicUsize>);

impl Drop for NetworkSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Release);
    }
}

/// Who is holding a repository open.
///
/// An enum rather than an opaque token because every one of these decides
/// what it wants by looking at the frame it is rendering, not by remembering
/// that it asked. Declaring "this is what I want now" is idempotent; calling
/// `acquire` from `render` would count up forever.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ScmWatcher {
    /// The source control panel, while it is the visible tab.
    Panel,
    /// The file tree, while it is showing a directory inside the repository.
    FileTree,
    /// The code editor, while it has a file from the repository open.
    Editor,
}

/// Who is watching which repository, and a generation to date results by.
///
/// The point is the zero: with nobody looking, a repository must cost nothing
/// — no watch, no timer, no `status -uall` in the background. Anything already
/// in flight when the last holder leaves is not cancellable, so it is dated
/// instead: the generation moves and the result is dropped on arrival.
#[derive(Default)]
pub struct GitSubscriptions {
    refs: ByHost<PathBuf, u32>,
    sub_gen: ByHost<PathBuf, u64>,
    held: HashMap<ScmWatcher, (HostId, PathBuf)>,
}

impl GitSubscriptions {
    /// Take a hold, and report the generation results must carry to be kept.
    pub fn acquire(&mut self, host: HostId, root: &Path) -> u64 {
        let next = self.count(host, root) + 1;
        self.refs.insert(host, root.to_path_buf(), next);
        self.generation(host, root)
    }

    /// Give one back. The last one out invalidates everything in flight.
    pub fn release(&mut self, host: HostId, root: &Path) {
        let left = self.count(host, root).saturating_sub(1);
        if left > 0 {
            self.refs.insert(host, root.to_path_buf(), left);
            return;
        }
        self.refs.remove(host, root);
        let next = self.generation(host, root) + 1;
        self.sub_gen.insert(host, root.to_path_buf(), next);
    }

    pub fn generation(&self, host: HostId, root: &Path) -> u64 {
        self.sub_gen.get(host, root).copied().unwrap_or(0)
    }

    pub fn count(&self, host: HostId, root: &Path) -> u32 {
        self.refs.get(host, root).copied().unwrap_or(0)
    }

    pub fn is_subscribed(&self, host: HostId, root: &Path) -> bool {
        self.count(host, root) > 0
    }

    /// State what `who` wants right now — safe to call every frame.
    ///
    /// Returns the repository that just lost its last holder, if any, so the
    /// caller can tear down what was keeping it alive.
    pub fn declare(
        &mut self,
        who: ScmWatcher,
        target: Option<(HostId, PathBuf)>,
    ) -> Option<(HostId, PathBuf)> {
        let previous = self.held.get(&who).cloned();
        if previous == target {
            return None;
        }
        match &target {
            Some((host, root)) => {
                self.acquire(*host, root);
                self.held.insert(who, (*host, root.clone()));
            }
            None => {
                self.held.remove(&who);
            }
        }
        let (host, root) = previous?;
        self.release(host, &root);
        (!self.is_subscribed(host, &root)).then_some((host, root))
    }

    fn clear_host(&mut self, host: HostId) {
        self.refs.clear_host(host);
        self.sub_gen.clear_host(host);
        self.held.retain(|_, (held, _)| *held != host);
    }

    fn is_empty(&self) -> bool {
        self.refs.is_empty() && self.held.is_empty()
    }

    fn subscribed(&self) -> Vec<(HostId, PathBuf)> {
        self.refs
            .keys()
            .map(|(host, root)| (host, root.clone()))
            .collect()
    }
}

/// One repository's `.git` watch and the burst it is feeding.
#[derive(Default)]
struct RepoWatch {
    sub: Option<Arc<WatchSub>>,
    /// A watch is two round trips to open (`rev-parse`, then `watch`), so the
    /// frame after the one that asked must not ask again.
    opening: bool,
    debounce: Debounce,
}

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
    network: ByHost<PathBuf, Arc<AtomicUsize>>,
    /// repo root → its `.git` watch, once someone is looking. A plain map
    /// rather than a [`ByHost`] because this one is mutated in place.
    watches: HashMap<(HostId, PathBuf), RepoWatch>,
    subs: GitSubscriptions,
    /// Bumped by [`ScmData::clear_host`]. A probe that was in flight across a
    /// disconnect is holding a picture of a machine we have stopped believing,
    /// and neither the epoch nor the generation can say so: the disconnect
    /// wipes both back to their defaults, which is exactly what the in-flight
    /// read recorded on the way out.
    wipe: u64,
}

impl gpui::Global for ScmData {}

impl ScmData {
    pub fn status_for(&self, host: HostId, root: &Path) -> Option<Arc<WorkingTreeStatus>> {
        self.status.get(host, root).cloned()
    }

    pub fn index_for(&self, host: HostId, root: &Path) -> Option<Arc<StatusIndex>> {
        self.index.get(host, root).cloned()
    }

    /// Three-valued, and the middle value is the one that matters: `None` for
    /// "never looked", `Some(None)` for "looked, and there is no repository
    /// there". A directory that is not a repository is a perfectly normal
    /// thing for a pane to be sitting in, and without somewhere to record the
    /// negative answer the next look asks again — every frame.
    pub fn known_status(
        &self,
        host: HostId,
        root: &Path,
    ) -> Option<Option<Arc<WorkingTreeStatus>>> {
        self.read_at.get(host, root)?;
        Some(self.status_for(host, root))
    }

    /// Whether this root has been read at all, whatever the answer was.
    pub fn probed(&self, host: HostId, root: &Path) -> bool {
        self.read_at.get(host, root).is_some()
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
        self.watches.retain(|(held, _), _| *held != host);
        self.subs.clear_host(host);
        self.wipe += 1;
    }

    /// Which hosts we are holding anything for. Used to notice the ones that
    /// have since disappeared from the registry.
    pub fn hosts(&self) -> Vec<HostId> {
        let mut hosts: Vec<HostId> = self.epoch.keys().map(|(host, _)| host).collect();
        hosts.sort_unstable();
        hosts.dedup();
        hosts
    }

    /// Claim one of a host's network slots, or `None` when it is already at
    /// the ceiling. Drop the returned guard to give it back.
    pub fn take_network_slot(&mut self, host: HostId, root: &Path) -> Option<NetworkSlot> {
        let counter = match self.network.get(host, root) {
            Some(counter) => Arc::clone(counter),
            None => {
                let counter = Arc::new(AtomicUsize::new(0));
                self.network
                    .insert(host, root.to_path_buf(), Arc::clone(&counter));
                counter
            }
        };
        // Only the UI thread hands these out, so read-then-add cannot race
        // another claim; the release side is the one that runs anywhere.
        if counter.load(Ordering::Acquire) >= MAX_CONCURRENT_NETWORK_OPS {
            return None;
        }
        counter.fetch_add(1, Ordering::AcqRel);
        Some(NetworkSlot(counter))
    }

    pub fn network_slots(&self, host: HostId, root: &Path) -> usize {
        self.network
            .get(host, root)
            .map(|c| c.load(Ordering::Acquire))
            .unwrap_or(0)
    }

    pub fn subscriptions(&mut self) -> &mut GitSubscriptions {
        &mut self.subs
    }

    pub fn generation(&self, host: HostId, root: &Path) -> u64 {
        self.subs.generation(host, root)
    }

    pub fn is_subscribed(&self, host: HostId, root: &Path) -> bool {
        self.subs.is_subscribed(host, root)
    }

    /// Nobody is holding a repository open and nothing is watching one.
    pub fn is_quiet(&self) -> bool {
        self.subs.is_empty() && self.watches.is_empty()
    }

    /// Repositories that have a holder but no watch, and nothing on the way.
    fn unwatched(&self) -> Vec<(HostId, PathBuf)> {
        self.subs
            .subscribed()
            .into_iter()
            .filter(|key| match self.watches.get(key) {
                Some(watch) => watch.sub.is_none() && !watch.opening,
                None => true,
            })
            .collect()
    }

    fn watch_mut(&mut self, host: HostId, root: &Path) -> &mut RepoWatch {
        self.watches.entry((host, root.to_path_buf())).or_default()
    }

    /// Say a watch is being opened, unless one already is or already exists.
    fn begin_watch_open(&mut self, host: HostId, root: &Path) -> bool {
        let watch = self.watch_mut(host, root);
        if watch.opening || watch.sub.is_some() {
            return false;
        }
        watch.opening = true;
        true
    }

    fn finish_watch_open(&mut self, host: HostId, root: &Path, sub: Option<Arc<WatchSub>>) {
        let watch = self.watch_mut(host, root);
        watch.opening = false;
        watch.sub = sub;
    }

    /// Stop everything a repository was costing: the watch closes when the
    /// last `Arc` goes, and the burst is cleared so a timer still asleep on it
    /// wakes to `Idle`.
    fn drop_watch(&mut self, host: HostId, root: &Path) {
        self.watches.remove(&(host, root.to_path_buf()));
    }

    /// Record an invalidation. `Some(seq)` means a burst opened and the caller
    /// owes it a timer.
    pub fn note_change(&mut self, host: HostId, root: &Path, now: Instant) -> Option<u64> {
        self.watch_mut(host, root).debounce.note(now)
    }

    pub fn poll_debounce(&mut self, host: HostId, root: &Path, now: Instant) -> DebounceStep {
        match self.watches.get_mut(&(host, root.to_path_buf())) {
            Some(watch) => watch.debounce.poll(now),
            None => DebounceStep::Idle,
        }
    }

    pub fn burst_seq(&self, host: HostId, root: &Path) -> u64 {
        self.watches
            .get(&(host, root.to_path_buf()))
            .map(|watch| watch.debounce.seq())
            .unwrap_or(0)
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
        let sub_gen = data.generation(id, &root);
        let wipe = data.wipe;

        let probe_root = root.clone();
        let this = cx.weak_entity();
        let again = host.clone();
        HostOps::run_detached(
            host,
            cx,
            move |h| {
                let status = probe_status(h, &probe_root)?;
                let index = StatusIndex::build(&status);
                Some((Arc::new(status), Arc::new(index)))
            },
            move |cx, result| {
                let data = cx.default_global::<ScmData>();
                // `finish` says whether the epoch held still while this was in
                // flight. It usually did; when it did not, the result is still
                // worth showing (stale beats blank) but something has to ask
                // again, and nothing else will — the trigger that bumped found
                // this probe already running and declined to start its own.
                let superseded = !data.probes.finish(&key);
                if data.wipe != wipe || data.generation(id, &root) != sub_gen {
                    return;
                }
                let changed = match result {
                    Some((status, index)) => {
                        let same = data
                            .status
                            .get(id, root.as_path())
                            .is_some_and(|held| **held == *status);
                        data.status.insert(id, root.clone(), status);
                        data.index.insert(id, root.clone(), index);
                        !same
                    }
                    // Not a repository — a perfectly ordinary answer, and one
                    // that has to be recorded like any other. `read_at` below
                    // is what stops the next frame asking again: a pane whose
                    // cwd is an ordinary directory would otherwise spawn a
                    // `rev-parse` per frame, forever.
                    None => {
                        let held = data.status.remove(id, root.as_path()).is_some();
                        data.index.remove(id, root.as_path());
                        held
                    }
                };
                data.read_at.insert(id, root.clone(), at);
                // `run_detached` lands with an `App` and no view, and writing
                // a global marks nothing dirty, so without this the panel and
                // the decorations wait for the next unrelated repaint.
                //
                // Only when something actually moved, though. Most probes
                // confirm what is already on screen — a re-read after a write
                // that touched another repository, or the "still not a
                // repository" answer for an ordinary directory — and repainting
                // for those would put a frame behind every file the tree
                // notices changing.
                if changed {
                    cx.refresh_windows();
                }
                if superseded {
                    let _ = this.update(cx, |app, cx| app.scm_refresh(again, root, cx));
                }
            },
        );
    }

    /// The one way to say "this repository changed".
    ///
    /// Every source lands here — the `.git` watch, a write we just made, the
    /// file tree, the editor, a command boundary — so that a `git add` and the
    /// watcher event it provokes cost one probe between them rather than two.
    ///
    /// With nobody subscribed the epoch bump is the whole job: the repository
    /// is now marked stale and whoever opens the panel next pays for one read,
    /// which is much better than running `status -uall` for an empty room.
    pub(crate) fn scm_invalidate(&mut self, host: HostId, root: &Path, cx: &mut Context<Self>) {
        let data = cx.default_global::<ScmData>();
        data.bump(host, root);
        if !data.is_subscribed(host, root) {
            return;
        }
        let Some(seq) = data.note_change(host, root, Instant::now()) else {
            return;
        };
        let sub_gen = data.generation(host, root);
        self.scm_debounce(host, root.to_path_buf(), seq, sub_gen, cx);
    }

    /// As [`Tty7App::scm_invalidate`], for a caller that knows a directory
    /// rather than a repository — which is every caller outside this module.
    pub(crate) fn scm_invalidate_cwd(&mut self, host: HostId, cwd: &Path, cx: &mut Context<Self>) {
        let Some(root) = cx
            .try_global::<crate::terminal::git_status::GitStatusCache>()
            .and_then(|cache| cache.repo_root_for(host, cwd))
            .map(Path::to_path_buf)
        else {
            return;
        };
        self.scm_invalidate(host, &root, cx);
    }

    /// Sit on the burst until it goes quiet, then probe once.
    fn scm_debounce(
        &mut self,
        host: HostId,
        root: PathBuf,
        seq: u64,
        sub_gen: u64,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |this, cx| {
            loop {
                let step = this
                    .update(cx, |_app, cx| {
                        let data = cx.default_global::<ScmData>();
                        // Two ways to be the wrong timer: the burst we were
                        // started for already fired and a newer one opened, or
                        // the last holder let go while we slept.
                        if data.burst_seq(host, &root) != seq
                            || data.generation(host, &root) != sub_gen
                        {
                            return DebounceStep::Idle;
                        }
                        data.poll_debounce(host, &root, Instant::now())
                    })
                    .unwrap_or(DebounceStep::Idle);
                match step {
                    DebounceStep::Idle => return,
                    DebounceStep::Wait(left) => cx.background_executor().timer(left).await,
                    DebounceStep::Fire => break,
                }
            }
            let _ = this.update(cx, |app, cx| {
                if cx.default_global::<ScmData>().generation(host, &root) != sub_gen {
                    return;
                }
                let Some(shared) = crate::ui::host_registry::HostRegistry::get(cx, host) else {
                    return;
                };
                app.scm_refresh(shared, root, cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// Reconcile who is watching what. Called once per frame.
    ///
    /// Everything here is a property of the frame — is the panel the visible
    /// tab, which repository is the active pane in, is that host still up — so
    /// it is stated rather than remembered, and running it twice costs nothing.
    pub(crate) fn scm_sync_watchers(&mut self, window: &Window, cx: &mut Context<Self>) {
        self.scm_forget_lost_hosts(cx);

        let wanted = [
            (ScmWatcher::Panel, self.scm_panel_target(window, cx)),
            (ScmWatcher::FileTree, self.scm_tree_target(window, cx)),
            (ScmWatcher::Editor, self.scm_editor_target(cx)),
        ];
        // Nothing to watch and nothing being watched, which is every frame of
        // a window whose panel is on another tab. Taking the global mutably
        // here would queue a global-observer effect per frame for no reason.
        if wanted.iter().all(|(_, t)| t.is_none())
            && cx.try_global::<ScmData>().is_none_or(ScmData::is_quiet)
        {
            return;
        }
        for (who, target) in wanted {
            let dropped = cx
                .default_global::<ScmData>()
                .subscriptions()
                .declare(who, target);
            if let Some((host, root)) = dropped {
                cx.default_global::<ScmData>().drop_watch(host, &root);
            }
        }

        for (host, root) in cx.default_global::<ScmData>().unwatched() {
            let Some(shared) = crate::ui::host_registry::HostRegistry::get(cx, host) else {
                continue;
            };
            self.scm_open_watch(shared.clone(), root.clone(), cx);
            self.scm_refresh(shared, root, cx);
        }
    }

    /// The repository the panel is showing, while it is showing one.
    ///
    /// Prefers whatever the panel itself settled on; falls back to the repo
    /// the active pane is sitting in, which is what the panel will pick anyway
    /// and is already known from the cheap tab-badge probe.
    fn scm_panel_target(&self, window: &Window, cx: &gpui::App) -> Option<(HostId, PathBuf)> {
        use crate::core::config::RightPanelTab;

        if !self.right_panel_visible || self.right_panel_tab != RightPanelTab::Scm {
            return None;
        }
        if let Some(repo) = self.scm.active_repo() {
            return Some((repo.host, repo.root.clone()));
        }
        let leaf = self.tabs.get(self.active)?.detail_pane(window, cx)?;
        let view = leaf.read(cx);
        let host = view.host_id();
        let root = cx
            .try_global::<crate::terminal::git_status::GitStatusCache>()?
            .repo_root_for(host, view.git_status_cwd()?)?;
        Some((host, root.to_path_buf()))
    }

    /// The repository the file tree is decorating, while it is on screen.
    ///
    /// The tree can be rooted at several repositories at once but a watcher
    /// holds one; the active pane's is the one whose decorations the user is
    /// looking at, and it is the one the panel would pick too — so the two
    /// subscriptions usually collapse onto the same repository and cost one
    /// watch between them.
    fn scm_tree_target(&self, window: &Window, cx: &gpui::App) -> Option<(HostId, PathBuf)> {
        if !self.file_tree_on_screen(cx) {
            return None;
        }
        let leaf = self.tabs.get(self.active)?.detail_pane(window, cx)?;
        let view = leaf.read(cx);
        let host = view.host_id();
        let root = cx
            .try_global::<crate::terminal::git_status::GitStatusCache>()?
            .repo_root_for(host, view.git_status_cwd()?)?;
        Some((host, root.to_path_buf()))
    }

    /// The repository the focused editor's file belongs to.
    ///
    /// Only the focused one: an editor on a background tab is not showing
    /// anyone a gutter, and holding a watch per open file would put a `status`
    /// probe behind every repository the user has visited this session.
    fn scm_editor_target(&self, cx: &gpui::App) -> Option<(HostId, PathBuf)> {
        let code = self.tabs.get(self.active)?.code.as_ref()?;
        if !code.visible {
            return None;
        }
        let open = code.active_file()?;
        let host = self.spawn_host(cx);
        let root = cx
            .try_global::<crate::terminal::git_status::GitStatusCache>()?
            .repo_root_for(host, open.path.parent()?)?;
        Some((host, root.to_path_buf()))
    }

    /// Forget hosts that have left the registry.
    ///
    /// A dropped SSH link is the case that matters: without this, the panel
    /// keeps showing the branch and the file list the machine had at the
    /// moment it fell off, with no way to tell that from live data.
    fn scm_forget_lost_hosts(&mut self, cx: &mut Context<Self>) {
        let held = match cx.try_global::<ScmData>() {
            Some(data) => data.hosts(),
            None => return,
        };
        if held.is_empty() {
            return;
        }
        let live = crate::ui::host_registry::HostRegistry::ids(cx);
        for host in held.into_iter().filter(|h| !live.contains(h)) {
            cx.default_global::<ScmData>().clear_host(host);
            cx.default_global::<crate::terminal::git_status::GitStatusCache>()
                .clear_host(host);
        }
    }

    /// Open a `.git` watch: resolve the directories, then subscribe to them.
    ///
    /// Both halves are one background job because both are round trips on a
    /// remote workspace, and the directory list is only wanted in order to
    /// pass it straight to `watch`.
    fn scm_open_watch(&mut self, host: SharedHost, root: PathBuf, cx: &mut Context<Self>) {
        let id = host.id();
        if !cx.default_global::<ScmData>().begin_watch_open(id, &root) {
            return;
        }
        let sub_gen = cx.default_global::<ScmData>().generation(id, &root);
        let probe_root = root.clone();
        HostOps::run(
            host,
            cx,
            move |h| {
                let dirs = scm_watch_dirs(h, &probe_root)?;
                match h.watch(&dirs) {
                    Ok(sub) => Some(Arc::new(sub)),
                    Err(e) => {
                        log::warn!("source control: no watch for {probe_root:?}: {e}");
                        None
                    }
                }
            },
            move |app, sub: Option<Arc<WatchSub>>, cx| {
                app.scm_watch_opened(id, root, sub_gen, sub, cx)
            },
        );
    }

    fn scm_watch_opened(
        &mut self,
        host: HostId,
        root: PathBuf,
        sub_gen: u64,
        sub: Option<Arc<WatchSub>>,
        cx: &mut Context<Self>,
    ) {
        let data = cx.default_global::<ScmData>();
        // Letting go while the watch was opening leaves the only `Arc` here,
        // so returning closes it.
        if data.generation(host, &root) != sub_gen || !data.is_subscribed(host, &root) {
            data.finish_watch_open(host, &root, None);
            return;
        }
        let events = sub.as_ref().map(|sub| sub.events().clone());
        data.finish_watch_open(host, &root, sub);
        let Some(events) = events else {
            return;
        };
        cx.spawn(async move |app, cx| {
            // The batch is not read. Anything at all under `.git` means the
            // answer to "what does this repository look like" may have moved,
            // and working out which paths imply which parts of the answer
            // would be a second, worse copy of what `git status` already does.
            while events.recv().await.is_ok() {
                if app
                    .update(cx, |app, cx| app.scm_invalidate(host, &root, cx))
                    .is_err()
                {
                    return;
                }
            }
        })
        .detach();
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

        // The guard rides into the work closure rather than being released in
        // the landing one: `run_in` skips landing entirely if the view died
        // first, and a slot released only there is a slot lost for good.
        let slot = if op.is_network() {
            match cx.default_global::<ScmData>().take_network_slot(id, &root) {
                Some(slot) => Some(slot),
                None => return,
            }
        } else {
            None
        };

        let op_root = root.clone();
        HostOps::run_in(
            host,
            window,
            cx,
            move |h| {
                let outcome = run_op(h, &op_root, &op, &head);
                drop(slot);
                outcome
            },
            move |app, result, window, cx| {
                // Onto the same bus as everything else: the watcher event this
                // write is about to cause arrives inside the debounce window
                // and the two of them cost one probe.
                app.scm_invalidate(id, &root, cx);
                app.on_git_op_done(result, window, cx);
            },
        );
    }

    fn on_git_op_done(
        &mut self,
        result: Result<GitOpOutcome, GitOpError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Err(err) = result {
            self.report_git_op_error(&err, window, cx);
        }
        cx.notify();
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

/// Ask a host where a repository's `.git` is, and what to watch inside it.
///
/// Two round trips: `rev-parse` for the two directories, then one `read_dir`
/// for the namespaces under `refs/heads`. The list is resolved once, when the
/// watch opens, and not maintained afterwards — a branch created in a
/// namespace nobody had yet still writes `<git_dir>` and eventually
/// `packed-refs`, both of which are watched, and the panel re-resolves the
/// whole set the next time it is opened.
fn scm_watch_dirs(host: &dyn Host, root: &Path) -> Option<Vec<PathBuf>> {
    let out = host
        .git(
            root,
            &[
                "rev-parse",
                "--path-format=absolute",
                "--git-dir",
                "--git-common-dir",
            ],
        )
        .ok()?;
    if !out.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut lines = text.lines().map(|l| l.trim_end_matches(['\n', '\r']));
    let git_dir = PathBuf::from(lines.next().filter(|l| !l.is_empty())?);
    let common_dir = lines
        .next()
        .filter(|l| !l.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| git_dir.clone());

    let sep = host.separator();
    let heads = tty7_core::host::default_join(
        &tty7_core::host::default_join(&common_dir, "refs", sep),
        "heads",
        sep,
    );
    let namespaces: Vec<String> = host
        .read_dir(&heads, None)
        .unwrap_or_default()
        .into_iter()
        .filter(|entry| entry.is_dir)
        .map(|entry| entry.name)
        .collect();
    Some(git_watch_dirs(sep, &git_dir, &common_dir, &namespaces))
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

    // -- what to watch ----------------------------------------------------

    fn names(dirs: &[PathBuf]) -> Vec<String> {
        dirs.iter().map(|d| d.display().to_string()).collect()
    }

    #[test]
    fn a_plain_repository_watches_its_git_dir_and_the_three_ref_roots() {
        let git_dir = PathBuf::from("/repo/.git");
        let dirs = git_watch_dirs('/', &git_dir, &git_dir, &[]);
        assert_eq!(
            names(&dirs),
            [
                "/repo/.git",
                "/repo/.git/refs/heads",
                "/repo/.git/refs/remotes",
                "/repo/.git/refs/tags",
            ],
            "the common dir is the git dir here, so it must not be listed twice"
        );
    }

    #[test]
    fn a_linked_worktree_watches_both_of_its_directories() {
        // HEAD and index live under the worktree's own git dir; packed-refs
        // and every branch live in the one it borrows.
        let git_dir = PathBuf::from("/repo/.git/worktrees/feat");
        let common = PathBuf::from("/repo/.git");
        let dirs = git_watch_dirs('/', &git_dir, &common, &["feat".into()]);
        assert_eq!(
            names(&dirs),
            [
                "/repo/.git/worktrees/feat",
                "/repo/.git",
                "/repo/.git/refs/heads",
                "/repo/.git/refs/remotes",
                "/repo/.git/refs/tags",
                "/repo/.git/refs/heads/feat",
            ]
        );
    }

    #[test]
    fn a_windows_host_gets_its_own_separator() {
        let git_dir = PathBuf::from(r"C:\src\repo\.git");
        let dirs = git_watch_dirs('\\', &git_dir, &git_dir, &[]);
        assert!(
            names(&dirs).contains(&r"C:\src\repo\.git\refs\heads".to_string()),
            "got {:?}",
            names(&dirs)
        );
    }

    #[test]
    fn too_many_branch_namespaces_are_dropped_rather_than_watched() {
        let git_dir = PathBuf::from("/repo/.git");
        let many: Vec<String> = (0..MAX_WATCHED_REF_DIRS + 1)
            .map(|i| format!("ns{i}"))
            .collect();
        let dirs = git_watch_dirs('/', &git_dir, &git_dir, &many);
        assert_eq!(dirs.len(), 4, "packed-refs and the git dir still cover it");

        let just_enough = &many[..MAX_WATCHED_REF_DIRS];
        let dirs = git_watch_dirs('/', &git_dir, &git_dir, just_enough);
        assert_eq!(dirs.len(), 4 + MAX_WATCHED_REF_DIRS);
    }

    // -- debounce ---------------------------------------------------------

    fn at(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    #[test]
    fn a_burst_inside_the_window_costs_one_probe() {
        let t0 = Instant::now();
        let mut debounce = Debounce::default();

        assert_eq!(debounce.note(t0), Some(1), "the first event opens a burst");
        for ms in [40, 90, 150] {
            assert_eq!(
                debounce.note(at(t0, ms)),
                None,
                "an open burst already has a timer"
            );
        }
        assert_eq!(
            debounce.poll(at(t0, 300)),
            DebounceStep::Wait(Duration::from_millis(100)),
            "the last event at 150ms moved the deadline out to 400ms"
        );
        assert_eq!(debounce.poll(at(t0, 400)), DebounceStep::Fire);
        assert_eq!(
            debounce.poll(at(t0, 400)),
            DebounceStep::Idle,
            "firing closes the burst; four events cost one probe"
        );
    }

    #[test]
    fn an_unending_burst_still_fires_at_the_ceiling() {
        let t0 = Instant::now();
        let mut debounce = Debounce::default();
        debounce.note(t0);

        // A checkout writing `.git` every 100ms would push the quiet deadline
        // out forever; the ceiling is what stops the panel looking hung.
        let mut fired = None;
        for ms in (100..=2000).step_by(100) {
            debounce.note(at(t0, ms));
            if debounce.poll(at(t0, ms)) == DebounceStep::Fire {
                fired = Some(ms);
                break;
            }
        }
        assert_eq!(fired, Some(1000), "GIT_WATCH_MAX_DELAY is the backstop");
    }

    #[test]
    fn a_new_burst_gets_a_new_sequence_so_the_old_timer_stops() {
        let t0 = Instant::now();
        let mut debounce = Debounce::default();
        assert_eq!(debounce.note(t0), Some(1));
        assert_eq!(debounce.poll(at(t0, 250)), DebounceStep::Fire);
        assert_eq!(debounce.note(at(t0, 500)), Some(2));
        assert_eq!(debounce.seq(), 2);
    }

    #[test]
    fn a_write_and_the_watcher_event_it_causes_probe_once() {
        // `git add` writes `.git/index`, so the watcher fires a few
        // milliseconds after `run_git_op` has already said so itself. Both go
        // through the same bus, so both land in one window.
        let t0 = Instant::now();
        let mut data = ScmData::default();
        data.subscriptions().acquire(HostId::LOCAL, &root());

        assert_eq!(
            data.note_change(HostId::LOCAL, &root(), t0),
            Some(1),
            "the write announces itself"
        );
        assert_eq!(
            data.note_change(HostId::LOCAL, &root(), at(t0, 30)),
            None,
            "the watcher event it provoked joins the same burst"
        );

        let mut fires = 0;
        for ms in [100, 200, 280, 400, 800] {
            if data.poll_debounce(HostId::LOCAL, &root(), at(t0, ms)) == DebounceStep::Fire {
                fires += 1;
            }
        }
        assert_eq!(fires, 1, "one probe, not two — and no oscillation after it");
    }

    // -- subscriptions ----------------------------------------------------

    #[test]
    fn the_last_holder_out_moves_the_generation() {
        let mut subs = GitSubscriptions::default();
        let gen0 = subs.acquire(HostId::LOCAL, &root());
        assert_eq!(subs.acquire(HostId::LOCAL, &root()), gen0, "still the same");
        assert_eq!(subs.count(HostId::LOCAL, &root()), 2);

        subs.release(HostId::LOCAL, &root());
        assert_eq!(
            subs.generation(HostId::LOCAL, &root()),
            gen0,
            "one holder left, so anything in flight is still wanted"
        );
        subs.release(HostId::LOCAL, &root());
        assert!(!subs.is_subscribed(HostId::LOCAL, &root()));
        assert_ne!(
            subs.generation(HostId::LOCAL, &root()),
            gen0,
            "a result landing now belongs to a panel nobody is looking at"
        );

        // …and taking a hold again does not resurrect the old generation.
        assert_ne!(subs.acquire(HostId::LOCAL, &root()), gen0);
    }

    #[test]
    fn releasing_a_repository_nobody_holds_does_not_underflow() {
        let mut subs = GitSubscriptions::default();
        subs.release(HostId::LOCAL, &root());
        assert_eq!(subs.count(HostId::LOCAL, &root()), 0);
    }

    #[test]
    fn declaring_the_same_target_twice_does_not_count_twice() {
        let mut subs = GitSubscriptions::default();
        let here = Some((HostId::LOCAL, root()));
        for _ in 0..30 {
            assert_eq!(subs.declare(ScmWatcher::Panel, here.clone()), None);
        }
        assert_eq!(
            subs.count(HostId::LOCAL, &root()),
            1,
            "render runs every frame; a hold is a statement, not an event"
        );

        subs.declare(ScmWatcher::FileTree, here.clone());
        assert_eq!(subs.count(HostId::LOCAL, &root()), 2);
        assert_eq!(
            subs.declare(ScmWatcher::Panel, None),
            None,
            "the file tree is still looking"
        );
        assert_eq!(
            subs.declare(ScmWatcher::FileTree, None),
            Some((HostId::LOCAL, root())),
            "the last one out reports the repository to tear down"
        );
    }

    #[test]
    fn moving_a_subscriber_to_another_repository_releases_the_first() {
        let mut subs = GitSubscriptions::default();
        let other = PathBuf::from("/elsewhere");
        subs.declare(ScmWatcher::Panel, Some((HostId::LOCAL, root())));
        assert_eq!(
            subs.declare(ScmWatcher::Panel, Some((HostId::LOCAL, other.clone()))),
            Some((HostId::LOCAL, root()))
        );
        assert_eq!(subs.count(HostId::LOCAL, &root()), 0);
        assert_eq!(subs.count(HostId::LOCAL, &other), 1);
    }

    #[test]
    fn an_unsubscribed_repository_costs_nothing_but_an_epoch() {
        // The invalidation still has to be recorded — the next subscriber must
        // find it stale — but nothing may be scheduled for an empty room.
        let mut data = ScmData::default();
        data.bump(HostId::LOCAL, &root());
        assert!(data.is_stale(HostId::LOCAL, &root()));
        assert!(!data.is_subscribed(HostId::LOCAL, &root()));
        assert_eq!(data.burst_seq(HostId::LOCAL, &root()), 0);
        assert_eq!(
            data.poll_debounce(HostId::LOCAL, &root(), Instant::now()),
            DebounceStep::Idle
        );
    }

    #[test]
    fn only_repositories_without_a_watch_are_asked_for_one() {
        let mut data = ScmData::default();
        data.subscriptions().acquire(HostId::LOCAL, &root());
        assert_eq!(data.unwatched(), vec![(HostId::LOCAL, root())]);

        assert!(data.begin_watch_open(HostId::LOCAL, &root()));
        assert!(
            !data.begin_watch_open(HostId::LOCAL, &root()),
            "the frame after the one that asked must not ask again"
        );
        assert!(data.unwatched().is_empty());

        // A watch that failed to open leaves nothing behind, so the next
        // frame is free to try again.
        data.finish_watch_open(HostId::LOCAL, &root(), None);
        assert_eq!(data.unwatched(), vec![(HostId::LOCAL, root())]);
    }

    #[test]
    fn dropping_a_watch_forgets_the_burst_with_it() {
        let mut data = ScmData::default();
        data.subscriptions().acquire(HostId::LOCAL, &root());
        data.note_change(HostId::LOCAL, &root(), Instant::now());
        data.subscriptions().release(HostId::LOCAL, &root());
        data.drop_watch(HostId::LOCAL, &root());
        assert_eq!(
            data.poll_debounce(HostId::LOCAL, &root(), Instant::now()),
            DebounceStep::Idle,
            "a timer still asleep on that burst has to wake up to nothing"
        );
    }

    // -- network slots ----------------------------------------------------

    #[test]
    fn a_network_slot_comes_back_however_the_operation_ends() {
        let mut data = ScmData::default();
        let first = data.take_network_slot(HostId::LOCAL, &root()).unwrap();
        let second = data.take_network_slot(HostId::LOCAL, &root()).unwrap();
        assert_eq!(data.network_slots(HostId::LOCAL, &root()), 2);
        assert!(
            data.take_network_slot(HostId::LOCAL, &root()).is_none(),
            "the third push has to wait, or keepalive misses its deadline"
        );

        // The failure path is the one that used to leak: the guard travels
        // with the work, so it is released whether or not anything lands.
        drop(first);
        assert_eq!(data.network_slots(HostId::LOCAL, &root()), 1);
        assert!(data.take_network_slot(HostId::LOCAL, &root()).is_some());
        drop(second);
        assert_eq!(data.network_slots(HostId::LOCAL, &root()), 0);
    }

    #[test]
    fn network_slots_are_counted_per_repository_and_per_host() {
        let mut data = ScmData::default();
        let other = HostId::from_connection_key("ssh-direct:me@box:22");
        let _a = data.take_network_slot(HostId::LOCAL, &root()).unwrap();
        let _b = data.take_network_slot(HostId::LOCAL, &root()).unwrap();
        assert!(data.take_network_slot(HostId::LOCAL, &root()).is_none());
        assert!(
            data.take_network_slot(other, &root()).is_some(),
            "a busy laptop must not stop a push on a remote box"
        );
        assert!(
            data.take_network_slot(HostId::LOCAL, Path::new("/other"))
                .is_some()
        );
    }

    // -- disconnect -------------------------------------------------------

    #[test]
    fn clearing_a_host_takes_the_watch_and_the_subscription_with_it() {
        let mut data = ScmData::default();
        let gone = HostId::from_connection_key("ssh-direct:me@box:22");
        data.subscriptions().acquire(gone, &root());
        data.begin_watch_open(gone, &root());
        data.note_change(gone, &root(), Instant::now());
        data.status.insert(gone, root(), Arc::new(fake_status()));
        data.read_at.insert(gone, root(), 7);
        data.subscriptions().acquire(HostId::LOCAL, &root());

        let wipe = data.wipe;
        data.clear_host(gone);

        assert!(data.status_for(gone, &root()).is_none());
        assert!(!data.is_subscribed(gone, &root()));
        assert!(data.is_stale(gone, &root()));
        assert_eq!(
            data.poll_debounce(gone, &root(), Instant::now()),
            DebounceStep::Idle
        );
        assert_ne!(data.wipe, wipe, "a probe in flight across the drop is void");
        assert!(
            data.is_subscribed(HostId::LOCAL, &root()),
            "the same path on this machine is a different repository"
        );
    }

    #[test]
    fn the_tracked_host_list_is_what_the_sweep_looks_at() {
        let mut data = ScmData::default();
        let remote = HostId::from_connection_key("ssh-direct:me@box:22");
        assert!(data.hosts().is_empty(), "an idle app sweeps nothing");
        data.bump(HostId::LOCAL, &root());
        data.bump(remote, &root());
        data.bump(remote, Path::new("/other"));
        let mut hosts = data.hosts();
        hosts.sort_unstable();
        let mut want = vec![HostId::LOCAL, remote];
        want.sort_unstable();
        assert_eq!(hosts, want, "each host once, however many repositories");
    }

    fn fake_status() -> WorkingTreeStatus {
        use crate::core::git::status::HeadState;
        WorkingTreeStatus {
            root: root(),
            home: root(),
            head: HeadState::Detached {
                oid: "0".repeat(40),
            },
            upstream: None,
            ahead_behind: None,
            entries: Vec::new(),
            total_entries: 0,
            truncated: false,
            stash_count: 0,
            operation: None,
            prefilled_message: None,
        }
    }

    // -- what a probe leaves behind ---------------------------------------

    /// Landing a result, with the bookkeeping `scm_refresh` does around it.
    fn land(data: &mut ScmData, root: &Path, status: Option<WorkingTreeStatus>) {
        let at = data.epoch(HostId::LOCAL, root);
        match status {
            Some(status) => {
                data.status
                    .insert(HostId::LOCAL, root.to_path_buf(), Arc::new(status));
            }
            None => {
                data.status.remove(HostId::LOCAL, root);
            }
        }
        data.read_at.insert(HostId::LOCAL, root.to_path_buf(), at);
    }

    #[test]
    fn a_directory_that_is_not_a_repository_is_only_asked_once() {
        let mut data = ScmData::default();
        let plain = Path::new("/tmp/notes");
        assert_eq!(data.known_status(HostId::LOCAL, plain), None);

        land(&mut data, plain, None);
        assert_eq!(
            data.known_status(HostId::LOCAL, plain),
            Some(None),
            "'there is no repository here' is an answer, not a missing one"
        );
        assert!(
            !data.is_stale(HostId::LOCAL, plain),
            "otherwise every frame spawns another rev-parse for a plain directory"
        );

        data.bump(HostId::LOCAL, plain);
        assert!(
            data.is_stale(HostId::LOCAL, plain),
            "…until something moves"
        );
    }

    #[test]
    fn a_repository_that_goes_away_stops_being_reported_as_one() {
        let mut data = ScmData::default();
        land(&mut data, &root(), Some(fake_status()));
        assert!(
            data.known_status(HostId::LOCAL, &root())
                .flatten()
                .is_some()
        );

        data.bump(HostId::LOCAL, &root());
        land(&mut data, &root(), None);
        assert_eq!(
            data.known_status(HostId::LOCAL, &root()),
            Some(None),
            "the last good answer must not outlive the repository"
        );
    }

    #[test]
    fn an_app_with_nothing_open_does_not_touch_the_global() {
        let data = ScmData::default();
        assert!(data.is_quiet(), "no holders, no watches, nothing to sync");
    }

    // -- against a real repository ----------------------------------------

    fn run(host: &dyn Host, cwd: &Path, args: &[&str]) -> bool {
        let mut full = vec![
            "-c",
            "user.name=tty7",
            "-c",
            "user.email=test@tty7.invalid",
            "-c",
            "commit.gpgsign=false",
        ];
        full.extend_from_slice(args);
        host.git(cwd, &full).map(|o| o.success()).unwrap_or(false)
    }

    #[test]
    fn a_linked_worktree_resolves_to_both_of_its_real_directories() {
        // The layout rules are covered above; this is here because the two
        // directories come out of one `rev-parse`, and a repository is the
        // only thing that can say whether we asked it the right question.
        let host = tty7_core::host::local::LocalHost::new();
        let Ok(scratch) = tempfile::tempdir() else {
            return;
        };
        // `git rev-parse` reports real paths, and on macOS the temp directory
        // is reached through the `/var` → `/private/var` symlink.
        let base = std::fs::canonicalize(scratch.path()).unwrap();
        let repo = base.join("main");
        std::fs::create_dir(&repo).unwrap();
        if !run(&*host, &repo, &["init", "--quiet"]) {
            return; // no git on this machine
        }
        std::fs::write(repo.join("a.txt"), "one\n").unwrap();
        assert!(run(&*host, &repo, &["add", "-A"]));
        assert!(run(&*host, &repo, &["commit", "--quiet", "-m", "base"]));

        let plain = spelled(&scm_watch_dirs(&*host, &repo).expect("a repository is here"));
        assert_eq!(plain[0], one_spelling(&repo.join(".git")));
        assert!(plain.contains(&one_spelling(&repo.join(".git").join("refs").join("heads"))));

        let linked = base.join("wt");
        if !run(
            &*host,
            &repo,
            &["worktree", "add", "-q", "-b", "feat/x", "../wt"],
        ) {
            return; // git too old for worktrees
        }
        let dirs = spelled(&scm_watch_dirs(&*host, &linked).expect("the worktree is a repository"));
        assert!(
            dirs[0].starts_with(&one_spelling(&repo.join(".git").join("worktrees"))),
            "HEAD and index live in the worktree's own git dir, got {dirs:?}"
        );
        assert!(
            dirs.contains(&one_spelling(&repo.join(".git"))),
            "packed-refs lives in the common dir, and it is a different one, got {dirs:?}"
        );
        assert!(
            dirs.contains(&one_spelling(
                &repo.join(".git").join("refs").join("heads").join("feat")
            )),
            "`feat/x` needs its namespace listed: the watch does not recurse, got {dirs:?}"
        );
    }

    /// A path in the one spelling both halves of that test can agree on.
    ///
    /// The two halves do not naturally agree. `scm_watch_dirs` passes on what
    /// `git rev-parse` answered, and git writes forward slashes and no
    /// extended-length prefix even on Windows; the expected side is built from
    /// `fs::canonicalize`, which on Windows returns `\\?\C:\…`. Both name the
    /// same directory and the Win32 file APIs take either, so the watcher is
    /// right to hand git's answer straight to `Host::watch` — it is only the
    /// comparison here that has to pick a spelling.
    fn one_spelling(p: &Path) -> String {
        let slashed = p.to_string_lossy().replace('\\', "/");
        match slashed.strip_prefix("//?/") {
            Some(bare) => bare.to_string(),
            None => slashed,
        }
    }

    fn spelled(paths: &[std::path::PathBuf]) -> Vec<String> {
        paths.iter().map(|p| one_spelling(p)).collect()
    }
}
