//! Git, the way every part of tty7 reads it: one shell-out per field, always
//! `git -C <cwd>`, always `GIT_OPTIONAL_LOCKS=0`.
//!
//! This is the shared bottom layer, not a feature: the sidebar's branch/diff
//! line, the diff overlay, and (from the remote-workspace work on) the server
//! side all go through the same [`git`] invocation, so a read tty7 performs can
//! never take `index.lock` and fight a real git command the user is running.
//! Deliberately shell-out simple — blocking, one process per question; callers
//! run it on a background thread so nothing UI-facing waits on a slow repo.
//!
//! The caching layer that fans one probe out to every pane in a repo is *not*
//! here: it is a gpui global, so it stays in the GUI crate
//! (`terminal::git_status::GitStatusCache`).

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::host::{Host, Output};

/// A repo's git snapshot: the branch it's on and how much the working tree has
/// changed against `HEAD`. `added`/`removed` sum the per-file line counts from
/// `git diff --numstat HEAD` (tracked staged + unstaged changes); binary files
/// and untracked files don't contribute a line count.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct GitStatus {
    /// The branch name (`main`, `feat/x`), or a short commit sha when the HEAD
    /// is detached. Never empty.
    pub branch: String,
    /// Lines added across the working tree vs `HEAD`.
    pub added: u32,
    /// Lines removed across the working tree vs `HEAD`.
    pub removed: u32,
}

/// One raw probe result, before it's folded into the cache: which work tree
/// `cwd` belongs to, plus the fields probed there. `counts` is `None` when the
/// `git diff` invocation itself failed (e.g. it raced a concurrent git write) —
/// distinct from a clean tree's `Some((0, 0))`, so the cache can keep the
/// previous numbers instead of pretending the tree went clean.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct RepoSnapshot {
    /// The work tree root (`git rev-parse --show-toplevel`) — the cache key
    /// every pane inside this work tree shares. For a linked worktree this is
    /// the worktree's own directory, not the main checkout's.
    pub root: PathBuf,
    /// The *repository* the work tree belongs to: the main checkout's root
    /// when `root` is a linked worktree, otherwise `root` itself. The
    /// sidebar's grouping key — every worktree of one repo shares it, while
    /// branch/diff state stays per work tree under `root`.
    pub home: PathBuf,
    pub branch: String,
    pub counts: Option<(u32, u32)>,
}

/// Probe the git snapshot for `cwd` on `host`, or `None` when it isn't inside a
/// git work tree (or the path is gone, or the host can't be reached).
/// Blocking — the GUI calls it through `ui::host_ops`, never on the UI thread.
///
/// Three invocations, and deliberately not fewer. The first `rev-parse` answers
/// every *path* question at once: the work-tree root (which doubles as the "is
/// this a git repo" gate — it fails outside a work tree) plus the
/// git-dir/common-dir pair that tells a linked worktree from a main checkout.
/// Asking those separately cost two process spawns per probe, which mattered
/// once probes stopped being rare: they now also fire on window activation and
/// on an agent's tool calls, across every pane. The branch cannot join them —
/// `symbolic-ref` is what names a branch before the first commit exists, and
/// folding `--abbrev-ref HEAD` into the `rev-parse` would make the whole
/// invocation fail on an unborn branch and lose the paths with it.
///
/// On a remote host each of the three is a round trip; the throttle and the
/// in-flight dedup in the GUI's `GitStatusCache` are what keep that from being
/// three per pane per trigger.
pub fn probe(host: &dyn Host, cwd: &Path) -> Option<RepoSnapshot> {
    // No `exists` pre-check: a vanished cwd already fails `Host::git` with
    // `NotFound`, which lands as `None` here — the same answer, one round trip
    // cheaper.
    let paths = git(
        host,
        cwd,
        &[
            "rev-parse",
            "--path-format=absolute",
            "--show-toplevel",
            "--git-dir",
            "--git-common-dir",
        ],
    )?;
    let mut lines = paths.lines().map(|l| l.trim_end_matches(['\n', '\r']));
    let root = PathBuf::from(lines.next()?);
    // A git old enough to reject `--path-format` fails the whole invocation
    // above, so reaching here means the two dirs are present — but degrade to
    // "main checkout" rather than trusting that, same as the old code did.
    let home = repo_home(&root, lines.next(), lines.next());
    let branch = branch_name(host, cwd)?;
    Some(RepoSnapshot {
        home,
        root,
        branch,
        counts: diff_numstat(host, cwd),
    })
}

/// The repository "home" every checkout of one repo shares, from the work-tree
/// `root` and the `--git-dir` / `--git-common-dir` pair: for a linked worktree
/// (its git dir differs from the common git dir) the main work tree's root —
/// the parent of `<main>/.git`; for the main checkout itself, a submodule, or
/// any failure to tell, the work-tree root unchanged. A bare common dir (no
/// trailing `.git` component, the bare-repo-plus-worktrees layout) anchors on
/// the bare directory itself — still one shared key.
fn repo_home(root: &Path, git_dir: Option<&str>, common_dir: Option<&str>) -> PathBuf {
    let (Some(git_dir), Some(common)) = (git_dir, common_dir) else {
        return root.to_path_buf();
    };
    if git_dir == common {
        return root.to_path_buf();
    }
    let common = Path::new(common);
    match (common.file_name(), common.parent()) {
        (Some(name), Some(parent)) if name == ".git" => parent.to_path_buf(),
        _ => common.to_path_buf(),
    }
}

/// The current branch name, or a short sha for a detached HEAD. Shared with
/// `terminal::git_diff` in the GUI crate, which fronts its overlay with the
/// same branch label the sidebar row shows.
pub fn branch_name(host: &dyn Host, cwd: &Path) -> Option<String> {
    // On a branch — even before the first commit — `symbolic-ref` names it.
    if let Some(out) = git(host, cwd, &["symbolic-ref", "--quiet", "--short", "HEAD"]) {
        let name = out.trim();
        if !name.is_empty() {
            return Some(name.to_string());
        }
    }
    // Detached HEAD (or a rebase/bisect): fall back to the short commit sha.
    let sha = git(host, cwd, &["rev-parse", "--short", "HEAD"])?;
    let sha = sha.trim();
    (!sha.is_empty()).then(|| sha.to_string())
}

/// Sum added/removed lines across the working tree vs `HEAD` from
/// `git diff --numstat HEAD`. Binary files (`-\t-`) contribute nothing.
/// `None` when the invocation itself failed — the caller keeps old counts.
fn diff_numstat(host: &dyn Host, cwd: &Path) -> Option<(u32, u32)> {
    let out = git(host, cwd, &["diff", "--numstat", "HEAD"])?;
    let mut added = 0u32;
    let mut removed = 0u32;
    for line in out.lines() {
        let mut fields = line.split('\t');
        if let Some(n) = fields.next().and_then(|s| s.parse::<u32>().ok()) {
            added += n;
        }
        if let Some(n) = fields.next().and_then(|s| s.parse::<u32>().ok()) {
            removed += n;
        }
    }
    Some((added, removed))
}

/// Run `git -C <cwd> <args>` on `host` and return stdout on success, `None` on
/// a non-zero exit or a git that never ran.
///
/// The projection of [`git_output`] the snapshot readers want: they treat "git
/// said no" and "git could not be asked" identically — a failed probe leaves
/// the previous snapshot standing rather than blanking the branch line — where
/// callers like `core::worktree` need the two kept apart, and the stderr with
/// them. Non-UTF-8 stdout is `None`: there is nothing sensible to parse out of
/// it.
///
/// Which machine's `git` runs is the host's business; the *invariants* of the
/// invocation are [`git_output`]'s, and every implementation of
/// [`Host::git`] owes them.
pub fn git(host: &dyn Host, cwd: &Path, args: &[&str]) -> Option<String> {
    let out = host.git(cwd, args).ok()?;
    if !out.success() {
        return None;
    }
    String::from_utf8(out.stdout).ok()
}

/// The full result of `git -C <cwd> <args>` — exit code, stdout *and* stderr —
/// under the invariants every git invocation in tty7 shares.
///
/// This is the bottom layer [`git`] is a projection of, and the one
/// [`crate::host::Host::git`] exposes: a `Host` has to answer for a remote
/// machine's git too, where "non-zero exit" and "git never ran" are genuinely
/// different outcomes and the caller needs stderr to say which. `Ok` means the
/// process ran (the exit code is in [`Output::status`]); `Err` means it could
/// not be run at all.
///
/// The invariants, all of them load-bearing:
///
/// - **`-C <cwd>`**, never `Command::current_dir` — the working directory of
///   this process is not a thing a GUI with many panes can meaningfully set.
/// - **`GIT_OPTIONAL_LOCKS=0`**, so a background read can never take
///   `index.lock` and lose a race against a real git command the user is
///   running. It only suppresses *optional* locks; writes still lock normally.
/// - **stdin nulled**, so a misconfigured credential helper or a prompt-happy
///   subcommand fails immediately instead of hanging a background thread
///   forever on a terminal nobody is attached to.
/// - **`GIT_DIR` / `GIT_WORK_TREE` removed**, so a tty7 launched from inside a
///   git hook (or any shell that exported them) can't have that ambient
///   repository silently override the `-C` we just passed.
/// - **`hide_console` on Windows**, so probing a repo doesn't flash a console
///   window.
///
/// A `cwd` that doesn't exist is [`io::ErrorKind::NotFound`] rather than git's
/// own exit 128: "the directory is gone" is not a question about the repository,
/// and callers (and the remote `Host` contract) distinguish the two.
pub fn git_output(cwd: &Path, args: &[&str]) -> io::Result<Output> {
    if !cwd.exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("git cwd does not exist: {}", cwd.display()),
        ));
    }
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(cwd)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let out = crate::core::proc::hide_console(&mut cmd).output()?;
    Ok(Output {
        status: out.status.code(),
        stdout: out.stdout,
        stderr: out.stderr,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The host these tests probe through: this machine, which is what the GUI
    /// hands `probe` for a local pane.
    fn h() -> crate::host::SharedHost {
        crate::host::local::LocalHost::new()
    }

    /// A tmp path that is not a git repo yields no snapshot (and never panics).
    #[test]
    fn non_repo_is_none() {
        let dir = std::env::temp_dir().join("tty7-git-status-not-a-repo-xyz");
        let _ = std::fs::create_dir_all(&dir);
        assert_eq!(probe(&*h(), &dir), None);
    }

    /// A path that doesn't exist is `None`, not a panic.
    #[test]
    fn missing_path_is_none() {
        assert_eq!(probe(&*h(), Path::new("/no/such/tty7/path/here")), None);
    }

    /// This repo (the crate root is inside the tty7 work tree) reports a branch
    /// and a root, exercising the real `git` probe end-to-end.
    #[test]
    fn own_repo_has_a_branch_and_root() {
        let here = env!("CARGO_MANIFEST_DIR");
        if let Some(snap) = probe(&*h(), Path::new(here)) {
            assert!(!snap.branch.is_empty());
            assert!(Path::new(here).starts_with(&snap.root));
        }
        // If the crate is built outside a work tree (e.g. a vendored tarball),
        // `None` is the correct answer and the assertions above are skipped.
    }
    /// The four shapes `repo_home` has to tell apart, straight from the
    /// `--git-dir` / `--git-common-dir` pair the merged `rev-parse` returns.
    #[test]
    fn repo_home_resolves_worktree_layouts() {
        let root = Path::new("/repo/.wt/feat");

        // A main checkout: the two dirs agree, so the work tree is its own home.
        assert_eq!(
            repo_home(Path::new("/repo"), Some("/repo/.git"), Some("/repo/.git")),
            PathBuf::from("/repo")
        );
        // A linked worktree: the common dir is the main checkout's `.git`, so
        // the home is that `.git`'s parent — the main work tree.
        assert_eq!(
            repo_home(root, Some("/repo/.git/worktrees/feat"), Some("/repo/.git")),
            PathBuf::from("/repo")
        );
        // A bare repo with worktrees hanging off it: no `.git` component to
        // strip, so the bare dir itself is the shared key.
        assert_eq!(
            repo_home(root, Some("/bare.git/worktrees/feat"), Some("/bare.git")),
            PathBuf::from("/bare.git")
        );
        // A git too old (or too odd) to answer both: degrade to the work tree
        // rather than guessing a grouping key.
        assert_eq!(
            repo_home(root, Some("/repo/.git"), None),
            root.to_path_buf()
        );
        assert_eq!(repo_home(root, None, None), root.to_path_buf());
    }
}
