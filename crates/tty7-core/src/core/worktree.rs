//! Git-worktree support for the tab context menu's "New Worktree Tab": derive
//! the repo from a pane's cwd, propose an unused two-word name (editable in the
//! sheet, see `ui::worktree_prompt`), and run `git worktree add -b` under the
//! repository's own `.tty7/worktrees/` (kept out of `git status` by an
//! auto-written self-ignoring `.tty7/.gitignore`) — so a coding agent gets an
//! isolated checkout on its own branch, physically next to the code it forks.
//!
//! Every filesystem touch and every `git` invocation goes through the [`Host`]
//! the pane belongs to, so a worktree is created on the machine the code
//! actually lives on rather than always on this one. That also means **all of
//! it blocks** — on a remote host every call here is a round trip — so callers
//! run the whole module on the background executor (`ui::host_ops`), never
//! inline while building a menu.
//!
//! Path arithmetic is deliberately the host's too ([`Host::join`], never
//! `PathBuf::join`): a Windows client driving a Linux host would otherwise
//! build `/home/me\.tty7` and create a repository directory with a backslash in
//! its name.

use std::path::{Path, PathBuf};

use crate::host::Host;

/// Word pools for generated branch names (`quiet-otter`). Short, lowercase,
/// branch-safe; two pools of 24 give 576 combinations before the numeric
/// fallback in [`defaults`] kicks in.
const ADJECTIVES: [&str; 24] = [
    "quiet", "amber", "bold", "calm", "cedar", "coral", "dusky", "early", "fable", "gold", "hazel",
    "ivory", "jade", "keen", "lunar", "mossy", "noble", "ochre", "pale", "rapid", "sunny", "tidal",
    "vivid", "wild",
];
const NOUNS: [&str; 24] = [
    "otter", "heron", "lynx", "wren", "fox", "elk", "crane", "finch", "gecko", "ibis", "koala",
    "llama", "marten", "newt", "osprey", "puffin", "quail", "raven", "seal", "tern", "urchin",
    "vole", "walrus", "yak",
];

/// A freshly created worktree: where it lives and the branch checked out in it.
#[derive(Debug)]
pub struct NewWorktree {
    pub path: PathBuf,
    pub branch: String,
}

/// What to create, as confirmed (or edited) in the sheet: the checkout's
/// directory name under the managed root, the new branch's name, and the
/// commit-ish it starts from.
#[derive(Debug, Clone)]
pub struct WorktreeRequest {
    pub name: String,
    pub branch: String,
    pub base: String,
}

/// Pre-filled values for the sheet: an unused two-word candidate (offered as
/// both directory name and branch), the branch currently checked out (the
/// natural start point; `"HEAD"` when detached), and the directory the new
/// checkout would land in, for the live path preview.
#[derive(Debug, Clone)]
pub struct WorktreeDefaults {
    pub name: String,
    pub base: String,
    pub dir: PathBuf,
}

/// `<root>/.tty7/worktrees` — where this repository's managed checkouts live.
/// Built with [`Host::join`] rather than [`PathBuf::join`] so the separator is
/// the *host's*, not the client's.
fn managed_root(host: &dyn Host, main_root: &Path) -> PathBuf {
    host.join(&host.join(main_root, ".tty7"), "worktrees")
}

/// Run `git -C <dir> <args>` on `host`, returning trimmed stdout on success and
/// trimmed stderr as the error otherwise.
///
/// The shape is unchanged from when this module ran `git` itself; what changed
/// is that the invocation is now the one every git read in tty7 shares
/// (`core::git::git_output`): `GIT_OPTIONAL_LOCKS=0`, stdin nulled, ambient
/// `GIT_DIR`/`GIT_WORK_TREE` removed. `GIT_OPTIONAL_LOCKS` only suppresses
/// *optional* sub-operations (git's own words) — the locks `worktree add` and
/// `branch -d` need to do their job are not optional and are still taken.
///
/// The three outcomes stay distinct: `Err` from the host means git never ran
/// (missing binary, vanished cwd, dead connection) and carries the same
/// `failed to run git:` prefix this module has always produced, while a git
/// that ran and failed still reports its own stderr.
fn git(host: &dyn Host, dir: &Path, args: &[&str]) -> Result<String, String> {
    match host.git(dir, args) {
        Ok(out) if out.success() => Ok(out.stdout_trimmed()),
        Ok(out) => Err(out.stderr_trimmed()),
        Err(e) => Err(format!("failed to run git: {e}")),
    }
}

/// Whether `name` already exists as a local branch in the repo at `repo_root`.
/// A failed probe (`--verify --quiet` exits non-zero) means it's free.
fn branch_exists(host: &dyn Host, repo_root: &Path, name: &str) -> bool {
    git(
        host,
        repo_root,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{name}"),
        ],
    )
    .is_ok()
}

/// A tiny xorshift over a time+pid seed — enough randomness to spread branch
/// names without pulling in a `rand` dependency.
fn seed() -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9E37_79B9_7F4A_7C15);
    nanos ^ ((std::process::id() as u64) << 32) | 1
}

fn next(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

/// One `adjective-noun` candidate from the pools.
fn candidate(state: &mut u64) -> String {
    let a = ADJECTIVES[(next(state) % ADJECTIVES.len() as u64) as usize];
    let n = NOUNS[(next(state) % NOUNS.len() as u64) as usize];
    format!("{a}-{n}")
}

/// A tty7-managed worktree a closing tab sat in, resolved for the
/// close-time cleanup offer: where it is, its branch, the repository it
/// belongs to, and whether it holds uncommitted changes.
#[derive(Debug, Clone)]
pub struct ManagedWorktree {
    pub path: PathBuf,
    pub branch: String,
    pub main_root: PathBuf,
    pub dirty: bool,
}

/// Resolve `cwd` to the tty7-managed worktree containing it, or `None` when it
/// sits anywhere else. Only checkouts under the main repository's
/// `.tty7/worktrees/` count — a user's own linked worktrees are never offered
/// for removal. Blocking (spawns `git`).
pub fn managed(host: &dyn Host, cwd: &Path) -> Option<ManagedWorktree> {
    // Canonicalize before the component test: git reports resolved physical
    // paths (`/private/var/…` on macOS), while `cwd` may arrive through
    // symlinks — a textual comparison would then never match. The `.tty7/
    // worktrees` ancestor check is a cheap pure-textual pre-filter, so the
    // common case (every ordinary tab close) never spawns git.
    let cwd = host.canonicalize(cwd).ok()?;
    let suffix = host.join(Path::new(".tty7"), "worktrees");
    if !cwd.ancestors().any(|a| a.ends_with(&suffix)) {
        return None;
    }
    let path = PathBuf::from(git(host, &cwd, &["rev-parse", "--show-toplevel"]).ok()?);
    let main_root = git(
        host,
        &path,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .ok()
    .map(PathBuf::from)?
    .parent()?
    .to_path_buf();
    // The checkout must really sit in *this* repository's managed directory —
    // both paths come from git, so they compare on equal (physical) footing.
    if !path.starts_with(managed_root(host, &main_root)) {
        return None;
    }
    let branch = git(host, &path, &["rev-parse", "--abbrev-ref", "HEAD"]).ok()?;
    let dirty = !git(host, &path, &["status", "--porcelain"])
        .ok()?
        .is_empty();
    Some(ManagedWorktree {
        path,
        branch,
        main_root,
        dirty,
    })
}

/// Whether any of `cwds` still lives inside the worktree at `path` — removing
/// the checkout then would pull the directory out from under a live shell (new
/// tabs inherit the current cwd, so two tabs sharing one worktree is common).
/// Both sides are canonicalized before the ancestor test — cwds may arrive
/// through symlinks, and on Windows canonicalize adds a `\\?\` verbatim prefix
/// that git-reported paths lack, so comparing raw would never match. A
/// vanished path never counts as occupying.
pub fn occupied(host: &dyn Host, path: &Path, cwds: &[PathBuf]) -> bool {
    let Ok(path) = host.canonicalize(path) else {
        return false;
    };
    cwds.iter()
        .any(|c| host.canonicalize(c).is_ok_and(|c| c.starts_with(&path)))
}

/// Remove a managed worktree (`git worktree remove`, `--force` to discard
/// uncommitted changes), then best-effort delete its branch with `-d` — so a
/// branch carrying unmerged commits survives the cleanup.
pub fn remove(host: &dyn Host, wt: &ManagedWorktree, force: bool) -> Result<(), String> {
    let path = wt.path.to_str().ok_or("worktree path is not valid UTF-8")?;
    let mut args = vec!["worktree", "remove"];
    if force {
        args.push("--force");
    }
    args.push(path);
    git(host, &wt.main_root, &args)?;
    let _ = git(host, &wt.main_root, &["branch", "-d", &wt.branch]);
    Ok(())
}

/// Locate the repository containing `cwd` and the directory its managed
/// worktrees live in: `(repo_root, <main-root>/.tty7/worktrees)`. Anchored on
/// the *main* repository even when `cwd` is itself inside a linked worktree
/// (a worktree tab spawning another worktree), so checkouts never nest. The
/// common git-dir is `<main>/.git`, whose parent is the main root.
fn repo_dir(host: &dyn Host, cwd: &Path) -> Result<(PathBuf, PathBuf), String> {
    let repo_root = git(host, cwd, &["rev-parse", "--show-toplevel"])
        .map_err(|_| "not inside a git repository".to_string())?;
    let repo_root = PathBuf::from(repo_root);
    let main_root = git(
        host,
        cwd,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .ok()
    .map(PathBuf::from)
    .and_then(|d| d.parent().map(Path::to_path_buf))
    .unwrap_or_else(|| repo_root.clone());
    let dir = managed_root(host, &main_root);
    Ok((repo_root, dir))
}

/// Compute the sheet's pre-filled values: a generated `adjective-noun` name
/// (retried until both the branch and the directory are unused, with a
/// numeric-suffix fallback so a saturated pool still terminates) and the
/// currently checked-out branch as the start point.
pub fn defaults(host: &dyn Host, cwd: &Path) -> Result<WorktreeDefaults, String> {
    let (repo_root, dir) = repo_dir(host, cwd)?;

    let mut state = seed();
    let mut name = candidate(&mut state);
    for attempt in 0..64 {
        // Both the ref and the directory must be free — a stale directory from a
        // hand-removed worktree would make `git worktree add` fail either way.
        if !branch_exists(host, &repo_root, &name) && !host.exists(&host.join(&dir, &name)) {
            break;
        }
        name = if attempt < 32 {
            candidate(&mut state)
        } else {
            format!("{}-{}", candidate(&mut state), next(&mut state) % 1000)
        };
    }

    // Detached HEAD (or an unborn branch) has no abbrev-ref; start from HEAD.
    let base = git(host, &repo_root, &["rev-parse", "--abbrev-ref", "HEAD"])
        .unwrap_or_else(|_| "HEAD".to_string());
    Ok(WorktreeDefaults { name, base, dir })
}

/// Create the requested worktree for the repository containing `cwd`, at
/// `<main-root>/.tty7/worktrees/<name>`, on new branch `branch` starting from
/// `base`. Branch and base validity is git's to judge; the directory name only
/// has to stay a single path component so it can't escape the managed root.
pub fn create(host: &dyn Host, cwd: &Path, req: &WorktreeRequest) -> Result<NewWorktree, String> {
    if req.name.is_empty() || req.name == "." || req.name == ".." || req.name.contains(['/', '\\'])
    {
        return Err(format!("invalid worktree name \"{}\"", req.name));
    }
    let (repo_root, dir) = repo_dir(host, cwd)?;
    host.create_dir(&dir, true)
        .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    // A `*` gitignore inside `.tty7/` keeps the whole tree (checkouts included,
    // the ignore file itself too) out of the repository's `git status`, without
    // ever editing the repo's own .gitignore. Best-effort: a failed write only
    // costs status noise, never the worktree.
    let ignore = host.join(
        dir.parent().expect(".tty7/worktrees has a parent"),
        ".gitignore",
    );
    if !host.exists(&ignore) {
        let _ = host.write_file(&ignore, b"*\n");
    }

    let path = host.join(&dir, &req.name);
    if host.exists(&path) {
        return Err(format!("{} already exists", path.display()));
    }
    git(
        host,
        &repo_root,
        &[
            "worktree",
            "add",
            "-b",
            &req.branch,
            path.to_str().ok_or("worktree path is not valid UTF-8")?,
            &req.base,
        ],
    )?;
    Ok(NewWorktree {
        path,
        branch: req.branch.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh scratch dir under the system temp location, unique per test —
    /// the same std-only pattern the config tests use (no tempfile dep).
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tty7-wt-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Strip Windows' `\\?\` verbatim prefix so `std::fs::canonicalize` output
    /// compares equal to the plain absolute paths git reports; a no-op on Unix.
    fn plain(p: &Path) -> PathBuf {
        let s = p.to_string_lossy();
        PathBuf::from(s.strip_prefix(r"\\?\").unwrap_or(&s).to_string())
    }

    fn sh(dir: &Path, args: &[&str]) {
        assert!(
            std::process::Command::new(args[0])
                .args(&args[1..])
                .current_dir(dir)
                .output()
                .unwrap()
                .status
                .success(),
            "command failed: {args:?}"
        );
    }

    /// A throwaway repo with one commit, so `worktree add` has a HEAD to branch
    /// from.
    fn temp_repo(name: &str) -> PathBuf {
        let dir = scratch(name);
        sh(&dir, &["git", "init", "-q"]);
        sh(&dir, &["git", "config", "user.email", "t@t"]);
        sh(&dir, &["git", "config", "user.name", "t"]);
        std::fs::write(dir.join("a.txt"), "a").unwrap();
        sh(&dir, &["git", "add", "."]);
        sh(&dir, &["git", "commit", "-q", "-m", "init"]);
        dir
    }

    /// The host every test drives: this machine. `LocalHost` is what the GUI
    /// hands these functions today, so testing against it tests the real path;
    /// a remote host is covered by the conformance suite instead.
    fn h() -> crate::host::SharedHost {
        crate::host::local::LocalHost::new()
    }

    /// The simplest sensible request: directory and branch share `name`,
    /// starting from HEAD — what the sheet submits when nothing is edited.
    fn req(name: &str) -> WorktreeRequest {
        WorktreeRequest {
            name: name.into(),
            branch: name.into(),
            base: "HEAD".into(),
        }
    }

    #[test]
    fn candidate_is_two_pool_words() {
        let mut state = seed();
        let name = candidate(&mut state);
        let (a, n) = name.split_once('-').unwrap();
        assert!(ADJECTIVES.contains(&a));
        assert!(NOUNS.contains(&n));
    }

    #[test]
    fn defaults_proposes_fresh_name_current_branch_and_target_dir() {
        let h = h();
        let repo = temp_repo("dflt");
        let d = defaults(&*h, &repo).unwrap();
        assert!(!branch_exists(&*h, &repo, &d.name));
        let head = git(&*h, &repo, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap();
        assert_eq!(d.base, head);
        // The target dir is the repo's own `.tty7/worktrees` (git reports the
        // canonical root: /var → /private/var on macOS).
        let canon = plain(&std::fs::canonicalize(&repo).unwrap());
        assert_eq!(plain(&d.dir), canon.join(".tty7").join("worktrees"));
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn create_makes_worktree_on_new_branch_inside_the_repo() {
        let h = h();
        let repo = temp_repo("repo");
        let wt = create(&*h, &repo, &req("quiet-otter")).unwrap();
        assert!(wt.path.join("a.txt").exists());
        assert!(branch_exists(&*h, &repo, &wt.branch));
        // The worktree lands under `<repo>/.tty7/worktrees/<name>`…
        let canon = plain(&std::fs::canonicalize(&repo).unwrap());
        assert_eq!(plain(&wt.path), canon.join(".tty7/worktrees/quiet-otter"));
        // …on the new branch…
        let head = git(&*h, &wt.path, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap();
        assert_eq!(head, wt.branch);
        // …and the auto-written `.tty7/.gitignore` keeps the main repo's
        // status clean despite the checkout living inside it.
        assert_eq!(
            std::fs::read_to_string(canon.join(".tty7/.gitignore")).unwrap(),
            "*\n"
        );
        assert_eq!(git(&*h, &repo, &["status", "--porcelain"]).unwrap(), "");
        // A second request colliding on the directory is refused up front.
        assert!(
            create(&*h, &repo, &req("quiet-otter"))
                .unwrap_err()
                .contains("already exists")
        );
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn create_honors_custom_branch_and_base() {
        let h = h();
        let repo = temp_repo("base");
        // A `stable` branch one commit behind the default branch's HEAD.
        sh(&repo, &["git", "branch", "stable"]);
        std::fs::write(repo.join("b.txt"), "b").unwrap();
        sh(&repo, &["git", "add", "."]);
        sh(&repo, &["git", "commit", "-q", "-m", "second"]);
        let wt = create(
            &*h,
            &repo,
            &WorktreeRequest {
                name: "my-dir".into(),
                branch: "feat/my-branch".into(),
                base: "stable".into(),
            },
        )
        .unwrap();
        // Directory and branch names diverge as requested…
        assert_eq!(wt.path.file_name().unwrap().to_str().unwrap(), "my-dir");
        let head = git(&*h, &wt.path, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap();
        assert_eq!(head, "feat/my-branch");
        // …and the checkout starts from `stable` (no b.txt yet).
        assert!(wt.path.join("a.txt").exists());
        assert!(!wt.path.join("b.txt").exists());
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn create_rejects_escaping_names() {
        let h = h();
        let repo = temp_repo("names");
        for bad in ["", ".", "..", "a/b", "a\\b"] {
            let mut r = req("x");
            r.name = bad.into();
            assert!(
                create(&*h, &repo, &r)
                    .unwrap_err()
                    .contains("invalid worktree name"),
                "{bad:?} should be rejected"
            );
        }
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn create_from_a_linked_worktree_lands_in_the_main_repo() {
        let h = h();
        let repo = temp_repo("nest");
        let first = create(&*h, &repo, &req("first-wt")).unwrap();
        // Spawn the second worktree from *inside* the first: it must land in
        // the main repo's `.tty7/worktrees`, not nest inside the first checkout.
        let second = create(&*h, &first.path, &req("second-wt")).unwrap();
        assert_eq!(second.path.parent().unwrap(), first.path.parent().unwrap());
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn managed_resolves_managed_checkouts_and_remove_cleans_up() {
        let h = h();
        let repo = temp_repo("mg");
        let wt = create(&*h, &repo, &req("mg-wt")).unwrap();
        // The repo root itself is never "managed"…
        assert!(managed(&*h, &repo).is_none());
        // …nor is a linked worktree the user made outside `.tty7/worktrees`.
        let own = scratch("mg-own");
        let _ = std::fs::remove_dir_all(&own);
        sh(
            &repo,
            &[
                "git",
                "worktree",
                "add",
                "-b",
                "own-branch",
                own.to_str().unwrap(),
            ],
        );
        assert!(managed(&*h, &own).is_none());
        // Any path inside the managed checkout resolves to it, initially clean.
        let sub = wt.path.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        let m = managed(&*h, &sub).unwrap();
        assert_eq!(m.branch, wt.branch);
        assert_eq!(m.path, wt.path);
        assert!(!m.dirty);
        // Uncommitted changes flip `dirty` and block a plain remove; --force
        // discards them. The branch (no unique commits) is deleted with it.
        std::fs::write(wt.path.join("b.txt"), "b").unwrap();
        let m = managed(&*h, &wt.path).unwrap();
        assert!(m.dirty);
        assert!(remove(&*h, &m, false).is_err());
        remove(&*h, &m, true).unwrap();
        assert!(!wt.path.exists());
        assert!(!branch_exists(&*h, &repo, &wt.branch));
        let _ = std::fs::remove_dir_all(&repo);
        let _ = std::fs::remove_dir_all(&own);
    }

    /// `Host::git` runs every invocation with `GIT_OPTIONAL_LOCKS=0`, which
    /// this module did *not* set when it spawned `git` itself. That variable is
    /// the one thing in the unified invocation with any claim to affect a
    /// *write*, so the whole create → list → remove → delete-branch path is
    /// exercised end to end under it rather than argued about.
    ///
    /// It is safe by git's own definition — "complete any requested operation
    /// without performing any optional sub-operations that require taking a
    /// lock" (`git(1)`, GIT_OPTIONAL_LOCKS). `worktree add` and `branch -d` are
    /// the *requested* operations, never optional sub-operations, and the locks
    /// they need are taken regardless. This test is what keeps that from being
    /// a reading of the manual: it fails if a git version ever decides
    /// otherwise.
    #[test]
    fn writes_survive_the_optional_locks_invariant() {
        let h = h();
        let repo = temp_repo("locks");
        // That the variable is *set* on every `Host::git` is asserted once, for
        // every host, by the conformance suite's `git_optional_locks_env_is_set`
        // — not re-derived here. What this test owns is the consequence.

        // add — the write the contract flags as the one to prove.
        let wt = create(&*h, &repo, &req("lock-wt")).unwrap();
        assert!(wt.path.join("a.txt").exists());

        // list — the new checkout is really registered, not merely on disk.
        let list = git(&*h, &repo, &["worktree", "list", "--porcelain"]).unwrap();
        assert!(
            list.lines()
                .any(|l| l.starts_with("branch ") && l.ends_with(&wt.branch)),
            "worktree list must show the new checkout: {list}"
        );

        // A commit inside the checkout: an index write, the operation whose
        // *optional* index refresh is what the variable suppresses.
        std::fs::write(wt.path.join("c.txt"), "c").unwrap();
        git(&*h, &wt.path, &["add", "."]).unwrap();
        git(
            &*h,
            &wt.path,
            &[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-qm",
                "c",
            ],
        )
        .unwrap();
        assert_eq!(git(&*h, &wt.path, &["status", "--porcelain"]).unwrap(), "");

        // remove + `branch -d`: the branch now carries a commit the main branch
        // does not, so the best-effort `-d` correctly declines and the branch
        // survives — the safety property `remove` documents.
        let m = managed(&*h, &wt.path).unwrap();
        remove(&*h, &m, false).unwrap();
        assert!(!wt.path.exists());
        assert!(branch_exists(&*h, &repo, &wt.branch));
        // …and a branch with nothing unique on it is deleted, so the `-d` is
        // genuinely running rather than always failing.
        let plain_wt = create(&*h, &repo, &req("lock-wt2")).unwrap();
        let m = managed(&*h, &plain_wt.path).unwrap();
        remove(&*h, &m, false).unwrap();
        assert!(!branch_exists(&*h, &repo, &plain_wt.branch));

        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn occupied_detects_live_cwds_inside_the_worktree() {
        let h = h();
        let repo = temp_repo("occ");
        let wt = create(&*h, &repo, &req("occ-wt")).unwrap();
        let inside = wt.path.join("deep");
        std::fs::create_dir_all(&inside).unwrap();
        assert!(occupied(&*h, &wt.path, &[repo.clone(), inside]));
        // Cwds elsewhere in the repo don't count…
        assert!(!occupied(&*h, &wt.path, std::slice::from_ref(&repo)));
        // …and neither does a cwd that no longer exists.
        assert!(!occupied(&*h, &wt.path, &[wt.path.join("gone")]));
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn create_outside_a_repo_errors() {
        let h = h();
        let plain = scratch("plain");
        let err = create(&*h, &plain, &req("x")).unwrap_err();
        assert_eq!(err, "not inside a git repository");
        assert_eq!(
            defaults(&*h, &plain).unwrap_err(),
            "not inside a git repository"
        );
        let _ = std::fs::remove_dir_all(&plain);
    }
}
